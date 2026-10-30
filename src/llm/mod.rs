pub mod anthropic;
pub mod openai;

use crate::config::{Config, ProviderKind};
use crate::session::{Message, ToolCall};
use serde_json::Value;
use std::pin::Pin;
use std::sync::Arc;

pub type BoxStream<T> = Pin<Box<dyn futures_util::Stream<Item = T> + Send>>;

#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolSpec {
    pub fn new(name: &str, description: &str, parameters: Value) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            parameters,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub system: String,
}

#[derive(Clone, Debug)]
pub enum Event {
    Text(String),
    Complete { tool_calls: Vec<ToolCall> },
}

pub trait Provider: Send + Sync {
    fn stream(&self, request: ChatRequest) -> BoxStream<Result<Event, String>>;
}

pub fn build(config: &Config) -> Result<Arc<dyn Provider>, String> {
    let timeout = std::time::Duration::from_secs(config.request_timeout_secs);
    match config.provider {
        ProviderKind::Anthropic => Ok(Arc::new(anthropic::AnthropicProvider::new(
            config.base_url.clone(),
            config.api_key.clone(),
            timeout,
        )?)),
        ProviderKind::OpenAi | ProviderKind::Ollama | ProviderKind::Custom => {
            Ok(Arc::new(openai::OpenAiProvider::new(
                config.base_url.clone(),
                config.api_key.clone(),
                timeout,
            )?))
        }
    }
}

pub(crate) struct LineDecoder {
    buffer: Vec<u8>,
}

impl LineDecoder {
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);
        let mut lines = Vec::new();
        while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=pos).collect();
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            lines.push(String::from_utf8_lossy(&line).into_owned());
        }
        lines
    }
}

pub(crate) fn sse_data(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("data:")?;
    Some(rest.trim())
}

#[derive(Default)]
pub(crate) struct OpenAiToolAcc {
    pub by_index: std::collections::BTreeMap<usize, ToolCall>,
}

impl OpenAiToolAcc {
    pub fn apply(&mut self, index: usize, id: Option<String>, name: Option<String>, args: Option<String>) {
        let entry = self.by_index.entry(index).or_insert_with(|| ToolCall {
            id: String::new(),
            name: String::new(),
            arguments: String::new(),
        });
        if let Some(id) = id {
            if !id.is_empty() {
                entry.id = id;
            }
        }
        if let Some(name) = name {
            if !name.is_empty() {
                entry.name = name;
            }
        }
        if let Some(args) = args {
            entry.arguments.push_str(&args);
        }
    }

    pub fn finish(&mut self) -> Vec<ToolCall> {
        let mut calls: Vec<ToolCall> = std::mem::take(&mut self.by_index)
            .into_values()
            .filter(|c| !c.name.is_empty())
            .collect();
        for (i, call) in calls.iter_mut().enumerate() {
            if call.id.is_empty() {
                call.id = format!("call_{i}");
            }
            if call.arguments.trim().is_empty() {
                call.arguments = "{}".into();
            }
        }
        calls
    }
}
