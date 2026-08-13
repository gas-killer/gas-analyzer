# Gas-Killer Indexer Service

A persistent service that consumes new Ethereum blocks, runs the existing gas-analyzer on every qualifying transaction, groups results by project, and exposes them through a purpose-built web UI (`indexer-web`) for BD outreach.

## Architecture

```mermaid
flowchart TB
  classDef ext fill:#fef3c7,stroke:#92400e,color:#78350f
  classDef compose fill:#e0e7ff,stroke:#3730a3,color:#1e1b4b
  classDef api fill:#dcfce7,stroke:#166534,color:#14532d
  classDef store fill:#fce7f3,stroke:#9d174d,color:#500724
  classDef existing fill:#f3f4f6,stroke:#374151,color:#111827,stroke-dasharray: 4 2

  RPC[("Premium RPC<br/>Alchemy / QuickNode")]:::ext
  DEFILLAMA[("DefiLlama API<br/>/protocols")]:::ext
  PRICE[("Coingecko API")]:::ext
  ESCAN[("Etherscan v2 API<br/>getsourcecode")]:::ext

  subgraph compose["docker-compose stack on a single VM"]
    direction TB
    HEAD["head-tracker<br/>(1 replica)"]:::compose
    REDIS[("Redis<br/>analyzer:queue:pending<br/>analyzer:queue:dead<br/>indexer:state:last_head<br/>labeler:queue (ZSET)")]:::compose
    WORKERS["worker<br/>(N replicas)"]:::compose
    REFRESH["refresher<br/>4 loops:<br/>resolver / price / rollups / labeler"]:::compose
    PG[("Postgres<br/>analysis, projects,<br/>address_project,<br/>address_label_attempt,<br/>eth_prices, project_daily MV")]:::store
    WEB["indexer-web<br/>axum + askama + htmx<br/>(BD dashboards + admin health)"]:::compose
  end

  USER[("Browser<br/>(allowlisted user)")]:::ext

  subgraph worker_internals["Inside each worker — in-process"]
    direction TB
    API["indexer-api<br/>Analyzer trait<br/>EvmSketchAnalyzer<br/>(with_retry on RPC,<br/>60s timeout backstop)"]:::api
    LIM["indexer-rpc<br/>token bucket + retry +<br/>is_transient_rpc_error"]:::api
    RES["indexer-resolver<br/>address → project<br/>(overlay + DefiLlama +<br/>Etherscan NameDict)"]:::api
    STO["indexer-store<br/>Postgres writes"]:::api

    subgraph existing["gas-analyzer crates (UNTOUCHED)"]
      direction LR
      GA_RPC["gas-analyzer-rpc"]:::existing
      GA_EVM["gas-analyzer-evmsketch"]:::existing
      GA_CORE["gas-analyzer-core"]:::existing
    end

    API --> GA_RPC
    API --> GA_EVM
    GA_RPC --> GA_CORE
    GA_EVM --> GA_CORE
  end

  HEAD -- "RPUSH AnalyzeTxJob" --> REDIS
  REDIS -- "BLPOP claim" --> WORKERS
  WORKERS --> API
  WORKERS --> RES
  WORKERS --> STO

  HEAD -. "eth_blockNumber<br/>eth_getBlockByNumber (full)" .-> RPC
  GA_RPC -. "debug_traceTransaction<br/>eth_getTransactionReceipt" .-> RPC
  GA_EVM -. "eth_getStorageAt<br/>(bypasses limiter — see Known Limitations)" .-> RPC

  HEAD -. uses .-> LIM
  GA_RPC -. uses .-> LIM

  STO -- "INSERT analysis<br/>UPSERT projects<br/>UPSERT address_project" --> PG

  REFRESH -. "every 24h" .-> DEFILLAMA
  REFRESH -. "every 1h" .-> PRICE
  REFRESH -. "labeler consumer<br/>≤3 rps" .-> ESCAN
  REFRESH -- "UPSERT projects,<br/>UPSERT address_project,<br/>UPSERT eth_prices,<br/>UPSERT address_label_attempt,<br/>UPDATE analysis (relabel),<br/>REFRESH MV" --> PG
  REFRESH -- "labeler producer:<br/>ZADD by wei_saved<br/>ZPOPMAX one address" --> REDIS

  HEAD -- "SET last_head<br/>(60s TTL)" --> REDIS
  USER -- "form login<br/>signed cookie" --> WEB
  WEB -- "SQL aggregates" --> PG
  WEB -- "LLEN queue / dead<br/>GET last_head" --> REDIS
```

