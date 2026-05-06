use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum WebError {
    #[error("sqlx error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("redis error: {0}")]
    Redis(#[from] redis::RedisError),
    #[error("template render error: {0}")]
    Render(#[from] askama::Error),
    #[error("serde json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("not found")]
    NotFound,
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let (status, body) = match &self {
            WebError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            WebError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            other => {
                tracing::error!(error = %other, "web handler error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
        };
        (status, body).into_response()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("io error reading allowlist: {0}")]
    Io(#[from] std::io::Error),
    #[error("yaml parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("session secret missing or too short (need >=32 bytes)")]
    SecretInvalid,
}
