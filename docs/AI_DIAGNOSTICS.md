# Plan: AI-generated diagnostics for the indexer admin page

A "Diagnose now" button in `/admin` that gathers a bundle of operational
signals (queue depths, recent error logs, insert rates, blocks-behind, top
unknowns, recent labeler outcomes), sends them to an LLM via OpenRouter, and
renders a short human-readable summary explaining what's going wrong, what's
healthy, and what the operator should consider doing next.

## Why this and not just a static dashboard

The dashboard already shows the *primitives* (queue depth, blocks behind,
last insert age). What the operator needs is *interpretation* — "your
workers are stuck retrying QuickNode 429s, and the queue is full of May 6
jobs from a backlog that started after restart at 12:24 — consider draining
the queue or upgrading the RPC plan." That synthesis is exactly what an LLM
is good at given a structured signal bundle.

This is **explicitly not** a chatbot or an autonomous agent — it's a
button that produces one paragraph of diagnosis per click.

## Locked decisions (proposing)

- **Provider**: OpenRouter (`openrouter.ai`). Drop-in OpenAI-compatible API,
  one API key fans out to many models. Lets us swap models without code
  changes via env var.
- **Model default**: `anthropic/claude-sonnet-4-6` (good at structured ops
  reasoning, fast enough for a button click). Configurable via env.
- **Trigger**: button in `/admin`, manual only. No background polling, no
  scheduled summaries. One paragraph + a list of suggested actions per click.
- **Auth**: same as the rest of `/admin` — bcrypt cookie. No new auth
  surface.
- **Caching**: 30-second TTL on the response. Prevents accidentally double-
  clicking → double-billing. Long enough to cover impatient retries, short
  enough that a refresh after a config change reflects new state.
- **Cost cap**: hard limit of one call per 10 seconds per user (in-memory
  rate limiter), and a daily request count surfaced on the admin page so
  the operator knows roughly what they're spending.
- **No tool-calling / no agentic loops**: single-turn request, single-turn
  response. Keeps cost predictable and the UI deterministic.

## Architecture

```
                                    ┌─────────────────────┐
                                    │ OpenRouter          │
                                    │ /chat/completions   │
                                    └──────────▲──────────┘
                                               │ HTTPS
                                               │ Bearer OPENROUTER_API_KEY
                                               │
   Browser ──── POST /admin/diagnose ────▶ indexer-web ──┐
                                               │         │
                                               ▼         │
                                    ┌─────────────────────┐
                                    │ DiagnosticsCollector │
                                    │  - Postgres aggregates
                                    │  - Redis counters
                                    │  - last 50 worker errors
                                    │  - top 10 unknowns
                                    │  - last 10 labeler outcomes
                                    └──────────┬──────────┘
                                               │ structured JSON bundle
                                               ▼
                                    ┌─────────────────────┐
                                    │ LLMClient (OpenRouter)│
                                    │  - prompt template
                                    │  - 60s timeout
                                    │  - retry on 5xx
                                    └──────────┬──────────┘
                                               │
                                               ▼
                                    ┌─────────────────────┐
                                    │ DiagnosisResult cache │
                                    │ (30s TTL, in-memory) │
                                    └──────────┬──────────┘
                                               │
                                               ▼
                                          rendered partial
                                          (markdown → HTML)
```

The LLM call is the only new external dependency; everything else is local.

## Components

### 1. `DiagnosticsCollector` (new, in `indexer-web/src/diagnostics.rs`)

Async function `collect(state: &AppState) -> Bundle`. Runs ~6 SQL/Redis
queries concurrently (already cheap):

