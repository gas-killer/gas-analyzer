//! Minimal OpenRouter chat-completions client for the admin diagnostics
//! button. Single-shot, no streaming, no tool-calling. Returns the raw
//! markdown content + token-usage counts so the caller can log cost.
//!
//! OpenRouter is OpenAI-compatible at the wire level, so swapping models
//! (or pointing `OPENROUTER_BASE_URL` at a self-hosted proxy) is a config-
//! only change.

use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api returned no content")]
    NoContent,
    #[error("api error {status}: {message}")]
    Api { status: u16, message: String },
}

#[derive(Clone)]
pub struct LlmClient {
    api_key: String,
    base_url: String,
    model: String,
    http: reqwest::Client,
}

impl LlmClient {
    pub fn new(api_key: String, base_url: String, model: String) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            api_key,
            base_url,
            model,
            http,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Single-turn chat completion. `system` is the role/style frame,
    /// `user` is the data bundle + question. Returns the model's content
    /// and the token counts.
    pub async fn complete(
        &self,
        system: &str,
        user: &str,
    ) -> Result<LlmResponse, LlmError> {
        let body = ChatRequest {
            model: &self.model,
            messages: vec![
                Message { role: "system", content: system },
                Message { role: "user", content: user },
            ],
            temperature: 0.2,
            max_tokens: 600,
        };
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            // OpenRouter recommends these headers — they identify the
            // calling app for routing/leaderboards, harmless if omitted.
            .header("HTTP-Referer", "https://github.com/gas-killer/gas-analyzer")
            .header("X-Title", "Gas Killer Indexer")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let message = resp.text().await.unwrap_or_default();
            return Err(LlmError::Api {
                status: status.as_u16(),
                message: message.chars().take(500).collect(),
            });
        }
        let parsed: ChatResponse = resp.json().await?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or(LlmError::NoContent)?;
        Ok(LlmResponse {
            content,
            tokens_in: parsed.usage.as_ref().map(|u| u.prompt_tokens).unwrap_or(0),
            tokens_out: parsed
                .usage
                .as_ref()
                .map(|u| u.completion_tokens)
                .unwrap_or(0),
        })
    }
}

#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub content: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
}

// ---------- wire types ----------

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    temperature: f32,
    max_tokens: u32,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: String,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}
