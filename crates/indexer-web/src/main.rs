//! `indexer-web`: read-only BD dashboard + narrow admin surface for the
//! gas-killer indexer service.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod admin;
mod auth;
mod diagnostics;
mod error;
mod handlers;
mod labels;
mod llm;
mod queries;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::FromRef;
use axum::routing::{get, post};
use axum::response::Redirect;
use clap::Parser;
use indexer_store::Store;
use redis::aio::ConnectionManager;
use tower_cookies::CookieManagerLayer;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::auth::AuthState;

#[derive(Debug, Parser)]
#[command(name = "indexer-web", about = "BD dashboard for the gas-killer indexer")]
struct Cli {
    /// Postgres connection string.
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Redis connection string.
    #[arg(long, env = "REDIS_URL", default_value = "redis://127.0.0.1:6379")]
    redis_url: String,

    /// HMAC secret for session cookies. Must be >=32 bytes.
    #[arg(long, env = "SESSION_SECRET")]
    session_secret: String,

    /// Path to the YAML allowlist of (username, bcrypt_hash) pairs.
    #[arg(long, env = "AUTH_ALLOWLIST_PATH", default_value = "/etc/indexer/users.yaml")]
    auth_allowlist_path: PathBuf,

    /// Bind address.
    #[arg(long, env = "BIND_ADDR", default_value = "0.0.0.0:3000")]
    bind_addr: String,

    /// Chain ID this dashboard is showing. Must match the indexer-service
    /// deployment writing into the connected DB.
    #[arg(long, env = "CHAIN_ID", default_value_t = 1)]
    chain_id: i64,

    /// Block-explorer base URL for tx links (trailing slash included).
    #[arg(long, env = "EXPLORER_TX_URL", default_value = "https://etherscan.io/tx/")]
    explorer_tx_url: String,

    /// Block-explorer base URL for address links (trailing slash included).
    #[arg(long, env = "EXPLORER_ADDRESS_URL", default_value = "https://etherscan.io/address/")]
    explorer_address_url: String,

    /// Static-file directory served at `/static`.
    #[arg(long, env = "STATIC_DIR", default_value = "crates/indexer-web/static")]
    static_dir: PathBuf,

    // -------- Manual refresh sources --------
    //
    // Mirror the relevant CommonConfig / RefresherConfig env vars so the
    // admin "refresh now" buttons can hit the same endpoints the loop hits.
    #[arg(long, env = "OVERLAY_PATH", default_value = "/etc/indexer/overlay.yaml")]
    overlay_path: PathBuf,

    #[arg(long, env = "DEFILLAMA_URL", default_value = "https://api.llama.fi/protocols")]
    defillama_url: String,

    #[arg(long, env = "PRICE_URL",
          default_value = "https://api.coingecko.com/api/v3/simple/price?ids=ethereum&vs_currencies=usd")]
    price_url: String,

    #[arg(long, env = "LABELER_BATCH_SIZE", default_value_t = 200)]
    labeler_batch_size: i64,

    #[arg(long, env = "LABELER_RETRY_DAYS", default_value_t = 7)]
    labeler_retry_days: i64,

    // -------- AI diagnostics --------
    /// OpenRouter API key. Empty disables the diagnostics button.
    #[arg(long, env = "OPENROUTER_KEY", default_value = "")]
    openrouter_key: String,

    /// OpenRouter model identifier.
    #[arg(long, env = "OPENROUTER_MODEL", default_value = "anthropic/claude-sonnet-4-6")]
    openrouter_model: String,

    /// OpenRouter base URL (no trailing slash).
    #[arg(long, env = "OPENROUTER_BASE_URL", default_value = "https://openrouter.ai/api/v1")]
    openrouter_base_url: String,

    /// Cache TTL for the most recent diagnose response (seconds). Repeat
    /// clicks within this window return the cached result.
    #[arg(long, env = "DIAGNOSE_CACHE_TTL_SECS", default_value_t = 30)]
    diagnose_cache_ttl_secs: u64,

    /// Minimum gap between diagnose requests across all users (seconds).
    /// Stops accidental double-clicks from doubling spend.
    #[arg(long, env = "DIAGNOSE_RATE_LIMIT_SECS", default_value_t = 10)]
    diagnose_rate_limit_secs: u64,
}

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub redis: ConnectionManager,
    pub auth: AuthState,
    pub chain_id: i64,
    pub explorer_tx_url: Arc<String>,
    pub explorer_address_url: Arc<String>,

    // Refresh-button surface.
    pub resolver: Arc<indexer_resolver::Resolver>,
    pub overlay_path: Arc<PathBuf>,
    pub defillama_url: Arc<String>,
    pub price_url: Arc<String>,
    pub labeler_batch_size: i64,
    pub labeler_retry_days: i64,

    // AI diagnostics. `llm` is None when no API key is configured.
    pub llm: Option<llm::LlmClient>,
    pub diagnose_cache: Arc<tokio::sync::Mutex<DiagnoseCache>>,
    pub diagnose_rate_limit_secs: u64,
    pub diagnose_cache_ttl_secs: u64,
    /// Hint string used by the diagnostics bundle to surface "etherscan
    /// labeling enabled" without leaking the key. Empty when disabled.
    pub etherscan_enabled_hint: Arc<String>,
}

