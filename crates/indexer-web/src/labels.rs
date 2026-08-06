//! Label-override endpoints. Lets a signed-in operator manually set the
//! `project_slug` for any (chain_id, address). Writes are sticky — the
//! `manual_override` flag on `address_project` keeps subsequent resolver
//! and auto-labeler upserts from clobbering the override.
//!
//! Three endpoints, all htmx-friendly fragments:
//!   GET  /api/labels/cell  — read-only "Project · [✎]" cell
//!   GET  /api/labels/edit  — inline edit form
//!   POST /api/labels/override — apply the change

use askama::Template;
use askama_axum::IntoResponse;
use axum::extract::{Query, State};
use axum::response::Response;
use indexer_store::Project;
use serde::Deserialize;

use crate::AppState;
use crate::auth::AuthUser;
use crate::error::WebError;
use crate::queries;

#[derive(Debug, Deserialize)]
pub struct CellQuery {
    pub address: String,
    #[serde(default)]
    pub chain_id: Option<i64>,
}

#[derive(Template)]
#[template(path = "_label_cell.html")]
pub struct LabelCell {
    pub chain_id: i64,
    pub address_hex: String,
    pub display_name: String,
    pub project_slug: String,
    pub is_unknown: bool,
    pub is_manual: bool,
}

#[derive(Template)]
#[template(path = "_label_edit.html")]
pub struct LabelEdit {
    pub chain_id: i64,
    pub address_hex: String,
    pub current_slug: String,
}

pub async fn label_cell(
    _user: AuthUser,
    State(state): State<AppState>,
    Query(q): Query<CellQuery>,
) -> Result<Response, WebError> {
    let chain_id = q.chain_id.unwrap_or(state.chain_id);
    let addr = parse_addr(&q.address)?;
    let cell = build_cell(&state, chain_id, addr).await?;
    Ok(cell.into_response())
}

pub async fn label_edit(
    _user: AuthUser,
    State(state): State<AppState>,
    Query(q): Query<CellQuery>,
) -> Result<Response, WebError> {
    let chain_id = q.chain_id.unwrap_or(state.chain_id);
    let addr = parse_addr(&q.address)?;
    let current = current_slug(&state, chain_id, addr).await?;
    let edit = LabelEdit {
        chain_id,
        address_hex: format!("0x{}", hex::encode(addr)),
        current_slug: current,
    };
    Ok(edit.into_response())
}

#[derive(Debug, Deserialize)]
pub struct OverrideForm {
    pub address: String,
    pub chain_id: i64,
    pub slug: String,
    #[serde(default)]
    pub project_name: String,
}

pub async fn label_override(
    _user: AuthUser,
    State(state): State<AppState>,
    axum::Form(form): axum::Form<OverrideForm>,
) -> Result<Response, WebError> {
    let addr = parse_addr(&form.address)?;
    let slug = validate_slug(&form.slug)?;
    let display_name = if form.project_name.trim().is_empty() {
        slug.to_string()
    } else {
        form.project_name.trim().to_string()
    };

    // Ensure the projects row exists so the FK on address_project resolves.
    state
        .store
        .upsert_project(&Project {
            slug: slug.to_string(),
            name: display_name,
            category: None,
            contact_email: None,
            contact_url: None,
        })
        .await?;
    state
        .store
        .upsert_manual_address_project(form.chain_id as u64, addr, slug)
        .await?;
    // Retro-fix every historical analysis row for this address...
    let _ = state.store.relabel_unknowns().await?;
    // ...and refresh the materialized view so the overview leaderboard
    // groups by the new slug immediately. Without this the dashboard would
    // keep showing the row under `unknown:0xADDR` until the next rollup
    // tick (default 1h). Cheap on this DB size.
    if let Err(e) = state.store.refresh_rollups().await {
        tracing::warn!(error = %e, "refresh_rollups after override failed");
    }

    let cell = build_cell(&state, form.chain_id, addr).await?;
    Ok(cell.into_response())
}

async fn build_cell(
    state: &AppState,
    chain_id: i64,
    addr: [u8; 20],
) -> Result<LabelCell, WebError> {
    let resolved = queries::resolved_label(state.store.pool(), chain_id, addr).await?;
    let is_manual = state
        .store
        .is_manual_override(chain_id as u64, addr)
        .await
        .unwrap_or(false);
    let (slug, display) = match resolved {
        Some((s, name)) => {
            let display = name.unwrap_or_else(|| s.clone());
            (s, display)
        }
        None => {
            let synthetic = format!("unknown:0x{}", hex::encode(addr));
            (
                synthetic.clone(),
                format!("Unknown (0x{})", hex::encode(addr)),
            )
        }
    };
    Ok(LabelCell {
        chain_id,
        address_hex: format!("0x{}", hex::encode(addr)),
        display_name: display,
        is_unknown: slug.starts_with("unknown:"),
        project_slug: slug,
        is_manual,
    })
}

