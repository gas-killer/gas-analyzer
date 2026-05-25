use std::path::PathBuf;
use url::Url;

#[derive(Debug, Clone, clap::Args)]
pub struct CommonConfig {
    /// Ethereum-compatible JSON-RPC endpoint.
    #[arg(long, env = "RPC_URL")]
    pub rpc_url: Url,

    /// Chain ID this deployment is indexing. Stored on every analysis row.
    #[arg(long, env = "CHAIN_ID", default_value_t = 1)]
    pub chain_id: u64,

    /// Postgres URL.
    #[arg(long, env = "DATABASE_URL")]
    pub database_url: String,

    /// Redis URL for the job queue.
    #[arg(long, env = "REDIS_URL", default_value = "redis://127.0.0.1:6379")]
    pub redis_url: String,

    /// Sustained RPC tokens-per-second budget. Tune to ~30-40% of provider
    /// plan ceiling (CU/s for Alchemy-style plans). Conservative default.
    #[arg(long, env = "RPC_RPS_BUDGET", default_value_t = 100)]
    pub rpc_rps_budget: u32,

    /// Burst allowance for short spikes.
    #[arg(long, env = "RPC_BURST", default_value_t = 25)]
    pub rpc_burst: u32,

    /// Hard cap on simultaneous outbound RPC operations.
    #[arg(long, env = "RPC_MAX_CONCURRENCY", default_value_t = 8)]
    pub rpc_max_concurrency: usize,

    /// Skip transactions whose `gas_used` is below this threshold.
    #[arg(long, env = "MIN_GAS_USED", default_value_t = 50_000)]
    pub min_gas_used: u64,

    /// Path to the curated address-overlay YAML.
    #[arg(long, env = "OVERLAY_PATH", default_value = "/etc/indexer/overlay.yaml")]
    pub overlay_path: PathBuf,

    /// DefiLlama protocols endpoint. Empty string disables.
    #[arg(
        long,
        env = "DEFILLAMA_URL",
        default_value = "https://api.llama.fi/protocols"
    )]
    pub defillama_url: String,

    /// Coingecko price endpoint for ETH/USD. Empty string disables.
    #[arg(
        long,
        env = "PRICE_URL",
        default_value = "https://api.coingecko.com/api/v3/simple/price?ids=ethereum&vs_currencies=usd"
    )]
    pub price_url: String,
}

#[derive(Debug, Clone, clap::Args)]
pub struct HeadTrackerConfig {
    /// Pause enqueueing while pending queue depth exceeds this value
    /// (measured in *jobs*, not blocks).
    #[arg(long, env = "MAX_QUEUE_DEPTH", default_value_t = 1000)]
    pub max_queue_depth: usize,

    /// Polling interval for new blocks (ms).
    #[arg(long, env = "HEAD_POLL_MS", default_value_t = 4000)]
    pub head_poll_ms: u64,
}

#[derive(Debug, Clone, clap::Args)]
pub struct WorkerConfig {
    /// Per-worker max retries on transient failure before dead-lettering.
    #[arg(long, env = "WORKER_MAX_RETRIES", default_value_t = 3)]
    pub max_retries: u32,

    /// Hard cap on a single `analyze_tx` invocation. Backstop against hung
    /// RPC reads pinning the worker. On timeout the job is requeued.
    #[arg(long, env = "WORKER_ANALYZE_TIMEOUT_SECS", default_value_t = 60)]
    pub analyze_timeout_secs: u64,
}

#[derive(Debug, Clone, clap::Args)]
pub struct RefresherConfig {
    /// Resolver / DefiLlama refresh interval (seconds).
    #[arg(long, env = "RESOLVER_REFRESH_SECS", default_value_t = 86_400)]
    pub resolver_refresh_secs: u64,

    /// ETH/USD price refresh interval (seconds).
    #[arg(long, env = "PRICE_REFRESH_SECS", default_value_t = 3600)]
    pub price_refresh_secs: u64,

    /// Materialized view refresh interval (seconds).
    #[arg(long, env = "ROLLUP_REFRESH_SECS", default_value_t = 3600)]
    pub rollup_refresh_secs: u64,

    // -------- Auto-labeler --------
    //
    /// Etherscan API key. If empty, the labeler loop is disabled and the
    /// other refresher loops still run.
    #[arg(long, env = "ETHERSCAN_API_KEY", default_value = "")]
    pub etherscan_api_key: String,

    /// How often the labeler producer rebuilds its priority queue from the
    /// `analysis` table (seconds). The consumer drains continuously.
    #[arg(long, env = "LABELER_PRODUCER_INTERVAL_SECS", default_value_t = 3600)]
    pub labeler_producer_interval_secs: u64,

    /// Max addresses to push into the queue on each producer tick.
    #[arg(long, env = "LABELER_BATCH_SIZE", default_value_t = 200)]
    pub labeler_batch_size: i64,

    /// Re-attempt previously-failed addresses after this many days. Lets us
    /// pick up newly-verified contracts and newly-added dictionary entries.
    #[arg(long, env = "LABELER_RETRY_DAYS", default_value_t = 7)]
    pub labeler_retry_days: i64,

    /// Minimum delay between consecutive Etherscan calls (ms). Free tier on
    /// the v2 API uses a sliding 1-second window with a strict 3-call cap;
    /// 400ms can still trip it under jitter. 600ms gives ~1.6 rps with
    /// breathing room.
    #[arg(long, env = "LABELER_MIN_DELAY_MS", default_value_t = 600)]
    pub labeler_min_delay_ms: u64,

    /// Path to the curated Etherscan-name → slug dictionary.
    #[arg(
        long,
        env = "KNOWN_NAMES_PATH",
        default_value = "/etc/indexer/known_names.yaml"
    )]
    pub known_names_path: PathBuf,

    // -------- 4byte selector resolver --------
    //
    /// How often to scan for unresolved function selectors (seconds).
    #[arg(long, env = "FOURBYTE_TICK_SECS", default_value_t = 3600)]
    pub fourbyte_tick_secs: u64,

    /// Max selectors fetched per tick.
    #[arg(long, env = "FOURBYTE_BATCH_SIZE", default_value_t = 100)]
    pub fourbyte_batch_size: i64,

    /// Delay between consecutive 4byte requests (ms). The free API has no
    /// documented rate limit but we throttle to be polite.
    #[arg(long, env = "FOURBYTE_PER_REQ_DELAY_MS", default_value_t = 200)]
    pub fourbyte_per_req_delay_ms: u64,
}
