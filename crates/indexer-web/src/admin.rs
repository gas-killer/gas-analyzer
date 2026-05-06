//! Admin handlers: read-only service health + on-demand tx replay.

use askama::Template;
use askama_axum::IntoResponse;
use axum::Form;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use indexer_service::queue::AnalyzeTxJob;
use redis::AsyncCommands;
use serde::Deserialize;

use crate::AppState;
use crate::auth::AuthUser;
use crate::error::WebError;
use crate::queries;

#[derive(Template)]
#[template(path = "admin.html")]
pub struct AdminPage {
    pub user: String,
    pub health: HealthView,
    pub chain_id: i64,
    pub explorer_tx_url: String,
    pub replay_message: Option<ReplayBanner>,
}

#[derive(Template)]
#[template(path = "_health.html")]
pub struct HealthFragment {
    pub health: HealthView,
}

#[derive(Debug, Clone)]
pub struct HealthView {
    pub last_seen_block: Option<i64>,
    pub latest_analyzed_block: Option<i64>,
    pub blocks_behind: Option<i64>,
    pub head_stale: bool,
    pub pending_queue: i64,
    pub dead_letter: i64,
    pub heuristic_rate_1h: String,
    pub heuristic_rate_24h: String,
    pub last_insert_age_secs: Option<i64>,
    pub total_rows: i64,
}

#[derive(Debug, Clone)]
pub struct ReplayBanner {
    pub success: bool,
    pub message: String,
    pub tx_hash_hex: Option<String>,
}

pub async fn admin_page(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
) -> Result<Response, WebError> {
    let health = collect_health(&state).await?;
    let page = AdminPage {
        user,
        health,
        chain_id: state.chain_id,
        explorer_tx_url: state.explorer_tx_url.as_str().to_string(),
        replay_message: None,
    };
    Ok(page.into_response())
}

pub async fn health_partial(
    _user: AuthUser,
    State(state): State<AppState>,
) -> Result<Response, WebError> {
    let health = collect_health(&state).await?;
    Ok(HealthFragment { health }.into_response())
}

#[derive(Debug, Deserialize)]
pub struct ReplayForm {
    pub tx_hash: String,
}

pub async fn replay_post(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Form(form): Form<ReplayForm>,
) -> Result<Response, WebError> {
    let raw = form.tx_hash.trim();
    let stripped = raw.strip_prefix("0x").unwrap_or(raw);
    let bytes = hex::decode(stripped)
        .map_err(|e| WebError::BadRequest(format!("invalid hex: {e}")))?;
    if bytes.len() != 32 {
        return Err(WebError::BadRequest(format!(
            "tx hash must be 32 bytes, got {}",
            bytes.len()
        )));
    }
    let mut tx_hash = [0u8; 32];
    tx_hash.copy_from_slice(&bytes);

    let job = AnalyzeTxJob {
        chain_id: state.chain_id as u64,
        tx_hash,
        // The worker only uses tx_hash; block_number / tx_index are just for
        // dead-letter context. Zero them; the analyzer fetches the receipt.
        block_number: 0,
        tx_index: 0,
        attempt: 0,
    };

    let payload = serde_json::to_vec(&job)?;
    let mut conn = state.redis.clone();
    let _: () = conn
        .rpush(indexer_service::queue::QUEUE_KEY, payload)
        .await?;

    tracing::info!(user, tx_hash = %hex::encode(tx_hash), "replay enqueued");

    let health = collect_health(&state).await?;
    let page = AdminPage {
        user,
        health,
        chain_id: state.chain_id,
        explorer_tx_url: state.explorer_tx_url.as_str().to_string(),
        replay_message: Some(ReplayBanner {
            success: true,
            message: "Job enqueued. Refresh the page in ~10s; if the tx isn't in the analysis table by then, check worker logs.".to_string(),
            tx_hash_hex: Some(format!("0x{}", hex::encode(tx_hash))),
        }),
    };
    Ok((StatusCode::OK, page).into_response())
}

async fn collect_health(state: &AppState) -> Result<HealthView, WebError> {
    let chain_id = state.chain_id;
    let pool = state.store.pool();

    let analysis = queries::analysis_health(pool, chain_id).await?;
    let h1 = queries::heuristic_rate(pool, chain_id, "1 hour").await?;
    let h24 = queries::heuristic_rate(pool, chain_id, "24 hours").await?;

    let mut conn = state.redis.clone();
    let pending: i64 = conn
        .llen::<_, i64>(indexer_service::queue::QUEUE_KEY)
        .await
        .unwrap_or(0);
    let dead: i64 = conn
        .llen::<_, i64>(indexer_service::queue::DEAD_KEY)
        .await
        .unwrap_or(0);
    let last_head_raw: Option<i64> = conn
        .get::<_, Option<i64>>(indexer_service::state::LAST_HEAD_KEY)
        .await
        .unwrap_or(None);
    let head_stale = last_head_raw.is_none();

    let blocks_behind = match (last_head_raw, analysis.latest_block) {
        (Some(h), Some(a)) => Some(h - a),
        _ => None,
    };

    Ok(HealthView {
        last_seen_block: last_head_raw,
        latest_analyzed_block: analysis.latest_block,
        blocks_behind,
        head_stale,
        pending_queue: pending,
        dead_letter: dead,
        heuristic_rate_1h: format_pct(h1.rate),
        heuristic_rate_24h: format_pct(h24.rate),
        last_insert_age_secs: analysis.last_insert_age_secs.map(|s| s as i64),
        total_rows: analysis.total_rows.unwrap_or(0),
    })
}

fn format_pct(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{:.1}%", x * 100.0),
        None => "—".to_string(),
    }
}