```json
{
  "now": "2026-05-07T13:30:00Z",
  "service_health": {
    "head_block": 25043515,
    "latest_analyzed_block": 25030727,
    "blocks_behind": 12788,
    "pending_queue_depth": 1121,
    "dead_letter_depth": 5568,
    "last_insert_age_secs": 312,
    "total_rows": 893
  },
  "throughput": {
    "rows_last_1h": 17,
    "rows_last_24h": 66,
    "rows_last_7d": 893
  },
  "recent_worker_errors": [
    {"ts": "2026-05-07T13:25:11Z", "level": "warn",
     "kind": "rpc_transient", "message": "HTTP error 429 ..."},
    ...
  ],
  "top_unknowns": [
    {"address": "0xdac17f...", "wei_saved": "1.23e22", "tx_count": 102},
    ...
  ],
  "recent_labeler_outcomes": [
    {"address": "0x3fc91a3...", "result": "matched", "name": "UniversalRouter", "slug": "uniswap-v3"},
    {"address": "0x0ddcc8e...", "result": "error", "name": null, "slug": null},
    ...
  ],
  "config_summary": {
    "rpc_provider": "QuickNode",
    "worker_replicas": 4,
    "max_queue_depth": 1000,
    "etherscan_enabled": true
  }
}
```

Recent worker errors are sourced from a **bounded ring buffer** kept in
`AppState` — see component 4 for how it gets populated.

### 2. `LLMClient` (new, in `indexer-web/src/llm.rs`)

Thin wrapper around OpenRouter's `/api/v1/chat/completions`.

- Reuses `reqwest::Client` from app state.
- 60s request timeout.
- One retry on 5xx with `with_retry`-style backoff (or just inline; the
  helper currently lives in `indexer-rpc` but we can copy the predicate).
- Returns `(content, usage)` so we can log token counts.

### 3. Prompt template

Two parts:

- **System message** (static, ~600 chars): role definition. Tells the model
  it is the indexer's ops assistant; output should be ≤200 words plain
  English; structure as `[health verdict] · [primary issue] · [3 concrete
  actions]`; do not invent numbers, only cite values from the bundle.

- **User message**: the bundle JSON, prefixed with a one-line task:
  ```
  Diagnose the current state of this indexer. Bundle:
  <JSON>
  ```

Single shot. No tools. Temperature 0.2 (deterministic-ish so identical
state → similar diagnosis).

### 4. Recent-errors ring buffer

Currently the only way to read worker logs from the web container would be
mounting the Docker socket (rejected for security). Instead:

- Each service publishes its `WARN`/`ERROR` events to a Redis stream
  (`indexer:events`, capped at 500 entries via `XADD MAXLEN ~500`).
- A small `tracing` layer (or a dedicated subscriber) on each binary
  enqueues serialized event objects.
- `indexer-web` reads the last N entries when building the bundle.

Adds ~80 LOC, one new Redis key. Survives restarts (well, last 500 events
do). Not perfect — events while Redis is down are lost — but acceptable for
a diagnostics aid that's allowed to be best-effort.

This same buffer also powers the **Logs viewer** request in this thread, so
the two features share infrastructure.

### 5. Caching + rate limiting

In-memory in `AppState`:
- `Mutex<Option<(Instant, DiagnosisResult)>>` — cached response.
- `Mutex<Instant>` — last-call timestamp; reject if < 10s ago with a 429
  banner ("rate limited; try again in N seconds").

Both go away on web restart. Acceptable.

### 6. Endpoint + UI

- `POST /admin/diagnose` — runs the pipeline, returns an htmx fragment.
- Button on `/admin` page: `<button hx-post="/admin/diagnose"
  hx-target="#diagnosis" hx-swap="innerHTML">Diagnose now</button>`.
- `<div id="diagnosis"></div>` below.
- Spinner during fetch. Result rendered as markdown → HTML. Includes a
  "generated <ts>, model=<name>, tokens=<in>+<out>" footer.

## Files

**New:**
- `crates/indexer-web/src/diagnostics.rs` (~150 LOC)
- `crates/indexer-web/src/llm.rs` (~120 LOC)
- `crates/indexer-web/src/event_log.rs` (~80 LOC) — reads the ring buffer
- `crates/indexer-rpc/src/event_pub.rs` (new module) (~60 LOC) — `tracing`
  layer that publishes WARN/ERROR events to Redis stream
