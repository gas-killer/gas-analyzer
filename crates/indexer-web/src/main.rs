//! `indexer-web`: read-only BD dashboard + narrow admin surface for the
//! gas-killer indexer service.

mod admin;
mod auth;
mod error;
mod handlers;
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
}

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub redis: ConnectionManager,
    pub auth: AuthState,
    pub chain_id: i64,
    pub explorer_tx_url: Arc<String>,
    pub explorer_address_url: Arc<String>,
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

    let state = AppState {
        store,
        redis,
        auth,
        chain_id: cli.chain_id,
        explorer_tx_url: Arc::new(cli.explorer_tx_url),
        explorer_address_url: Arc::new(cli.explorer_address_url),
    };

    let app = Router::new()
        .route("/", get(handlers::public::overview))
        .route("/projects/:slug", get(handlers::public::project))
        .route("/unknowns", get(handlers::public::unknowns))
        .route("/admin", get(admin::admin_page))
        .route("/admin/health", get(admin::health_partial))
        .route("/admin/replay", post(admin::replay_post))
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
