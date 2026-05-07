//! Admin handlers: read-only service health + on-demand refresh buttons.
//!
//! The refresh endpoints reuse the same functions the refresher loop calls
//! (now exposed as `pub` from `indexer-service`). Each returns an htmx
//! fragment with status + duration so the admin page can swap a banner in
//! place without a full reload.

use std::time::Instant;

use askama::Template;
use askama_axum::IntoResponse;
use axum::extract::State;
use axum::response::Response;
use redis::AsyncCommands;

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
    pub last_insert_age_secs: Option<i64>,
    pub total_rows: i64,
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

// ---------- Refresh banners ----------

#[derive(Template)]
#[template(path = "_refresh_banner.html")]
pub struct RefreshBanner {
    pub success: bool,
    pub label: String,
    pub message: String,
    pub duration_ms: u128,
}

fn ok(label: &str, message: String, started: Instant) -> Response {
    RefreshBanner {
        success: true,
        label: label.to_string(),
        message,
        duration_ms: started.elapsed().as_millis(),
    }
    .into_response()
}

fn err(label: &str, message: String, started: Instant) -> Response {
    RefreshBanner {
        success: false,
        label: label.to_string(),
        message,
        duration_ms: started.elapsed().as_millis(),
    }
    .into_response()
}

pub async fn refresh_rollups(
    _user: AuthUser,
    State(state): State<AppState>,
) -> Response {
    let t = Instant::now();
    match state.store.refresh_rollups().await {
        Ok(()) => ok("Rollup", "project_daily refreshed".to_string(), t),
        Err(e) => err("Rollup", format!("{e}"), t),
    }
}

pub async fn refresh_relabel(
    _user: AuthUser,
    State(state): State<AppState>,
) -> Response {
    let t = Instant::now();
    match state.store.relabel_unknowns().await {
        Ok(n) => ok("Relabel", format!("{n} historical rows updated"), t),
        Err(e) => err("Relabel", format!("{e}"), t),
    }
}

pub async fn refresh_eth_price(
    _user: AuthUser,
    State(state): State<AppState>,
) -> Response {
    let t = Instant::now();
    match indexer_service::refresher::refresh_eth_price_now(&state.store, &state.price_url).await {
        Ok(price) => ok("ETH price", format!("stored ${price}"), t),
        Err(e) => err("ETH price", format!("{e}"), t),
    }
}

pub async fn refresh_resolver(
    _user: AuthUser,
    State(state): State<AppState>,
) -> Response {
    let t = Instant::now();
    let overlay = if state.overlay_path.exists() {
        Some(state.overlay_path.as_path())
    } else {
        None
    };
    let defillama = if state.defillama_url.is_empty() {
        None
    } else {
        Some(state.defillama_url.as_str())
    };
    let outcome = indexer_service::refresher::refresh_resolver_with(
        &state.resolver,
        &state.store,
        overlay,
        defillama,
    )
    .await;
    ok(
        "Resolver",
        format!(
            "{} projects, {} addresses, {} relabeled",
            outcome.projects, outcome.addresses, outcome.relabeled
        ),
        t,
    )
}

pub async fn refresh_labeler(
    _user: AuthUser,
    State(state): State<AppState>,
) -> Response {
    let t = Instant::now();
    let mut conn = state.redis.clone();
    match indexer_service::labeler::producer_tick_once(
        &state.store,
        &mut conn,
        state.chain_id as u64,
        state.labeler_batch_size,
        state.labeler_retry_days,
    )
    .await
    {
        Ok((pushed, depth)) => ok(
            "Labeler queue",
            format!("{pushed} new, {depth} total in queue"),
            t,
        ),
        Err(e) => err("Labeler queue", format!("{e}"), t),
    }
}

async fn collect_health(state: &AppState) -> Result<HealthView, WebError> {
    let chain_id = state.chain_id;
    let pool = state.store.pool();

    let analysis = queries::analysis_health(pool, chain_id).await?;

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
        last_insert_age_secs: analysis.last_insert_age_secs.map(|s| s as i64),
        total_rows: analysis.total_rows.unwrap_or(0),
    })
}
