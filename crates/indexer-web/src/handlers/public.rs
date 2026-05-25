//! Public BD-facing pages and the login flow.

use askama::Template;
use askama_axum::IntoResponse;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Redirect, Response};
use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tower_cookies::Cookies;

use crate::AppState;
use crate::auth::{AuthUser, clear_cookie, session_cookie};
use crate::error::WebError;
use crate::queries;

// ---------- Login / logout ----------

#[derive(Template)]
#[template(path = "login.html")]
pub struct LoginPage {
    pub error: Option<String>,
    pub next: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    pub next: Option<String>,
}

pub async fn login_get(Query(q): Query<LoginQuery>) -> Response {
    LoginPage {
        error: None,
        next: q.next.unwrap_or_else(|| "/".to_string()),
    }
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct LoginForm {
    pub username: String,
    pub password: String,
    pub next: Option<String>,
}

pub async fn login_post(
    State(state): State<AppState>,
    cookies: Cookies,
    Form(form): Form<LoginForm>,
) -> Response {
    match state.auth.verify_password(&form.username, &form.password) {
        Some(username) => {
            let token = state.auth.issue_token(&username);
            cookies.add(session_cookie(token));
            let target = sanitize_next(form.next.as_deref());
            Redirect::to(&target).into_response()
        }
        None => (
            StatusCode::UNAUTHORIZED,
            LoginPage {
                error: Some("Invalid username or password.".to_string()),
                next: form.next.unwrap_or_else(|| "/".to_string()),
            },
        )
            .into_response(),
    }
}

pub async fn logout(cookies: Cookies) -> Response {
    cookies.add(clear_cookie());
    Redirect::to("/login").into_response()
}

/// Avoid open-redirect: only allow paths starting with `/` and not `//`.
fn sanitize_next(next: Option<&str>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") => n.to_string(),
        _ => "/".to_string(),
    }
}

// ---------- Overview ----------

#[derive(Template)]
#[template(path = "overview.html")]
pub struct OverviewPage {
    pub user: String,
    pub chain_id: i64,
    pub totals_lifetime: TotalsView,
    pub totals_30d: TotalsView,
    pub totals_7d: TotalsView,
    pub totals_24h: TotalsView,
    /// USD/ETH extrapolations from the 30d window. "Monthly" is the 30d
    /// total verbatim (a 30-day window IS a month); "yearly" is the 30d
    /// total scaled by 365/30. Both are shown as separate cards so BD
    /// doesn't have to do the multiplication on a call.
    pub projection: ProjectionView,
    /// Active leaderboard pivot ("functions" / "contracts" / "projects").
    /// Template branches off this; only the matching Vec is populated.
    pub group: String,
    pub explorer_address_url: String,
    pub functions: Vec<FunctionLeaderView>,
    pub contracts: Vec<ContractLeaderView>,
    pub orgs: Vec<OrgLeaderView>,
    pub leaderboard: Vec<LeaderRow>,
    pub daily_30d: Vec<DailyView>,
}

#[derive(Debug, Clone)]
pub struct OrgLeaderView {
    pub org_slug: String,
    pub org_name: String,
    pub project_count: i64,
    pub contract_count: i64,
    pub tx_count: i64,
    pub usd_saved: String,
    pub avg_savings_pct_pct: String,
    pub savings_of_spend_pct: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct OverviewQuery {
    /// "functions" (default) | "contracts" | "projects"
    pub group: Option<String>,
}

/// Pagination + filters for the recent-txs tables on project / contract /
/// function drilldowns. `page` is 1-indexed. Filters default to "no
/// narrowing" — empty form = all txs (still excluding 0% and opcode-skipped).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TxQuery {
    pub page: Option<u32>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Minimum gas saved per tx — accepts plain integer (e.g. `10000`).
    pub min_gas_saved: Option<i64>,
}

/// Wraps a parsed `TxQuery` into per-handler state — current page, filters
/// to pass to the query, plus the rendered "next page available?" hint
/// derived from rows-returned.
#[derive(Debug, Clone)]
pub struct TxPagingView {
    pub page: u32,
    pub page_size: u32,
    pub has_prev: bool,
    pub has_next: bool,
    pub from: String,
    pub to: String,
    pub min_gas_saved: String,
    /// Pre-composed query-string suffix (`page=2&from=...&to=...`) for
    /// prev/next links so templates don't have to assemble it.
    pub prev_qs: String,
    pub next_qs: String,
}

const TX_PAGE_SIZE: u32 = 50;