Solid arrows are data writes; dotted arrows are RPC / HTTP fetches. The thin green box is the modular API layer — it is the *only* code in the indexer that knows about gas-analyzer internals.

## Components

| Crate | Role |
|---|---|
| `indexer-api` | Thin modular API layer over gas-analyzer. `Analyzer` trait + `EvmSketchAnalyzer` impl. Exposes `analyze_tx(tx_hash) -> AnalysisReport`. **The only code that depends on `gas-analyzer-rpc` / `gas-analyzer-evmsketch`.** |
| `indexer-rpc` | Global token-bucket rate limiter (`governor`) + bounded-concurrency semaphore + jittered exponential-backoff retry helper. Per-method weight constants in `weights` module. |
| `indexer-store` | Postgres schema + sqlx migrations + insert/refresh helpers. Owns the `analysis` fact table, `projects` / `address_project` / `eth_prices` dimensions, and the `project_daily` materialized view. |
| `indexer-resolver` | Address → project mapping. DefiLlama HTTP client + curated YAML overlay. Snapshot held behind `arc-swap` for lock-free reads. Unmapped addresses fall back to `unknown:0xADDR` so no row is ever dropped. |
| `indexer-service` | Single binary (lib + bin), three clap subcommands: `head-tracker`, `worker`, `refresher`. The library surface re-exports `queue` and `state` for use by `indexer-web`. |
| `indexer-web` | Axum + Askama + htmx web UI. Reads aggregates from Postgres, reads queue/last-head counters from Redis, and RPUSH-es replay jobs back to the same Redis queue. Auth is a static YAML allowlist + HMAC-signed session cookie. |

The six new crates are added to the workspace's `members` and `default-members`. **No edits were made to `crates/{core,gas-estimator,rpc,evmsketch,anvil,cli,wasm}`.**

## Service roles

### `head-tracker` (1 replica)

Polls the chain head every `HEAD_POLL_MS` (default 4s). For each new block:
1. Fetches the block with full transactions (`eth_getBlockByNumber(n, true)`).
2. For each tx that calls a contract (skip create txs at this layer), pushes one `AnalyzeTxJob` onto Redis list `analyzer:queue:pending`.
3. **Backpressure**: if `LLEN(pending) > MAX_QUEUE_DEPTH`, pauses enqueueing until the queue drains. We never drop blocks — we just lag.

Live-only: starts at the current head and never looks backward. Reorgs are ignored (accepted ~1% inaccuracy at head).

### `worker` (N replicas)

Each worker:
1. `BLPOP`s a job from the queue.
2. Acquires `weights::ANALYZE_TX` (250) tokens from its local `RateLimiter`.
3. Calls `analyzer.analyze_tx(tx_hash)` (the lifted CLI orchestration: receipt → trace → preceding txs → measured estimate via EvmSketch, with heuristic fallback).
4. Resolves `report.to` to a project via the resolver.
5. Inserts the row into `analysis`, upserts the project, upserts the address-to-project mapping.
6. On `Skipped(reason)` (contract create / below threshold / reverted), drops silently.
7. On other errors, retries up to `WORKER_MAX_RETRIES` then dead-letters to `analyzer:queue:dead`.

### `refresher` (1 replica)

