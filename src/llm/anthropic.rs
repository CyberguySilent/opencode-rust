use super::{BoxStream, ChatRequest, Event, LineDecoder, Provider, ToolSpec};
use crate::session::{Message, Role, ToolCall};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc::Sender;

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 8192;
const STREAM_CHUNK_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl AnthropicProvider {
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
        format!("{}/v1/messages", self.base_url.trim_end_matches('/'))
    }

    fn headers(&self) -> Result<HeaderMap, String> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            "anthropic-version",
            HeaderValue::from_static(ANTHROPIC_VERSION),
        );
        if let Some(key) = &self.api_key {
            let value = HeaderValue::from_str(key)
                .map_err(|e| format!("invalid api key: rejected by x-api-key header: {e}"))?;
            headers.insert("x-api-key", value);
        }
        Ok(headers)
    }

    fn build_body(&self, request: &ChatRequest) -> Value {
        let messages = convert_messages(&request.messages);
        let mut body = json!({
            "model": request.model,
            "max_tokens": request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            "messages": messages,
            "stream": true,
        });
        if !request.system.trim().is_empty() {
            body["system"] = json!(request.system);
        }
        if let Some(t) = request.temperature {
            body["temperature"] = json!(t);
        }
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(request.tools.iter().map(convert_tool).collect());
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
        let mut tool_blocks: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
        let mut saw_any = false;

        loop {
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
                if data.is_empty() {
                    continue;
                }
                let Ok(event) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                match event.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "error" => {
                        let msg = event
                            .get("error")
                            .and_then(|e| e.get("message"))
                            .and_then(|m| m.as_str())
                            .unwrap_or("unknown provider error");
                        return Err(msg.to_string());
                    }
                    "content_block_start" => {
                        saw_any = true;
                        let index = event.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                        if let Some(block) = event.get("content_block") {
                            if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                                let id = block
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default()
                                    .to_string();
                                let name = block
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default()
                                    .to_string();
                                let initial = block
                                    .get("input")
                                    .map(|v| v.to_string())
                                    .filter(|s| s != "null" && s != "{}")
                                    .unwrap_or_default();
                                tool_blocks.insert(index, (id, name, initial));
                            }
                        }
                    }
                    "content_block_delta" => {
                        saw_any = true;
                        let index = event.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                        let Some(delta) = event.get("delta") else {
                            continue;
                        };
                        match delta.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                            "text_delta" => {
                                if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                                    if !text.is_empty() {
                                        tx.send(Ok(Event::Text(text.to_string())))
                                            .await
                                            .map_err(|_| "cancelled".to_string())?;
                                    }
                                }
                            }
                            "input_json_delta" => {
                                if let Some(json_part) =
                                    delta.get("partial_json").and_then(|t| t.as_str())
                                {
                                    let entry = tool_blocks.entry(index).or_insert_with(|| {
                                        (String::new(), String::new(), String::new())
                                    });
                                    entry.2.push_str(json_part);
                                }
                            }
                            _ => {}
                        }
                    }
                    "message_stop" => {
                        let tool_calls = finalize_tools(&mut tool_blocks);
                        tx.send(Ok(Event::Complete { tool_calls }))
                            .await
                            .map_err(|_| "cancelled".to_string())?;
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }

        if !saw_any {
            return Err("empty response from provider".into());
        }
        let tool_calls = finalize_tools(&mut tool_blocks);
        tx.send(Ok(Event::Complete { tool_calls }))
            .await
            .map_err(|_| "cancelled".to_string())
    }
}

fn finalize_tools(blocks: &mut BTreeMap<usize, (String, String, String)>) -> Vec<ToolCall> {
    let mut out = Vec::new();
    for (index, (id, name, args)) in std::mem::take(blocks) {
        if name.is_empty() {
            continue;
        }
        let arguments = if args.trim().is_empty() {
            "{}".to_string()
        } else {
            args
        };
        out.push(ToolCall {
            id: if id.is_empty() {
                format!("toolu_{index}")
            } else {
                id
            },
            name,
            arguments,
        });
    }
    out
}

async fn emit_json_response(value: Value, tx: &Sender<Result<Event, String>>) -> Result<(), String> {
    let mut tool_calls = Vec::new();
    if let Some(content) = value.get("content").and_then(|c| c.as_array()) {
        for block in content {
            match block.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "text" => {
                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                        if !text.is_empty() {
                            tx.send(Ok(Event::Text(text.to_string())))
                                .await
                                .map_err(|_| "cancelled".to_string())?;
                        }
                    }
                }
                "tool_use" => {
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let args = block
                        .get("input")
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "{}".to_string());
                    if !name.is_empty() {
                        tool_calls.push(ToolCall { id, name, arguments: args });
                    }
                }
                _ => {}
            }
        }
    }
    tx.send(Ok(Event::Complete { tool_calls }))
        .await
        .map_err(|_| "cancelled".to_string())
}

fn status_error(status: reqwest::StatusCode, body: &str) -> String {
    let snippet: String = body.chars().take(600).collect();
    format!("provider returned {status}: {snippet}")
}

fn convert_tool(tool: &ToolSpec) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.parameters,
    })
}

fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for message in messages {
        let block = match message.role {
            Role::System => json!({"role": "user", "content": message.content}),
            Role::User => json!({"role": "user", "content": message.content}),
            Role::Assistant => {
                let mut content: Vec<Value> = Vec::new();
                if !message.content.is_empty() {
                    content.push(json!({"type": "text", "text": message.content}));
                }
                for call in &message.tool_calls {
                    let input: Value =
                        serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}));
                    content.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": input,
                    }));
                }
                if content.is_empty() {
                    content.push(json!({"type": "text", "text": ""}));
                }
                json!({"role": "assistant", "content": content})
            }
            Role::Tool => json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": message.tool_call_id.clone().unwrap_or_default(),
                    "content": message.content,
                }],
            }),
        };
        merge_block(&mut out, block);
    }
    out
}

fn merge_block(out: &mut Vec<Value>, block: Value) {
    let role = block.get("role").and_then(|r| r.as_str()).unwrap_or("");
    if let Some(last) = out.last_mut() {
        if last.get("role").and_then(|r| r.as_str()) == Some(role) {
            let mut last_content = last
                .get("content")
                .cloned()
                .unwrap_or_else(|| json!([]));
            let mut new_content = block.get("content").cloned().unwrap_or_else(|| json!([]));
            if last_content.is_string() {
                last_content = json!([{"type": "text", "text": last_content.as_str().unwrap_or("")}]);
            }
            if new_content.is_string() {
                new_content = json!([{"type": "text", "text": new_content.as_str().unwrap_or("")}]);
            }
            if let (Some(mut a), Some(b)) = (
                last_content.as_array().cloned(),
                new_content.as_array().cloned(),
            ) {
                a.extend(b);
                last["content"] = Value::Array(a);
                return;
            }
        }
    }
    out.push(block);
}

impl Provider for AnthropicProvider {
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
