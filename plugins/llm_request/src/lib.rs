use std::sync::Arc;

use harness_config::{KEY_LLM_CONFIG, LlmConfig};
use harness_core::{Context, Plugin, PluginMeta};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

pub const KEY_CHAT_SERVICE: &str = "llm.chat";

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    fn new(role: &str, content: impl Into<String>) -> Self {
        Message {
            role: role.to_owned(),
            content: content.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("provider returned no choices")]
    EmptyChoices,
    #[error("provider returned no message content")]
    EmptyContent,
}

#[derive(Debug, Serialize)]
struct CompletionRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
}

#[derive(Debug, Deserialize)]
struct CompletionResponse {
    choices: Vec<CompletionChoice>,
}

#[derive(Debug, Deserialize)]
struct CompletionChoice {
    message: CompletionMessage,
}

#[derive(Debug, Deserialize)]
struct CompletionMessage {
    content: Option<String>,
}

pub struct ChatService {
    client: reqwest::Client,
    config: Arc<LlmConfig>,
    history: Mutex<Vec<Message>>,
}

impl ChatService {
    pub fn new(client: reqwest::Client, config: Arc<LlmConfig>) -> Self {
        ChatService {
            client,
            config,
            history: Mutex::new(Vec::new()),
        }
    }

    pub async fn chat(&self, input: impl Into<String>) -> Result<String, LlmError> {
        let mut history = self.history.lock().await;
        history.push(Message::new("user", input));

        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let body = CompletionRequest {
            model: &self.config.model,
            messages: &history,
        };
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.config.api_key)
            .header("User-Agent", &self.config.user_agent)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        let completion: CompletionResponse = response.json().await?;
        let Some(choice) = completion.choices.into_iter().next() else {
            return Err(LlmError::EmptyChoices);
        };
        let Some(reply) = choice.message.content else {
            return Err(LlmError::EmptyContent);
        };
        history.push(Message::new("assistant", reply.clone()));
        Ok(reply)
    }
}

pub struct LlmPlugin {
    client: reqwest::Client,
}

impl LlmPlugin {
    pub fn new() -> Result<Self, LlmError> {
        Ok(LlmPlugin {
            client: reqwest::Client::builder().build()?,
        })
    }
}

impl Plugin for LlmPlugin {
    fn meta(&self) -> PluginMeta {
        PluginMeta::new("llm")
            .injects(KEY_LLM_CONFIG)
            .provides(KEY_CHAT_SERVICE)
    }

    fn build(&self, ctx: Context) -> harness_core::Result<()> {
        let config: Arc<LlmConfig> = ctx.inject_key(KEY_LLM_CONFIG)?;
        let service = Arc::new(ChatService::new(self.client.clone(), config));
        ctx.provide_key(KEY_CHAT_SERVICE, service);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_completion_request() {
        let history = vec![
            Message::new("user", "hi"),
            Message::new("assistant", "hello"),
        ];
        let request = CompletionRequest {
            model: "m1",
            messages: &history,
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["model"], "m1");
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(json["messages"][0]["content"], "hi");
        assert_eq!(json["messages"][1]["role"], "assistant");
    }

    #[test]
    fn parses_completion_response() {
        let payload = r#"{
            "id": "x",
            "choices": [
                { "index": 0, "message": { "role": "assistant", "content": "reply" } }
            ],
            "usage": { "total_tokens": 7 }
        }"#;
        let response: CompletionResponse = serde_json::from_str(payload).unwrap();
        assert_eq!(
            response.choices[0].message.content.as_deref(),
            Some("reply")
        );
    }

    #[tokio::test]
    async fn round_trips_chat_and_history() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let body = r#"{"choices":[{"message":{"role":"assistant","content":"pong"}}]}"#;
            let http = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(http.as_bytes()).await.unwrap();
            sock.shutdown().await.unwrap();
        });

        let service = ChatService::new(
            reqwest::Client::new(),
            Arc::new(LlmConfig {
                base_url: format!("http://{addr}"),
                model: "m1".into(),
                api_key: "k".into(),
                user_agent: "opencode/1.18.18".into(),
            }),
        );
        let reply = service.chat("ping").await.unwrap();
        assert_eq!(reply, "pong");

        let history = service.history.lock().await;
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].role, "user");
        assert_eq!(history[0].content, "ping");
        assert_eq!(history[1].role, "assistant");
        assert_eq!(history[1].content, "pong");

        server.await.unwrap();
    }
}