Four independent loops:
- Every 24h (`RESOLVER_REFRESH_SECS`): reload `overlay.yaml`, fetch DefiLlama `/protocols`, atomically swap the resolver snapshot, upsert all projects + the addresses DefiLlama exposes (currently ~1,000 Ethereum-mainnet entries — mostly governance tokens) into Postgres. Then run `relabel_unknowns()` to retroactively fix historical `analysis` rows whose synthetic `unknown:0xADDR` slug now resolves to a real project.
- Every 1h (`PRICE_REFRESH_SECS`): fetch ETH/USD from Coingecko, upsert into `eth_prices` keyed by today's UTC date.
- Every 1h (`ROLLUP_REFRESH_SECS`): `REFRESH MATERIALIZED VIEW CONCURRENTLY project_daily`.
- **Auto-labeler** (continuous producer + consumer):
  - **Producer** (every `LABELER_PRODUCER_INTERVAL_SECS`, default 1h): SQL query for top-N unknown `to_address` rows ranked by total `wei_saved`, `ZADD` to `labeler:queue` (Redis sorted set). Skips addresses whose last attempt was within `LABELER_RETRY_DAYS` and resulted in `unverified` or `no-match`; transient `error` results are re-enqueued every cycle.
  - **Consumer** (continuous, paced by `LABELER_MIN_DELAY_MS`, default 400ms ≈ 2.5 rps): `ZPOPMAX` highest-savings unknown, hit Etherscan `getsourcecode`, normalize the returned `ContractName` and look up in the curated `known_names.yaml` dictionary (with suffix-stripping fallbacks: `UniswapV2Router02 → uniswap-v2-router02 → uniswap-v2`). On match: upsert `address_project`, run `relabel_unknowns()`, record the attempt. On unverified / no-match / transport error: record the attempt with the appropriate `last_result` so the producer can decide retry policy.
  - Disabled at startup if `ETHERSCAN_API_KEY` is empty.

### `indexer-web` (1 replica)

Axum HTTP server on port 3000. Pages:

| Path | Auth | Purpose |
|---|---|---|
| `/login` | public | Username/password form. Verifies against `users.yaml`. Issues an HMAC-signed cookie on success. |
| `/` | required | Overview: tx-count totals (lifetime / 30d / 7d / 24h), sortable project leaderboard (txs, avg savings %), daily tx-count line chart. |
| `/projects/:slug` | required | Drill-down: per-project totals, daily series (90d), top contracts, top function selectors, recent transactions table. |
| `/unknowns` | required | Top unlabeled `to_address` rows by ETH saved last 30d. Sorted by ETH saved — feeds the auto-labeler's priority. |
| `/admin` | required | Service health (read-only). |
| `/admin/health` | required | htmx partial used by the admin page to refresh health counters every 5s. |
| `/healthz` | public | Plain `ok` response for compose healthchecks. |

**Auth.** Users are listed in a YAML allowlist mounted at `/etc/indexer/users.yaml`:
```yaml
- username: ramgos
  bcrypt_hash: "$2b$12$..."
```
Sessions are stateless — the cookie is `username|expires|HMAC-SHA256(secret, "username|expires")`, base64url-encoded. The HMAC key comes from `SESSION_SECRET` (≥32 bytes; generate with `openssl rand -hex 32`). 7-day TTL, `SameSite=Strict`, `HttpOnly`.

**Admin functions.**
- *Service health* — every 5s the admin page refreshes counters: latest analyzed block, head-tracker last seen block (read from Redis key `indexer:state:last_head`, set with 60s TTL by the head-tracker), blocks-behind, pending queue depth (`LLEN analyzer:queue:pending`), dead-letter depth (`LLEN analyzer:queue:dead`), last-insert age, total rows.

## Database schema

Defined in `crates/indexer-store/migrations/20260101000001_init.sql`.