/// In-memory cache for the most recent diagnose response. Per-process,
/// per-restart — no cross-replica coordination needed since this is a
/// single-replica admin tool.
#[derive(Default)]
pub struct DiagnoseCache {
    pub last_response: Option<(std::time::Instant, String)>,
    pub last_call_at: Option<std::time::Instant>,
}

impl FromRef<AppState> for AuthState {
    fn from_ref(s: &AppState) -> Self {
        s.auth.clone()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(true)
        .init();

    let cli = Cli::parse();

    let auth = AuthState::new(&cli.auth_allowlist_path, cli.session_secret.into_bytes())
        .context("load auth allowlist")?;

    let store = Store::connect(&cli.database_url, 6)
        .await
        .context("connect postgres")?;

    let redis_client = redis::Client::open(cli.redis_url.as_str()).context("redis client")?;
    let redis = ConnectionManager::new(redis_client)
        .await
        .context("redis connection")?;

    let llm = if cli.openrouter_key.trim().is_empty() {
        tracing::info!("AI diagnostics disabled (OPENROUTER_KEY not set)");
        None
    } else {
        match llm::LlmClient::new(
            cli.openrouter_key.trim().to_string(),
            cli.openrouter_base_url,
            cli.openrouter_model.clone(),
        ) {
            Ok(c) => {
                tracing::info!(model = %cli.openrouter_model, "AI diagnostics enabled");
                Some(c)
            }
            Err(e) => {
                tracing::warn!(error = %e, "AI diagnostics disabled (client init failed)");
                None
            }
        }
    };
    let etherscan_enabled_hint = if std::env::var("ETHERSCAN_API_KEY")
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        "etherscan".to_string()
    } else {
        String::new()
    };

    let state = AppState {
        store,
        redis,
        auth,
        chain_id: cli.chain_id,
        explorer_tx_url: Arc::new(cli.explorer_tx_url),
        explorer_address_url: Arc::new(cli.explorer_address_url),
        resolver: Arc::new(indexer_resolver::Resolver::new()),
        overlay_path: Arc::new(cli.overlay_path),
        defillama_url: Arc::new(cli.defillama_url),
        price_url: Arc::new(cli.price_url),
        labeler_batch_size: cli.labeler_batch_size,
        labeler_retry_days: cli.labeler_retry_days,
        llm,
        diagnose_cache: Arc::new(tokio::sync::Mutex::new(DiagnoseCache::default())),
        diagnose_rate_limit_secs: cli.diagnose_rate_limit_secs,
        diagnose_cache_ttl_secs: cli.diagnose_cache_ttl_secs,
        etherscan_enabled_hint: Arc::new(etherscan_enabled_hint),
    };

    let app = Router::new()
        .route("/", get(handlers::public::overview))
        .route("/projects/:slug", get(handlers::public::project))
        .route("/unknowns", get(handlers::public::unknowns))
        .route("/admin", get(admin::admin_page))
        .route("/admin/health", get(admin::health_partial))
        .route("/admin/refresh/rollups",   post(admin::refresh_rollups))
        .route("/admin/refresh/eth-price", post(admin::refresh_eth_price))
        .route("/admin/refresh/resolver",  post(admin::refresh_resolver))
        .route("/admin/refresh/labeler",   post(admin::refresh_labeler))
        .route("/admin/refresh/relabel",   post(admin::refresh_relabel))
        .route("/admin/diagnose",          post(admin::diagnose))
        .route("/api/labels/cell",         get(labels::label_cell))
        .route("/api/labels/edit",         get(labels::label_edit))
        .route("/api/labels/override",     post(labels::label_override))
        .route("/api/projects/cell",       get(labels::project_cell))
        .route("/api/projects/edit",       get(labels::project_edit))
        .route("/api/projects/rename",     post(labels::project_rename))
        .route("/login", get(handlers::public::login_get).post(handlers::public::login_post))
        .route("/logout", get(handlers::public::logout))
        .route("/healthz", get(|| async { "ok" }))
        .nest_service("/static", tower_http::services::ServeDir::new(&cli.static_dir))
        .fallback(get(|| async { Redirect::to("/") }))
        .layer(CookieManagerLayer::new())
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cli.bind_addr)
        .await
        .with_context(|| format!("bind {}", cli.bind_addr))?;
    tracing::info!(bind = cli.bind_addr, "indexer-web listening");
    axum::serve(listener, app).await.context("axum serve")?;
    Ok(())
}