- `docs/AI_DIAGNOSTICS.md` — this doc

**Modified:**
- `crates/indexer-web/Cargo.toml` — `comrak` (markdown→HTML), `pulldown-cmark`
  alternative; `dashmap` if we go with concurrent cache.
- `crates/indexer-web/src/main.rs` — wire up `OPENROUTER_API_KEY`,
  reqwest client in `AppState`, add `/admin/diagnose` route.
- `crates/indexer-web/src/admin.rs` — handler.
- `crates/indexer-web/templates/admin.html` — button + result div.
- `crates/indexer-service/src/{head_tracker,worker,refresher,labeler}.rs`
  — wire the event-publishing tracing layer (one-line subscribe at startup).
- `docker-compose.yml` — pass `OPENROUTER_API_KEY` and
  `OPENROUTER_MODEL` env vars to indexer-web.
- `.env.example` — document the vars.

## Configuration (env vars)

```
OPENROUTER_API_KEY=                                  # required (empty disables button)
OPENROUTER_MODEL=anthropic/claude-sonnet-4-6         # default
OPENROUTER_BASE_URL=https://openrouter.ai/api/v1     # override for proxies
DIAGNOSE_CACHE_TTL_SECS=30
DIAGNOSE_RATE_LIMIT_SECS=10
EVENT_BUFFER_MAX=500
```

## Threats / cost

- **Prompt injection**: the bundle includes free-text error messages from
  RPC/Etherscan responses. An attacker who controls those (e.g. via a
  malicious RPC) could attempt to manipulate the model. Mitigation:
  bundle is JSON-encoded so the model parses it as data; system prompt
  instructs the model to ignore instructions inside data. Low risk for
  this use case (the operator reads the output and acts manually).
- **Cost**: a typical bundle is ~3–5 KB → ~1.5k input tokens. Output is
  capped at 400 tokens. At Sonnet 4.6 prices that's <$0.01/click. Daily
  cap of ~50 clicks ≈ $0.50/day worst case.
- **Latency**: ~2–6 seconds per click. The button shows a spinner.
- **Privacy**: bundle includes contract addresses, tx counts, error
  messages. No private keys, no user data. Acceptable for the BD use case.

## Out of scope (deferred)

- Multi-turn chat. The button is fire-and-forget; if the operator wants
  to drill in further, they read the dashboard.
- Action-taking. The model never executes anything; it only suggests.
- Streaming responses. We block on the full response — keeps the UI code
  simple and reduces edge cases.
- Per-user usage tracking. Single-tenant tool; aggregate is enough.
- Model auto-selection based on bundle complexity. One model, configured.

## Verification plan

1. With `OPENROUTER_API_KEY` empty, the admin page shows the button greyed
   out with tooltip "OPENROUTER_API_KEY not set".
2. With key set, click button → spinner → 2–6s later, paragraph appears
   citing real numbers from the dashboard.
3. Inspect `indexer-web` logs: one info line per click with `tokens_in`,
   `tokens_out`, `model`, `duration_ms`.
4. Click twice in quick succession → second click returns the cached
   response (banner: "cached, regenerated in N s").
5. Click 5x in 30s → some are rate-limited with the rate-limit banner.
6. Stop a worker container, click again after ~2 min → the diagnosis
   should mention "the worker pool is reduced" or similar (sanity check
   that the bundle reflects current state, not stale cache).

## Sequence

1. **Phase 1** — event ring buffer (component 4). Useful on its own as the
   logs viewer. Ship + verify before touching LLMs.
2. **Phase 2** — `LLMClient` + prompt + endpoint + button. No caching.
3. **Phase 3** — caching + rate limiting + observability. Polishes the
   feature for daily use.

I'd recommend building it in that order so each phase is independently
useful (Phase 1 already gives you the logs viewer that was the other
question in this thread).