- `analysis` — fact table, one row per analyzed tx. Indexed by `(project_slug, block_timestamp DESC)` and `(chain_id, block_timestamp DESC)`.
- `projects` — slug → name, category, contact info.
- `address_project` — `(chain_id, address) → project_slug`. Hot lookup table for "which contracts belong to project X". FK on `project_slug → projects.project_slug`.
- `address_label_attempt` — auto-labeler attempt log. One row per (chain_id, address) with `last_attempted_at`, `last_result` (`matched` / `unverified` / `no-match` / `error`), `contract_name`, `matched_slug`. The labeler's producer query joins this to skip recently-failed addresses.
- `eth_prices` — `day → usd_per_eth`. Used for USD savings calculation.
- `project_daily` — materialized view: per-project per-day totals (tx count, gas saved, wei saved, USD saved, avg savings %). This is what `indexer-web` reads for leaderboards and overview totals.

## BD metrics (what the dashboard surfaces)

- **Project leaderboard** — sortable by tx count or avg savings %. The BD call list.
- **Per-project drill-down** — top 10 contracts, top 10 function selectors, daily 90d trend, recent transactions.
- **Whales we haven't labeled** — top `unknown:0x...` rows by ETH saved last 30d. The auto-labeler reads the same ranking from `analysis` to prioritize Etherscan lookups.
- **Service health** — blocks behind head, queue depth, dead-letter depth, last-insert age, total rows.

## Configuration

All via env vars. See `.env.example` for the full list. Most important:

| Var | Purpose | Default |
|---|---|---|
| `RPC_URL` | Required. Premium provider endpoint. | — |
| `CHAIN_ID` | Stamps every analysis row. **One indexer instance per chain.** | `1` |
| `DATABASE_URL`, `REDIS_URL` | Standard. | — |
| `RPC_RPS_BUDGET` | Sustained tokens/sec. **Top priority.** Aim for ~30-40% of plan ceiling — conservative on purpose. | `100` |
| `RPC_BURST` | Short-spike allowance. | `25` |
| `RPC_MAX_CONCURRENCY` | Hard cap on simultaneous outbound calls. | `8` |
| `MIN_GAS_USED` | Skip txs below this. | `50000` |
| `HEURISTIC_ONLY` | Estimate from state updates alone — no preceding-tx replay, no EvmSketch fork. ~300× fewer RPC calls per analysis; estimates are cruder (call-heavy txs report zero savings) and rows get `is_heuristic = true`. | `false` |
| `MAX_QUEUE_DEPTH` | Backpressure threshold for head-tracker. | `1000` |
| `MAX_BLOCKS_BEHIND` | Bound head-tracker lag: when the cursor falls more than this many blocks behind head, skip it forward and drop the intermediate blocks. `0` = never drop, lag and catch up. | `0` |
| `WORKER_MAX_RETRIES` | Per-job retry count before dead-letter. | `3` |
| `WORKER_ANALYZE_TIMEOUT_SECS` | Hard cap on a single `analyze_tx`. Backstop against hung HTTP reads pinning a worker. | `60` |
| `QUEUE_JOB_TTL_SECS` | Workers drop queued jobs older than this at claim time instead of analyzing them stale. `0` disables expiry. | `3600` |
| `OVERLAY_PATH` | Curated address overlay YAML. | `/etc/indexer/overlay.yaml` |
| `DEFILLAMA_URL`, `PRICE_URL` | Empty disables the corresponding refresh. | (set) |
| `ETHERSCAN_API_KEY` | Empty disables the auto-labeler loop. Free key from etherscan.io/myapikey. | — |
| `LABELER_PRODUCER_INTERVAL_SECS` | How often the producer rebuilds the priority queue. | `3600` |
| `LABELER_BATCH_SIZE` | Max addresses pushed into the queue per producer tick. | `200` |
| `LABELER_RETRY_DAYS` | Re-attempt window for `unverified` / `no-match` results. | `7` |
| `LABELER_MIN_DELAY_MS` | Floor delay between Etherscan calls. Free tier v2 caps at 3 rps. | `400` |
| `KNOWN_NAMES_PATH` | Curated Etherscan-name → slug dictionary. | `/etc/indexer/known_names.yaml` |
| `SESSION_SECRET` | Required for `indexer-web`. HMAC-SHA256 key, ≥32 bytes. | — |
| `EXPLORER_TX_URL`, `EXPLORER_ADDRESS_URL` | Block-explorer base URLs (trailing slash) used in tx / address links. | Etherscan |

