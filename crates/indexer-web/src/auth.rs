//! Static-allowlist auth.
//!
//! Users are loaded once from a YAML file at startup. Sessions are stateless
//! HMAC-signed cookies (no DB rows for sessions). Logout clears the cookie.

use std::path::Path;
use std::sync::Arc;

use axum::extract::{FromRef, FromRequestParts};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tower_cookies::Cookies;
use tower_cookies::cookie::SameSite;

use crate::error::AuthError;

pub const SESSION_COOKIE: &str = "ix_session";
pub const SESSION_TTL_SECS: i64 = 7 * 24 * 60 * 60;

#[derive(Debug, Clone, Deserialize)]
pub struct UserEntry {
    pub username: String,
    pub bcrypt_hash: String,
}

#[derive(Clone)]
pub struct AuthState {
    users: Arc<Vec<UserEntry>>,
    secret: Arc<Vec<u8>>,
}

impl AuthState {
    pub fn new(allowlist_path: &Path, secret: Vec<u8>) -> Result<Self, AuthError> {
        if secret.len() < 32 {
            return Err(AuthError::SecretInvalid);
        }
        let bytes = std::fs::read(allowlist_path)?;
        let users: Vec<UserEntry> = serde_yaml::from_slice(&bytes)?;
        tracing::info!(count = users.len(), path = ?allowlist_path, "loaded allowlist");
        Ok(Self {
            users: Arc::new(users),
            secret: Arc::new(secret),
        })
    }

    pub fn user_count(&self) -> usize {
        self.users.len()
    }

    /// Returns the matching username on success, or `None` if the credentials
    /// don't match. bcrypt is constant-time internally.
    pub fn verify_password(&self, username: &str, password: &str) -> Option<String> {
        let entry = self.users.iter().find(|u| u.username == username)?;
        match bcrypt::verify(password, &entry.bcrypt_hash) {
            Ok(true) => Some(entry.username.clone()),
            _ => None,
        }
    }

    /// Build a fresh signed session token for `username`, valid for
    /// [`SESSION_TTL_SECS`]. The token is `username|expires|hmac`,
    /// base64url-encoded.
    pub fn issue_token(&self, username: &str) -> String {
        let expires = chrono::Utc::now().timestamp() + SESSION_TTL_SECS;
        let payload = format!("{username}|{expires}");
        let sig = self.sign(&payload);
        let token_raw = format!("{payload}|{sig}");
        URL_SAFE_NO_PAD.encode(token_raw.as_bytes())
    }

    /// Verify a token previously issued by [`issue_token`]. Returns the
    /// username if the signature is valid and the expiry has not passed.
    pub fn verify_token(&self, token: &str) -> Option<String> {
        let raw = URL_SAFE_NO_PAD.decode(token.as_bytes()).ok()?;
        let raw = std::str::from_utf8(&raw).ok()?;
        let mut parts = raw.rsplitn(2, '|');
        let sig = parts.next()?;
        let payload = parts.next()?;
        let expected = self.sign(payload);
        if expected.as_bytes().ct_eq(sig.as_bytes()).unwrap_u8() != 1 {
            return None;
        }
        let (username, expires) = payload.rsplit_once('|')?;
        let expires: i64 = expires.parse().ok()?;
        if chrono::Utc::now().timestamp() > expires {
            return None;
        }
        Some(username.to_string())
    }

    fn sign(&self, payload: &str) -> String {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.secret)
            .expect("HMAC accepts any key length");
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
}

/// Authenticated user, populated by the extractor from the session cookie.
#[derive(Debug, Clone)]
pub struct AuthUser(pub String);

#[axum::async_trait]
impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
    AuthState: axum::extract::FromRef<S>,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let auth = AuthState::from_ref(state);
        let cookies = Cookies::from_request_parts(parts, state)
            .await
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "cookies layer missing").into_response())?;
        let token = cookies
            .get(SESSION_COOKIE)
            .map(|c| c.value().to_string());
        let username = token.and_then(|t| auth.verify_token(&t));
        match username {
            Some(u) => Ok(AuthUser(u)),
            None => {
                let next = parts.uri.path().to_string();
                let q = parts.uri.query().map(|q| format!("?{q}")).unwrap_or_default();
                let target = format!("/login?next={}{}", urlencode(&next), urlencode(&q));
                Err(Redirect::to(&target).into_response())
            }
        }
    }
}

/// Build a `Set-Cookie` for a freshly issued session token.
pub fn session_cookie(token: String) -> tower_cookies::Cookie<'static> {
    let mut c = tower_cookies::Cookie::new(SESSION_COOKIE, token);
    c.set_http_only(true);
    c.set_same_site(SameSite::Strict);
    c.set_path("/");
    c.set_max_age(tower_cookies::cookie::time::Duration::seconds(SESSION_TTL_SECS));
    c
}

/// Build a `Set-Cookie` that immediately expires the session cookie.
pub fn clear_cookie() -> tower_cookies::Cookie<'static> {
    let mut c = tower_cookies::Cookie::new(SESSION_COOKIE, "");
    c.set_http_only(true);
    c.set_same_site(SameSite::Strict);
    c.set_path("/");
    c.set_max_age(tower_cookies::cookie::time::Duration::seconds(0));
    c
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            other => out.push_str(&format!("%{:02X}", other)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_with_user(username: &str, password: &str) -> AuthState {
        let hash = bcrypt::hash(password, 4).unwrap();
        let users = vec![UserEntry {
            username: username.to_string(),
            bcrypt_hash: hash,
        }];
        AuthState {
            users: Arc::new(users),
            secret: Arc::new(vec![0xab; 32]),
        }
    }

    #[test]
    fn token_round_trip() {
        let auth = auth_with_user("alice", "hunter2");
        let token = auth.issue_token("alice");
        assert_eq!(auth.verify_token(&token).as_deref(), Some("alice"));
    }

    #[test]
    fn token_tamper_rejected() {
        let auth = auth_with_user("alice", "hunter2");
        let token = auth.issue_token("alice");
        let mut bytes = URL_SAFE_NO_PAD.decode(token.as_bytes()).unwrap();
        bytes[0] ^= 0x01;
        let tampered = URL_SAFE_NO_PAD.encode(&bytes);
        assert!(auth.verify_token(&tampered).is_none());
    }

    #[test]
    fn password_verify() {
        let auth = auth_with_user("alice", "hunter2");
        assert_eq!(auth.verify_password("alice", "hunter2").as_deref(), Some("alice"));
        assert!(auth.verify_password("alice", "wrong").is_none());
        assert!(auth.verify_password("bob", "hunter2").is_none());
    }
}
