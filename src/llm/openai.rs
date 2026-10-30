use super::{BoxStream, ChatRequest, Event, LineDecoder, OpenAiToolAcc, Provider, ToolSpec};
use crate::session::{Message, Role, ToolCall};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc::Sender;

const STREAM_CHUNK_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl OpenAiProvider {
    pub fn new(base_url: String, api_key: Option<String>, _timeout: Duration) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| format!("building http client: {e}"))?;
        Ok(Self {
            client,
            base_url,
            api_key,
        })
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    fn headers(&self) -> Result<HeaderMap, String> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(key) = &self.api_key {
            let value = HeaderValue::from_str(&format!("Bearer {key}"))
                .map_err(|e| format!("invalid api key: rejected by Authorization header: {e}"))?;
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        Ok(headers)
    }

    fn build_body(&self, request: &ChatRequest) -> Value {
        let mut messages: Vec<Value> = Vec::new();
        if !request.system.trim().is_empty() {
            messages.push(json!({"role": "system", "content": request.system}));
        }
        messages.extend(request.messages.iter().map(convert_message));
        let mut body = json!({
            "model": request.model,
            "messages": messages,
            "stream": true,
        });
        if let Some(t) = request.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(m) = request.max_tokens {
            if requires_completion_tokens(&request.model) {
                body["max_completion_tokens"] = json!(m);
            } else {
                body["max_tokens"] = json!(m);
            }
        }
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(request.tools.iter().map(convert_tool).collect());
            body["tool_choice"] = json!("auto");
        }
        body
    }

    async fn run(&self, request: ChatRequest, tx: &Sender<Result<Event, String>>) -> Result<(), String> {
        let body = self.build_body(&request);
        let response = self
            .client
            .post(self.endpoint())
            .headers(self.headers()?)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(status_error(status, &text));
        }

        let is_stream = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("text/event-stream"))
            .unwrap_or(true);

        if !is_stream {
            let text = response.text().await.map_err(|e| format!("reading body: {e}"))?;
            let value: Value =
                serde_json::from_str(&text).map_err(|e| format!("decoding response: {e}"))?;
            return emit_json_response(value, tx).await;
        }

        let mut byte_stream = response.bytes_stream();
        let mut decoder = LineDecoder::new();
        let mut acc = OpenAiToolAcc::default();
        let mut saw_any = false;
        let mut saw_text = false;
        let mut reasoning = String::new();

        'outer: loop {
            let chunk = match tokio::time::timeout(STREAM_CHUNK_TIMEOUT, byte_stream.next()).await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(_) => {
                    return Err(format!(
                        "stream timed out: no data received for {}s",
                        STREAM_CHUNK_TIMEOUT.as_secs()
                    ))
                }
            };
            let chunk = chunk.map_err(|e| format!("stream error: {e}"))?;
            for line in decoder.push(&chunk) {
                let Some(data) = super::sse_data(&line) else {
                    continue;
                };
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                if let Some(err) = value.get("error") {
                    let msg = err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown provider error");
                    return Err(msg.to_string());
                }
                let Some(choice) = value.get("choices").and_then(|c| c.get(0)) else {
                    continue;
                };
                if let Some(delta) = choice.get("delta") {
                    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                        if !content.is_empty() {
                            saw_any = true;
                            saw_text = true;
                            tx.send(Ok(Event::Text(content.to_string())))
                                .await
                                .map_err(|_| "cancelled".to_string())?;
                        }
                    }
                    if let Some(content) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
                        if !content.is_empty() {
                            saw_any = true;
                            reasoning.push_str(content);
                        }
                    }
                    if let Some(calls) = delta.get("tool_calls").and_then(|c| c.as_array()) {
                        for call in calls {
                            let id = call.get("id").and_then(|v| v.as_str()).map(String::from);
                            let index = match call.get("index").and_then(|i| i.as_u64()) {
                                Some(index) => index as usize,
                                None => next_index(&acc, id.as_deref()),
                            };
                            let name = call
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|v| v.as_str())
                                .map(String::from);
                            let args = call
                                .get("function")
                                .and_then(|f| f.get("arguments"))
                                .and_then(|v| v.as_str())
                                .map(String::from);
                            saw_any = true;
                            acc.apply(index, id, name, args);
                        }
                    }
                }
                if choice.get("finish_reason").and_then(|f| f.as_str()).is_some() {
                    saw_any = true;
                    break 'outer;
                }
            }
        }

        if !saw_any {
            return Err("empty response from provider".into());
        }
        let tool_calls = acc.finish();
        if !saw_text && tool_calls.is_empty() {
            let detail = if reasoning.is_empty() {
                String::new()
            } else {
                format!(
                    " (reasoning only, {} chars discarded)",
                    reasoning.chars().count()
                )
            };
            return Err(format!("model returned no content{detail}"));
        }
        tx.send(Ok(Event::Complete { tool_calls }))
            .await
            .map_err(|_| "cancelled".to_string())
    }
}