## Deployment

```bash
# .env (minimum: POSTGRES_PASSWORD, RPC_URL, SESSION_SECRET).
cp .env.example .env
$EDITOR .env

# Allowlist: at least one user with a bcrypt password hash.
cp users.yaml.example users.yaml
htpasswd -bnBC 12 "" 'YOUR-PASSWORD' | tr -d ':\n'   # paste the hash into users.yaml

# Build the image once.
docker compose build indexer-build

# Start everything.
docker compose up -d

# Open the dashboard: http://<host>:3000 — sign in with the user you added.
```

**Multi-chain**: stand up a second compose stack with a different `CHAIN_ID`, separate Postgres database, and a different RPC endpoint. The service does not multiplex chains in one process by design — every layer is chain-partitioned (`analysis.chain_id` in the PK, MVs grouped by `chain_id`, every web query filters `WHERE chain_id = $1`), so a new chain is a deployment, not a code change.

### Running a second chain (Sepolia)

`docker-compose.sepolia.yml` is a ready-made second stack for Ethereum Sepolia (chain `11155111`) — its own Postgres, Redis, networks and volumes, isolated from the mainnet project, with the dashboard on `127.0.0.1:3001`.

```bash
# Add the Sepolia RPC to .env (must support debug_traceTransaction).
echo 'SEPOLIA_RPC_URL=https://your-sepolia-rpc' >> .env

# Build the shared image (skip if the mainnet stack already built it).
docker compose -f docker-compose.sepolia.yml build indexer-build

# Start the Sepolia stack. Schema auto-migrates on first boot
# (worker/refresher call store.migrate()).
docker compose -f docker-compose.sepolia.yml up -d

# Dashboard on :3001 — reverse-proxy sepolia.<your-domain> at it.
# Logs / psql / teardown:
docker compose -f docker-compose.sepolia.yml logs -f head-tracker worker
docker compose -f docker-compose.sepolia.yml exec postgres psql -U indexer indexer
docker compose -f docker-compose.sepolia.yml down            # add -v to wipe data
```

It reuses `POSTGRES_PASSWORD` / `SESSION_SECRET` / `ETHERSCAN_API_KEY` / `OPENROUTER_*` / `users.yaml` from the mainnet `.env`. What differs on Sepolia, and why:

- **USD is priced at the mainnet ETH rate.** Testnet ETH has no market; the CoinGecko endpoint fetches `ethereum` regardless of chain, so "$ saved" reads as *would-save-on-mainnet* — useful for BD framing.
- **DefiLlama is disabled** (`DEFILLAMA_URL=""`). Its `/protocols` list is mainnet-only (chain_id 1) and can never match a Sepolia tx. Labeling falls back to Etherscan (the V2 API resolves Sepolia via `chainid=11155111` with the same key) plus `overlay.yaml` manual mappings — add entries with `chain_id: 11155111`.
- **Gas estimation uses the mainnet hardfork spec** (`EthSpec::mainnet()`). Per-opcode gas costs are identical to Sepolia at the current fork, so estimates are valid. The only caveat is the brief windows where Sepolia activates a fork ahead of mainnet — re-validate around network upgrades.

## Labeling unknown contracts

Three layers, in priority order:

1. **`crates/indexer-resolver/data/overlay.yaml`** — hand-curated address → project mapping. Highest priority; always wins.
   ```yaml
   - chain_id: 1
     address: "0x..."             # contract address (lowercase)
     project_slug: "uniswap-v3"
     project_name: "Uniswap V3"
     category: "dex"              # optional
     contact:                      # optional, used for BD outreach
       primary: "team@example.com"
       url: "https://discord.gg/..."
   ```
   Rebuild + restart, or wait for the resolver's 24h refresh.