fn build_tx_filters(q: &TxQuery) -> queries::TxFilters {
    queries::TxFilters {
        from: q.from.as_deref().and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()),
        to:   q.to.as_deref().and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()),
        min_gas_saved: q.min_gas_saved,
    }
}

fn paging_view(q: &TxQuery, rows_returned: usize) -> TxPagingView {
    let page = q.page.unwrap_or(1).max(1);
    let mut params: Vec<(String, String)> = Vec::new();
    if let Some(from) = q.from.as_deref().filter(|s| !s.is_empty()) {
        params.push(("from".into(), from.into()));
    }
    if let Some(to) = q.to.as_deref().filter(|s| !s.is_empty()) {
        params.push(("to".into(), to.into()));
    }
    if let Some(m) = q.min_gas_saved {
        params.push(("min_gas_saved".into(), m.to_string()));
    }
    let qs = |p: u32| {
        let mut all: Vec<(String, String)> = params.clone();
        all.push(("page".into(), p.to_string()));
        all.into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&")
    };
    TxPagingView {
        page,
        page_size: TX_PAGE_SIZE,
        has_prev: page > 1,
        has_next: rows_returned as u32 >= TX_PAGE_SIZE,
        from: q.from.clone().unwrap_or_default(),
        to: q.to.clone().unwrap_or_default(),
        min_gas_saved: q.min_gas_saved.map(|n| n.to_string()).unwrap_or_default(),
        prev_qs: qs(page.saturating_sub(1).max(1)),
        next_qs: qs(page + 1),
    }
}

#[derive(Debug, Clone)]
pub struct FunctionLeaderView {
    pub address_hex: String,
    pub selector_hex: String,
    /// Resolved name or empty if 4byte hasn't fetched it yet. Templates
    /// display this prominently when present and fall back to the raw
    /// selector otherwise.
    pub function_name: String,
    pub function_sig: String,
    pub project_slug: String,
    pub project_display: String,
    pub tx_count: i64,
    pub usd_saved: String,
    pub avg_savings_pct_pct: String,
    pub median_savings_pct_pct: String,
    pub savings_of_spend_pct: String,
}