async fn emit_json_response(value: Value, tx: &Sender<Result<Event, String>>) -> Result<(), String> {
    let mut saw_text = false;
    let mut reasoning_chars = 0usize;
    if let Some(message) = value
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
    {
        if let Some(content) = message.get("content").and_then(|c| c.as_str()) {
            if !content.is_empty() {
                saw_text = true;
                tx.send(Ok(Event::Text(content.to_string())))
                    .await
                    .map_err(|_| "cancelled".to_string())?;
            }
        }
        reasoning_chars = message
            .get("reasoning_content")
            .and_then(|c| c.as_str())
            .map(|c| c.chars().count())
            .unwrap_or(0);
    }
    let tool_calls = collect_tool_calls(&value);
    if !saw_text && tool_calls.is_empty() {
        let detail = if reasoning_chars == 0 {
            String::new()
        } else {
            format!(" (reasoning only, {reasoning_chars} chars discarded)")
        };
        return Err(format!("model returned no content{detail}"));
    }
    tx.send(Ok(Event::Complete { tool_calls }))
        .await
        .map_err(|_| "cancelled".to_string())
}

fn collect_tool_calls(value: &Value) -> Vec<ToolCall> {
    let mut acc = OpenAiToolAcc::default();
    if let Some(calls) = value
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("tool_calls"))
        .and_then(|c| c.as_array())
    {
        for (index, call) in calls.iter().enumerate() {
            let id = call.get("id").and_then(|v| v.as_str()).map(String::from);
            let name = call
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .map(String::from);
            let args = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
                .map(String::from);
            acc.apply(index, id, name, args);
        }
    }
    acc.finish()
}

fn requires_completion_tokens(model: &str) -> bool {
    if model.starts_with("gpt-5") {
        return true;
    }
    let mut chars = model.chars();
    matches!((chars.next(), chars.next()), (Some('o'), Some(first)) if first.is_ascii_digit())
}

fn next_index(acc: &OpenAiToolAcc, id: Option<&str>) -> usize {
    let Some(&last) = acc.by_index.keys().next_back() else {
        return 0;
    };
    let starts_new = matches!(
        id,
        Some(id) if !id.is_empty() && acc.by_index[&last].id != id
    );
    if starts_new {
        last + 1
    } else {
        last
    }
}

fn status_error(status: reqwest::StatusCode, body: &str) -> String {
    let snippet: String = body.chars().take(600).collect();
    format!("provider returned {status}: {snippet}")
}

fn convert_tool(tool: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
        }
    })
}

fn convert_message(message: &Message) -> Value {
    match message.role {
        Role::System => json!({"role": "system", "content": message.content}),
        Role::User => json!({"role": "user", "content": message.content}),
        Role::Assistant => {
            let content = if message.content.is_empty() {
                Value::Null
            } else {
                Value::String(message.content.clone())
            };
            let mut obj = json!({"role": "assistant", "content": content});
            if !message.tool_calls.is_empty() {
                let calls: Vec<Value> = message
                    .tool_calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "type": "function",
                            "function": {"name": c.name, "arguments": c.arguments}
                        })
                    })
                    .collect();
                obj["tool_calls"] = Value::Array(calls);
            }
            obj
        }
        Role::Tool => json!({
            "role": "tool",
            "tool_call_id": message.tool_call_id.clone().unwrap_or_default(),
            "content": message.content,
        }),
    }
}

impl Provider for OpenAiProvider {
    fn stream(&self, request: ChatRequest) -> BoxStream<Result<Event, String>> {
        let provider = Self {
            client: self.client.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
        };
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            if let Err(error) = provider.run(request, &tx).await {
                let _ = tx.send(Err(error)).await;
            }
        });
        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
    }
}