2. **DefiLlama bulk harvest** — every resolver refresh, the `address` field exposed by `https://api.llama.fi/protocols` is parsed and inserted into `address_project`. Currently catches ~1,000 Ethereum-mainnet addresses (mostly governance tokens like UNI / AAVE / COMP). Doesn't help for routers / aggregators / stablecoins.

3. **Auto-labeler (Etherscan)** — for everything in (2)'s blind spots, the labeler picks the highest-`wei_saved` unknown contract, fetches its `ContractName` from Etherscan, and looks it up in `crates/indexer-resolver/data/known_names.yaml`:
   ```yaml
   - name: UniswapV2Router02
     slug: uniswap-v2
   - name: TetherToken
     slug: tether
   ```
   Names match case-insensitively after stripping common suffixes (`V2`, `Router02`, `Proxy`, `Token`). Add a new entry whenever you see a recurring `ContractName` in the labeler logs that isn't matching — the next producer cycle will re-attempt every `error` row, and the next manual rebuild will pick up new dictionary entries.

When (2) or (3) introduces a new mapping, `relabel_unknowns()` runs immediately and rewrites all historical `analysis` rows for that address.

## Known v1 limitations

These are intentional trade-offs from the "minimal-touch wrapper" choice — the existing gas-analyzer crates were not modified. Each can be addressed later:

1. **EvmSketch internal RPC bypass.** `GasKillerEvmSketch::builder().build()` constructs its own internal `alloy` provider per `analyze_tx` call. Storage reads issued during simulation (`eth_getStorageAt`, `eth_getCode`) are not gated by our rate limiter. We compensate with a deliberately generous `weights::ANALYZE_TX = 250` charge that over-approximates the bypassed traffic. Closing this gap requires either (a) lifting the orchestration into `gas_analyzer_evmsketch` so it accepts a pre-built rate-limited provider, or (b) running an in-process HTTP proxy that sits in front of all RPC calls.

2. **No queue visibility timeout.** A worker that crashes mid-job loses that job. Acceptable because we ignore reorgs and per-block tx volume regenerates state quickly. To fix: track in-flight jobs in a Redis hash and have the head-tracker (or a sweeper) reclaim stale ones.

3. **No reorg handling.** ~1% of mainnet head rows may correspond to reorged blocks. Acceptable per project decision. To fix: lag the head by N confirmations, or analyze head + retract on reorg.

4. **No proxy resolution in the labeler.** When Etherscan returns a proxy contract (e.g. `FiatTokenProxy`), we use the proxy's `ContractName` and don't follow `Implementation`. Most proxy-fronted contracts have an obvious name (FiatTokenProxy → usd-coin) that the dictionary handles directly, so this is rarely load-bearing. To fix: a follow-up labeler step that fetches the implementation address's `ContractName` and re-runs the dict lookup if the proxy name didn't match.

5. **Library errors still use `anyhow`.** The `feedback_thiserror_internal.md` memory says internal libraries should use `thiserror`. The new indexer crates do; the existing gas-analyzer crates still use `anyhow` because we agreed not to touch them. To fix: a separate refactor PR that migrates `gas-analyzer-{core,rpc,evmsketch,anvil,gas-estimator}` to typed errors.

6. **Per-worker rate limiter, not global.** With `worker.deploy.replicas: 4`, each worker has its own in-process token bucket. The configured `RPC_RPS_BUDGET` therefore applies *per worker*, not globally. Either set the budget to `total / N`, or move to a Redis-backed shared limiter.

## Verification

```bash
# Unit tests (in-process, no external deps).
cargo test -p indexer-api -p indexer-rpc -p indexer-store -p indexer-resolver --lib

# Type-check the whole workspace.
cargo check --workspace

# Smoke test (live, requires .env with real RPC).
docker compose up -d
docker compose logs -f head-tracker worker
# Expect: "block fanned out" lines from head-tracker and "persisted" lines from workers
# within ~30s of startup. Check `analysis` row count grows.
psql $DATABASE_URL -c "SELECT count(*) FROM analysis;"
```
