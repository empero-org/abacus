use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::{Deserialize, Serialize};

pub const DEFAULT_FEEDBACK_ENDPOINT: &str = "https://abacus.empero.org/v1/feedback";

#[derive(Debug, Clone, Serialize)]
pub struct FeedbackPayload {
    pub category: String,
    pub message: String,
    pub include_diagnostics: bool,
    pub diagnostics: Vec<String>,
    pub session_id: Option<String>,
    pub workspace: String,
    pub app_version: String,
    pub os: String,
    pub arch: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeedbackReceipt {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Clone)]
pub struct FeedbackClient {
    endpoint: String,
    client: reqwest::Client,
}

impl FeedbackClient {
    pub fn new(endpoint: &str) -> Result<Self> {
        let endpoint = endpoint.trim();
        if endpoint.is_empty() {
            bail!("feedback endpoint is empty");
        }
        let url = reqwest::Url::parse(endpoint).context("feedback endpoint is not a valid URL")?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("feedback endpoint must use HTTP or HTTPS");
        }
        Ok(Self {
            endpoint: endpoint.to_owned(),
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(20))
                .user_agent(concat!("abacus-agent/", env!("CARGO_PKG_VERSION")))
                .build()?,
        })
    }

    pub async fn submit(&self, payload: &FeedbackPayload) -> Result<FeedbackReceipt> {
        let response = self
            .client
            .post(&self.endpoint)
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, "application/json")
            .json(payload)
            .send()
            .await
            .context("could not reach the feedback service")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail: String = body.chars().take(500).collect();
            bail!("feedback service returned {status}: {detail}");
        }
        if body.trim().is_empty() {
            return Ok(FeedbackReceipt {
                id: None,
                message: None,
            });
        }
        serde_json::from_str(&body).context("feedback service returned invalid JSON")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[tokio::test]
    async fn posts_feedback_without_a_transcript() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if let Some(header_end) = request.windows(4).position(|value| value == b"\r\n\r\n")
                {
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(str::trim)
                                .and_then(|value| value.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= header_end + 4 + length {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&request);
            assert!(text.contains("A useful report"));
            assert!(!text.contains("messages"));
            let body = r#"{"id":"feedback-1","message":"received"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let client = FeedbackClient::new(&format!("http://{address}/feedback")).unwrap();
        let receipt = client
            .submit(&FeedbackPayload {
                category: "general".into(),
                message: "A useful report".into(),
                include_diagnostics: false,
                diagnostics: Vec::new(),
                session_id: None,
                workspace: "demo".into(),
                app_version: "test".into(),
                os: "test".into(),
                arch: "test".into(),
            })
            .await
            .unwrap();
        assert_eq!(receipt.id.as_deref(), Some("feedback-1"));
        server.await.unwrap();
    }
}