#[derive(Debug, Clone)]
pub struct ContractLeaderView {
    pub address_hex: String,
    pub project_slug: String,
    pub project_display: String,
    pub tx_count: i64,
    pub function_count: i64,
    pub usd_saved: String,
    pub avg_savings_pct_pct: String,
    pub median_savings_pct_pct: String,
    pub savings_of_spend_pct: String,
    /// Only present when project_slug is unknown — gives the template
    /// an address to wire the rename ✎ icon to.
    pub unknown_address_hex: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TotalsView {
    pub usd_saved: String,
    pub eth_saved: String,
    pub tx_count: i64,
    pub project_count: i64,
}

#[derive(Debug, Clone)]
pub struct ProjectionView {
    pub monthly_usd: String,
    pub monthly_eth: String,
    pub yearly_usd: String,
    pub yearly_eth: String,
}

#[derive(Debug, Clone)]
pub struct LeaderRow {
    pub project_slug: String,
    pub display_name: String,
    pub tx_count: i64,
    pub avg_savings_pct_pct: String,
    /// % of total gas spend the project would have saved over the window.
    /// `wei_saved / wei_spent` — the headline BD metric.
    pub savings_of_spend_pct: String,
    /// % of the project's txs where gas-killer produced any savings.
    pub coverage_pct: String,
    /// Median (gas_saved / gas_used) over covered txs — robust to the
    /// many-zero-savings-txs skew that breaks the plain average.
    pub median_savings_pct_pct: String,
    /// Populated only when `project_slug` is `unknown:0xADDR`. Lets the
    /// template render a pencil-icon override link for that single
    /// address. None for real project slugs (which can span N addresses).
    pub unknown_address_hex: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DailyView {
    pub day: String,
    pub usd_saved: f64,
    pub tx_count: i64,
}

pub async fn overview(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Query(q): Query<OverviewQuery>,
) -> Result<Response, WebError> {
    let pool = state.store.pool();
    let chain_id = state.chain_id;

    let group = match q.group.as_deref() {
        Some("contracts") => "contracts",
        Some("projects") => "projects",
        Some("orgs") => "orgs",
        _ => "functions",
    }
    .to_string();

    let totals_lifetime_raw = queries::overview_totals(pool, chain_id, queries::Window::Lifetime).await?;
    let totals_30d_raw = queries::overview_totals(pool, chain_id, queries::Window::Days(30)).await?;
    let totals_7d_raw = queries::overview_totals(pool, chain_id, queries::Window::Days(7)).await?;
    let totals_24h_raw = queries::overview_totals(pool, chain_id, queries::Window::Days(1)).await?;

    let usd_30d = bd_to_f64(totals_30d_raw.usd_saved.as_ref());
    let eth_30d = bd_to_f64(totals_30d_raw.wei_saved.as_ref()) / 1e18;
    let projection = ProjectionView {
        monthly_usd: format_usd_value(usd_30d),
        monthly_eth: format_eth_value(eth_30d),
        yearly_usd: format_usd_value(usd_30d * (365.0 / 30.0)),
        yearly_eth: format_eth_value(eth_30d * (365.0 / 30.0)),
    };

    let totals_lifetime = totals_view(totals_lifetime_raw);
    let totals_30d = totals_view(totals_30d_raw);
    let totals_7d = totals_view(totals_7d_raw);
    let totals_24h = totals_view(totals_24h_raw);

    // Only fetch the leaderboard for the active pivot — the other
    // Vecs stay empty and the template branches off `group`.
    let mut functions: Vec<FunctionLeaderView> = Vec::new();
    let mut contracts: Vec<ContractLeaderView> = Vec::new();
    let mut orgs: Vec<OrgLeaderView> = Vec::new();
    let mut leaderboard: Vec<LeaderRow> = Vec::new();
    match group.as_str() {
        "functions" => {
            functions = queries::leaderboard_functions_30d(pool, chain_id, 50, 0)
                .await?
                .into_iter()
                .map(|r| {
                    let savings_of_spend = ratio_bd(
                        r.wei_saved_total.as_ref(),
                        r.wei_spent_total.as_ref(),
                    );
                    let project_display = r
                        .project_name
                        .clone()
                        .unwrap_or_else(|| r.project_slug.clone());
                    FunctionLeaderView {
                        address_hex: format!("0x{}", hex::encode(&r.to_address)),
                        selector_hex: format!("0x{}", hex::encode(&r.function_selector)),
                        function_name: r.function_name.unwrap_or_default(),
                        function_sig: r.function_sig.unwrap_or_default(),
                        project_slug: r.project_slug,
                        project_display,
                        tx_count: r.tx_count.unwrap_or(0),
                        usd_saved: format_usd(r.usd_saved.as_ref()),
                        avg_savings_pct_pct: format_pct(r.avg_savings_pct),
                        median_savings_pct_pct: format_pct(r.median_savings_pct),
                        savings_of_spend_pct: format_pct(savings_of_spend),
                    }
                })
                .collect();
        }
        "orgs" => {
            orgs = queries::leaderboard_orgs_30d(pool, chain_id, 50, 0)
                .await?
                .into_iter()
                .map(|r| {
                    let savings_of_spend = ratio_bd(
                        r.wei_saved_total.as_ref(),
                        r.wei_spent_total.as_ref(),
                    );
                    OrgLeaderView {
                        org_slug: r.org_slug,
                        org_name: r.org_name,
                        project_count: r.project_count.unwrap_or(0),
                        contract_count: r.contract_count.unwrap_or(0),
                        tx_count: r.tx_count.unwrap_or(0),
                        usd_saved: format_usd(r.usd_saved.as_ref()),
                        avg_savings_pct_pct: format_pct(r.avg_savings_pct),
                        savings_of_spend_pct: format_pct(savings_of_spend),
                    }
                })
                .collect();
        }
        "contracts" => {
            contracts = queries::leaderboard_contracts_30d(pool, chain_id, 50, 0)
                .await?
                .into_iter()
                .map(|r| {
                    let unknown_address_hex = r
                        .project_slug
                        .strip_prefix("unknown:0x")
                        .map(|hex| format!("0x{hex}"));
                    let savings_of_spend = ratio_bd(
                        r.wei_saved_total.as_ref(),
                        r.wei_spent_total.as_ref(),
                    );
                    let project_display = r
                        .project_name
                        .clone()
                        .unwrap_or_else(|| r.project_slug.clone());
                    ContractLeaderView {
                        address_hex: format!("0x{}", hex::encode(&r.to_address)),
                        project_slug: r.project_slug,
                        project_display,
                        tx_count: r.tx_count.unwrap_or(0),
                        function_count: r.function_count.unwrap_or(0),
                        usd_saved: format_usd(r.usd_saved.as_ref()),
                        avg_savings_pct_pct: format_pct(r.avg_savings_pct),
                        median_savings_pct_pct: format_pct(r.median_savings_pct),
                        savings_of_spend_pct: format_pct(savings_of_spend),
                        unknown_address_hex,
                    }
                })
                .collect();
        }
        _ => {
            leaderboard = queries::leaderboard_30d(pool, chain_id, 50, 0)
                .await?
                .into_iter()
                .map(|r| {
                    let unknown_address_hex = r
                        .project_slug
                        .strip_prefix("unknown:0x")
                        .map(|hex| format!("0x{hex}"));
                    let tx_count = r.tx_count.unwrap_or(0);
                    let covered = r.covered_tx_count.unwrap_or(0);
                    let coverage = if tx_count > 0 {
                        Some(covered as f64 / tx_count as f64)
                    } else {
                        None
                    };
                    let savings_of_spend = ratio_bd(
                        r.wei_saved_total.as_ref(),
                        r.wei_spent_total.as_ref(),
                    );
                    LeaderRow {
                        display_name: r
                            .project_name
                            .clone()
                            .unwrap_or_else(|| r.project_slug.clone()),
                        tx_count,
                        avg_savings_pct_pct: format_pct(r.avg_savings_pct),
                        savings_of_spend_pct: format_pct(savings_of_spend),
                        coverage_pct: format_pct(coverage),
                        median_savings_pct_pct: format_pct(r.median_savings_pct_covered),
                        project_slug: r.project_slug,
                        unknown_address_hex,
                    }
                })
                .collect();
        }
    }

    let daily = queries::daily_overview(pool, chain_id, 30).await?;
    let daily_30d = daily
        .into_iter()
        .map(|p| DailyView {
            day: p.day.format("%Y-%m-%d").to_string(),
            usd_saved: bd_to_f64(p.usd_saved.as_ref()),
            tx_count: p.tx_count.unwrap_or(0),
        })
        .collect();

    let page = OverviewPage {
        user,
        chain_id,
        totals_lifetime,
        totals_30d,
        totals_7d,
        totals_24h,
        projection,
        group,
        explorer_address_url: state.explorer_address_url.as_str().to_string(),
        functions,
        contracts,
        orgs,
        leaderboard,
        daily_30d,
    };
    Ok(page.into_response())
}

// ---------- Project drill-down ----------

#[derive(Template)]
#[template(path = "project.html")]
pub struct ProjectPage {
    pub user: String,
    pub chain_id: i64,
    pub explorer_tx_url: String,
    pub explorer_address_url: String,
    pub project_slug: String,
    pub project_name: String,
    pub category: String,
    pub contact_email: String,
    pub contact_url: String,
    pub totals_lifetime: TotalsView,
    pub totals_30d: TotalsView,
    pub avg_savings_pct_30d: String,
    pub daily_90d: Vec<DailyView>,
    pub top_contracts: Vec<ContractRowView>,
    pub top_selectors: Vec<SelectorRowView>,
    pub recent_txs: Vec<RecentTxView>,
    pub paging: TxPagingView,
}

#[derive(Debug, Clone)]
pub struct ContractRowView {
    pub address_hex: String,
    pub tx_count: i64,
    pub gas_saved_total: i64,
    pub wei_saved_total: String,
}

#[derive(Debug, Clone)]
pub struct SelectorRowView {
    pub selector_hex: String,
    pub tx_count: i64,
    pub gas_saved_total: i64,
    pub wei_saved_total: String,
}

#[derive(Debug, Clone)]
pub struct RecentTxView {
    pub block_number: i64,
    pub tx_hash_hex: String,
    pub gas_used: i64,
    pub gas_saved: i64,
    pub wei_saved: String,
    pub when: String,
}

pub async fn project(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Query(tx_q): Query<TxQuery>,
) -> Result<Response, WebError> {
    let pool = state.store.pool();
    let chain_id = state.chain_id;

    let header = queries::project_header(pool, &slug).await?;
    let totals_lifetime_raw = queries::project_totals(pool, chain_id, &slug, queries::Window::Lifetime).await?;
    let totals_30d_raw = queries::project_totals(pool, chain_id, &slug, queries::Window::Days(30)).await?;

    let totals_lifetime = TotalsView {
        usd_saved: format_usd(totals_lifetime_raw.usd_saved.as_ref()),
        eth_saved: format_eth(totals_lifetime_raw.wei_saved.as_ref()),
        tx_count: totals_lifetime_raw.tx_count.unwrap_or(0),
        project_count: 0,
    };
    let totals_30d = TotalsView {
        usd_saved: format_usd(totals_30d_raw.usd_saved.as_ref()),
        eth_saved: format_eth(totals_30d_raw.wei_saved.as_ref()),
        tx_count: totals_30d_raw.tx_count.unwrap_or(0),
        project_count: 0,
    };
    let avg_savings_pct_30d = format_pct(totals_30d_raw.avg_savings_pct);

    let daily = queries::daily_for_project(pool, chain_id, &slug, 90).await?;
    let daily_90d = daily
        .into_iter()
        .map(|p| DailyView {
            day: p.day.format("%Y-%m-%d").to_string(),
            usd_saved: bd_to_f64(p.usd_saved.as_ref()),
            tx_count: p.tx_count.unwrap_or(0),
        })
        .collect();

    let top_contracts = queries::top_contracts_for_project(pool, chain_id, &slug)
        .await?
        .into_iter()
        .map(|r| ContractRowView {
            address_hex: format!("0x{}", hex::encode(&r.address)),
            tx_count: r.tx_count.unwrap_or(0),
            gas_saved_total: r.gas_saved_total.unwrap_or(0),
            wei_saved_total: format_eth(r.wei_saved_total.as_ref()),
        })
        .collect();

    let top_selectors = queries::top_selectors_for_project(pool, chain_id, &slug)
        .await?
        .into_iter()
        .map(|r| SelectorRowView {
            selector_hex: format!("0x{}", hex::encode(&r.function_selector)),
            tx_count: r.tx_count.unwrap_or(0),
            gas_saved_total: r.gas_saved_total.unwrap_or(0),
            wei_saved_total: format_eth(r.wei_saved_total.as_ref()),
        })
        .collect();

    let filters = build_tx_filters(&tx_q);
    let page = tx_q.page.unwrap_or(1).max(1);
    let offset = (page as i64 - 1) * TX_PAGE_SIZE as i64;
    let recent_rows = queries::recent_txs_for_project(
        pool, chain_id, &slug, TX_PAGE_SIZE as i64, offset, &filters,
    )
    .await?;
    let paging = paging_view(&tx_q, recent_rows.len());
    let recent = recent_rows
        .into_iter()
        .map(|r| RecentTxView {
            block_number: r.block_number,
            tx_hash_hex: format!("0x{}", hex::encode(&r.tx_hash)),
            gas_used: r.gas_used,
            gas_saved: r.gas_saved,
            wei_saved: format_eth(Some(&r.wei_saved)),
            when: format_when(r.block_timestamp),
        })
        .collect();

    let header = header.unwrap_or(queries::ProjectHeader {
        project_slug: slug.clone(),
        project_name: None,
        category: None,
        contact_email: None,
        contact_url: None,
    });

    let page = ProjectPage {
        user,
        chain_id,
        explorer_tx_url: state.explorer_tx_url.as_str().to_string(),
        explorer_address_url: state.explorer_address_url.as_str().to_string(),
        project_name: header
            .project_name
            .clone()
            .unwrap_or_else(|| header.project_slug.clone()),
        category: header.category.unwrap_or_else(|| "—".to_string()),
        contact_email: header.contact_email.unwrap_or_default(),
        contact_url: header.contact_url.unwrap_or_default(),
        project_slug: header.project_slug,
        totals_lifetime,
        totals_30d,
        avg_savings_pct_30d,
        daily_90d,
        top_contracts,
        top_selectors,
        recent_txs: recent,
        paging,
    };
    Ok(page.into_response())
}

// ---------- Per-contract drilldown ----------

#[derive(Template)]
#[template(path = "contract.html")]
pub struct ContractPage {
    pub user: String,
    pub chain_id: i64,
    pub explorer_tx_url: String,
    pub explorer_address_url: String,
    pub address_hex: String,
    pub project_slug: String,
    pub project_display: String,
    pub totals_lifetime: TotalsView,
    pub totals_30d: TotalsView,
    pub function_count: i64,
    pub avg_savings_pct_30d: String,
    pub daily_90d: Vec<DailyView>,
    pub functions: Vec<ContractFunctionView>,
    pub recent_txs: Vec<RecentTxView>,
    pub paging: TxPagingView,
}

#[derive(Debug, Clone)]
pub struct ContractFunctionView {
    pub selector_hex: String,
    pub function_name: String,
    pub tx_count: i64,
    pub usd_saved: String,
    pub avg_savings_pct_pct: String,
}

pub async fn contract_page(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path(address_hex): Path<String>,
    Query(tx_q): Query<TxQuery>,
) -> Result<Response, WebError> {
    let pool = state.store.pool();
    let chain_id = state.chain_id;

    let address = parse_address(&address_hex)
        .ok_or_else(|| WebError::BadRequest("invalid address".into()))?;

    let header = queries::contract_header(pool, chain_id, address).await?;
    let totals_lifetime_raw = queries::contract_totals(pool, chain_id, address, queries::Window::Lifetime).await?;
    let totals_30d_raw = queries::contract_totals(pool, chain_id, address, queries::Window::Days(30)).await?;

    let totals_lifetime = TotalsView {
        usd_saved: format_usd(totals_lifetime_raw.usd_saved.as_ref()),
        eth_saved: format_eth(totals_lifetime_raw.wei_saved.as_ref()),
        tx_count: totals_lifetime_raw.tx_count.unwrap_or(0),
        project_count: 0,
    };
    let totals_30d = TotalsView {
        usd_saved: format_usd(totals_30d_raw.usd_saved.as_ref()),
        eth_saved: format_eth(totals_30d_raw.wei_saved.as_ref()),
        tx_count: totals_30d_raw.tx_count.unwrap_or(0),
        project_count: 0,
    };
    let function_count = totals_30d_raw.function_count.unwrap_or(0);
    let avg_savings_pct_30d = format_pct(totals_30d_raw.avg_savings_pct);

    let daily_90d = queries::daily_for_contract(pool, chain_id, address, 90)
        .await?
        .into_iter()
        .map(|p| DailyView {
            day: p.day.format("%Y-%m-%d").to_string(),
            usd_saved: bd_to_f64(p.usd_saved.as_ref()),
            tx_count: p.tx_count.unwrap_or(0),
        })
        .collect();

    let functions = queries::functions_for_contract(pool, chain_id, address)
        .await?
        .into_iter()
        .map(|r| ContractFunctionView {
            selector_hex: format!("0x{}", hex::encode(&r.function_selector)),
            function_name: r.function_name.unwrap_or_default(),
            tx_count: r.tx_count.unwrap_or(0),
            usd_saved: format_usd(r.usd_saved.as_ref()),
            avg_savings_pct_pct: format_pct(r.avg_savings_pct),
        })
        .collect();

    let filters = build_tx_filters(&tx_q);
    let cur_page = tx_q.page.unwrap_or(1).max(1);
    let offset = (cur_page as i64 - 1) * TX_PAGE_SIZE as i64;
    let recent_rows = queries::recent_txs_for_contract(
        pool, chain_id, address, TX_PAGE_SIZE as i64, offset, &filters,
    )
    .await?;
    let paging = paging_view(&tx_q, recent_rows.len());
    let recent_txs = recent_rows
        .into_iter()
        .map(|r| RecentTxView {
            block_number: r.block_number,
            tx_hash_hex: format!("0x{}", hex::encode(&r.tx_hash)),
            gas_used: r.gas_used,
            gas_saved: r.gas_saved,
            wei_saved: format_eth(Some(&r.wei_saved)),
            when: format_when(r.block_timestamp),
        })
        .collect();

    let page = ContractPage {
        user,
        chain_id,
        explorer_tx_url: state.explorer_tx_url.as_str().to_string(),
        explorer_address_url: state.explorer_address_url.as_str().to_string(),
        address_hex: format!("0x{}", hex::encode(address)),
        project_display: header
            .project_name
            .clone()
            .unwrap_or_else(|| header.project_slug.clone()),
        project_slug: header.project_slug,
        totals_lifetime,
        totals_30d,
        function_count,
        avg_savings_pct_30d,
        daily_90d,
        functions,
        recent_txs,
        paging,
    };
    Ok(page.into_response())
}

// ---------- Per-function drilldown ----------

#[derive(Template)]
#[template(path = "function.html")]
pub struct FunctionPage {
    pub user: String,
    pub chain_id: i64,
    pub explorer_tx_url: String,
    pub explorer_address_url: String,
    pub address_hex: String,
    pub selector_hex: String,
    pub function_name: String,
    pub function_sig: String,
    pub project_slug: String,
    pub project_display: String,
    pub totals_lifetime: TotalsView,
    pub totals_30d: TotalsView,
    pub avg_savings_pct_30d: String,
    pub daily_90d: Vec<DailyView>,
    pub recent_txs: Vec<RecentTxView>,
    pub paging: TxPagingView,
}

pub async fn function_page(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Path((address_hex, selector_hex)): Path<(String, String)>,
    Query(tx_q): Query<TxQuery>,
) -> Result<Response, WebError> {
    let pool = state.store.pool();
    let chain_id = state.chain_id;

    let address = parse_address(&address_hex)
        .ok_or_else(|| WebError::BadRequest("invalid address".into()))?;
    let selector = parse_selector(&selector_hex)
        .ok_or_else(|| WebError::BadRequest("invalid selector".into()))?;

    let header = queries::function_header(pool, chain_id, address, selector).await?;
    let totals_lifetime_raw = queries::function_totals(pool, chain_id, address, selector, queries::Window::Lifetime).await?;
    let totals_30d_raw = queries::function_totals(pool, chain_id, address, selector, queries::Window::Days(30)).await?;

    let totals_lifetime = TotalsView {
        usd_saved: format_usd(totals_lifetime_raw.usd_saved.as_ref()),
        eth_saved: format_eth(totals_lifetime_raw.wei_saved.as_ref()),
        tx_count: totals_lifetime_raw.tx_count.unwrap_or(0),
        project_count: 0,
    };
    let totals_30d = TotalsView {
        usd_saved: format_usd(totals_30d_raw.usd_saved.as_ref()),
        eth_saved: format_eth(totals_30d_raw.wei_saved.as_ref()),
        tx_count: totals_30d_raw.tx_count.unwrap_or(0),
        project_count: 0,
    };
    let avg_savings_pct_30d = format_pct(totals_30d_raw.avg_savings_pct);

    let daily_90d = queries::daily_for_function(pool, chain_id, address, selector, 90)
        .await?
        .into_iter()
        .map(|p| DailyView {
            day: p.day.format("%Y-%m-%d").to_string(),
            usd_saved: bd_to_f64(p.usd_saved.as_ref()),
            tx_count: p.tx_count.unwrap_or(0),
        })
        .collect();

    let filters = build_tx_filters(&tx_q);
    let cur_page = tx_q.page.unwrap_or(1).max(1);
    let offset = (cur_page as i64 - 1) * TX_PAGE_SIZE as i64;
    let recent_rows = queries::recent_txs_for_function(
        pool, chain_id, address, selector, TX_PAGE_SIZE as i64, offset, &filters,
    )
    .await?;
    let paging = paging_view(&tx_q, recent_rows.len());
    let recent_txs = recent_rows
        .into_iter()
        .map(|r| RecentTxView {
            block_number: r.block_number,
            tx_hash_hex: format!("0x{}", hex::encode(&r.tx_hash)),
            gas_used: r.gas_used,
            gas_saved: r.gas_saved,
            wei_saved: format_eth(Some(&r.wei_saved)),
            when: format_when(r.block_timestamp),
        })
        .collect();

    let page = FunctionPage {
        user,
        chain_id,
        explorer_tx_url: state.explorer_tx_url.as_str().to_string(),
        explorer_address_url: state.explorer_address_url.as_str().to_string(),
        address_hex: format!("0x{}", hex::encode(address)),
        selector_hex: format!("0x{}", hex::encode(selector)),
        function_name: header.function_name.unwrap_or_default(),
        function_sig: header.function_sig.unwrap_or_default(),
        project_display: header
            .project_name
            .clone()
            .unwrap_or_else(|| header.project_slug.clone()),
        project_slug: header.project_slug,
        totals_lifetime,
        totals_30d,
        avg_savings_pct_30d,
        daily_90d,
        recent_txs,
        paging,
    };
    Ok(page.into_response())
}

fn parse_address(s: &str) -> Option<[u8; 20]> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s).ok()?;
    if bytes.len() != 20 {
        return None;
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(&bytes);
    Some(out)
}

fn parse_selector(s: &str) -> Option<[u8; 4]> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s).ok()?;
    if bytes.len() != 4 {
        return None;
    }
    let mut out = [0u8; 4];
    out.copy_from_slice(&bytes);
    Some(out)
}