async fn current_slug(state: &AppState, chain_id: i64, addr: [u8; 20]) -> Result<String, WebError> {
    let resolved = queries::resolved_label(state.store.pool(), chain_id, addr).await?;
    Ok(match resolved {
        Some((s, _)) if !s.starts_with("unknown:") => s,
        _ => String::new(),
    })
}

fn parse_addr(s: &str) -> Result<[u8; 20], WebError> {
    let stripped = s.trim().strip_prefix("0x").unwrap_or(s.trim());
    let bytes =
        hex::decode(stripped).map_err(|e| WebError::BadRequest(format!("invalid hex: {e}")))?;
    if bytes.len() != 20 {
        return Err(WebError::BadRequest(format!(
            "address must be 20 bytes, got {}",
            bytes.len()
        )));
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(&bytes);
    Ok(out)
}

// ---------- Project rename (display-name only) ----------

#[derive(Debug, Deserialize)]
pub struct ProjectQuery {
    pub slug: String,
}

#[derive(Template)]
#[template(path = "_project_cell.html")]
pub struct ProjectCell {
    pub project_slug: String,
    pub display_name: String,
}

#[derive(Template)]
#[template(path = "_project_edit.html")]
pub struct ProjectEdit {
    pub project_slug: String,
    pub current_name: String,
}

pub async fn project_cell(
    _user: AuthUser,
    State(state): State<AppState>,
    Query(q): Query<ProjectQuery>,
) -> Result<Response, WebError> {
    let cell = build_project_cell(&state, &q.slug).await?;
    Ok(cell.into_response())
}

pub async fn project_edit(
    _user: AuthUser,
    State(state): State<AppState>,
    Query(q): Query<ProjectQuery>,
) -> Result<Response, WebError> {
    let row = queries::project_header(state.store.pool(), &q.slug).await?;
    let current_name = row
        .and_then(|h| h.project_name)
        .unwrap_or_else(|| q.slug.clone());
    Ok(ProjectEdit {
        project_slug: q.slug,
        current_name,
    }
    .into_response())
}

#[derive(Debug, Deserialize)]
pub struct RenameForm {
    pub slug: String,
    pub project_name: String,
}

pub async fn project_rename(
    _user: AuthUser,
    State(state): State<AppState>,
    axum::Form(form): axum::Form<RenameForm>,
) -> Result<Response, WebError> {
    let new_name = form.project_name.trim();
    if new_name.is_empty() || new_name.len() > 128 {
        return Err(WebError::BadRequest(
            "display name must be 1..=128 chars".to_string(),
        ));
    }
    if form.slug.is_empty() {
        return Err(WebError::BadRequest("slug is required".to_string()));
    }
    let renamed = state.store.rename_project(&form.slug, new_name).await?;
    if !renamed {
        return Err(WebError::NotFound);
    }
    let cell = build_project_cell(&state, &form.slug).await?;
    Ok(cell.into_response())
}

async fn build_project_cell(state: &AppState, slug: &str) -> Result<ProjectCell, WebError> {
    let header = queries::project_header(state.store.pool(), slug).await?;
    let display_name = header
        .and_then(|h| h.project_name)
        .unwrap_or_else(|| slug.to_string());
    Ok(ProjectCell {
        project_slug: slug.to_string(),
        display_name,
    })
}

/// Slugs follow DefiLlama's shape: lowercase, digits, dashes. Bounded
/// length. Cannot start with `unknown:` (defeats the purpose of an override).
fn validate_slug(s: &str) -> Result<&str, WebError> {
    let s = s.trim();
    if s.is_empty() || s.len() > 64 {
        return Err(WebError::BadRequest(
            "slug must be 1..=64 chars".to_string(),
        ));
    }
    if s.starts_with("unknown:") {
        return Err(WebError::BadRequest(
            "override cannot start with `unknown:`".to_string(),
        ));
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(WebError::BadRequest(
            "slug must match [a-z0-9-]+".to_string(),
        ));
    }
    Ok(s)
}