// ---------- Unknowns ----------

#[derive(Template)]
#[template(path = "unknowns.html")]
pub struct UnknownsPage {
    pub user: String,
    pub chain_id: i64,
    pub explorer_address_url: String,
    pub tab: String,
    pub contracts: Vec<UnknownContractView>,
    pub selectors: Vec<UnresolvedSelectorView>,
}

#[derive(Debug, Clone)]
pub struct UnknownContractView {
    pub address_hex: String,
    pub tx_count: i64,
    pub wei_saved: String,
    pub last_seen: String,
}

#[derive(Debug, Clone)]
pub struct UnresolvedSelectorView {
    pub selector_hex: String,
    pub example_address_hex: String,
    pub tx_count: i64,
    pub wei_saved: String,
    pub usd_saved: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct UnknownsQuery {
    /// "contracts" (default) | "selectors"
    pub tab: Option<String>,
}

pub async fn unknowns(
    AuthUser(user): AuthUser,
    State(state): State<AppState>,
    Query(q): Query<UnknownsQuery>,
) -> Result<Response, WebError> {
    let pool = state.store.pool();
    let tab = match q.tab.as_deref() {
        Some("selectors") => "selectors",
        _ => "contracts",
    }
    .to_string();

    let mut contracts: Vec<UnknownContractView> = Vec::new();
    let mut selectors: Vec<UnresolvedSelectorView> = Vec::new();
    match tab.as_str() {
        "selectors" => {
            selectors = queries::top_unresolved_selectors(pool, state.chain_id, 50)
                .await?
                .into_iter()
                .map(|r| UnresolvedSelectorView {
                    selector_hex: format!("0x{}", hex::encode(&r.selector)),
                    example_address_hex: format!("0x{}", hex::encode(&r.example_address)),
                    tx_count: r.tx_count.unwrap_or(0),
                    wei_saved: format_eth(r.wei_saved_total.as_ref()),
                    usd_saved: format_usd(r.usd_saved_total.as_ref()),
                })
                .collect();
        }
        _ => {
            contracts = queries::top_unknowns(pool, state.chain_id)
                .await?
                .into_iter()
                .map(|r| UnknownContractView {
                    address_hex: format!("0x{}", hex::encode(&r.address)),
                    tx_count: r.tx_count.unwrap_or(0),
                    wei_saved: format_eth(r.wei_saved_total.as_ref()),
                    last_seen: r
                        .last_seen
                        .map(format_when)
                        .unwrap_or_else(|| "—".to_string()),
                })
                .collect();
        }
    }

    Ok(UnknownsPage {
        user,
        chain_id: state.chain_id,
        explorer_address_url: state.explorer_address_url.as_str().to_string(),
        tab,
        contracts,
        selectors,
    }
    .into_response())
}

// ---------- helpers ----------

fn totals_view(t: queries::OverviewTotals) -> TotalsView {
    TotalsView {
        usd_saved: format_usd(t.usd_saved.as_ref()),
        eth_saved: format_eth(t.wei_saved.as_ref()),
        tx_count: t.tx_count.unwrap_or(0),
        project_count: t.project_count.unwrap_or(0),
    }
}

fn bd_to_f64(b: Option<&BigDecimal>) -> f64 {
    use std::str::FromStr;
    b.map(|x| f64::from_str(&x.to_string()).unwrap_or(0.0))
        .unwrap_or(0.0)
}

fn format_usd(b: Option<&BigDecimal>) -> String {
    format_usd_value(bd_to_f64(b))
}

fn format_usd_value(v: f64) -> String {
    if v >= 1_000_000.0 {
        format!("${:.2}M", v / 1_000_000.0)
    } else if v >= 1_000.0 {
        format!("${:.2}K", v / 1_000.0)
    } else {
        format!("${:.2}", v)
    }
}

fn format_eth(b: Option<&BigDecimal>) -> String {
    format_eth_value(bd_to_f64(b) / 1e18)
}

fn format_eth_value(v: f64) -> String {
    if v == 0.0 {
        "0 ETH".to_string()
    } else if v < 0.0001 {
        format!("{:.6e} ETH", v)
    } else if v < 1.0 {
        format!("{:.6} ETH", v)
    } else {
        format!("{:.4} ETH", v)
    }
}

fn format_pct(v: Option<f64>) -> String {
    match v {
        Some(x) => format!("{:.1}%", x * 100.0),
        None => "—".to_string(),
    }
}

/// Divide two BigDecimals as f64, returning None on missing/zero denom.
/// Used for the savings-to-gas-spend ratio where both sides are wei sums.
fn ratio_bd(num: Option<&BigDecimal>, denom: Option<&BigDecimal>) -> Option<f64> {
    let n = bd_to_f64(num);
    let d = bd_to_f64(denom);
    if d == 0.0 { None } else { Some(n / d) }
}

fn format_when(ts: DateTime<Utc>) -> String {
    let now = Utc::now();
    let delta = now - ts;
    let secs = delta.num_seconds();
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        ts.format("%Y-%m-%d %H:%M UTC").to_string()
    }
}
