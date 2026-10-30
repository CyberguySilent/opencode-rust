use crate::config::{Config, Permission};
use crate::llm::{build, ChatRequest, Event, Provider, ToolSpec};
use crate::session::{Message, ToolCall};
use crate::tools::{self, ToolContext};
use futures_util::StreamExt;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

const MAX_ITERATIONS: usize = 40;
const MAX_TOOL_RESULT: usize = tools::MAX_RESULT_CHARS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    AllowAlways,
    Deny,
}

pub enum AgentEvent {
    Text(String),
    Message(Message),
    ToolStart { name: String, detail: String },
    ToolEnd { id: String, _name: String, status: &'static str, _preview: String },
    Permission { tool: String, detail: String, reply: oneshot::Sender<Decision> },
    PermissionAlways { tool: String },
    Error(String),
    TurnDone,
}

pub struct Agent {
    provider: Arc<dyn Provider>,
    system: String,
    model: String,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    tools: Vec<ToolSpec>,
    ctx: ToolContext,
}

fn platform_line() -> String {
    format!("{} · {}", std::env::consts::OS, std::env::consts::ARCH)
}

fn day_line() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = now / 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

impl Agent {
    pub fn new(config: &Config) -> Result<Self, String> {
        let provider = build(config)?;
        let workdir = config.workdir.display().to_string();
        let system = format!(
            "{}\n\nEnvironment:\n- working directory: {}\n- platform: {}\n- today's date: {}\n- shell: /bin/sh",
            config.system_prompt,
            workdir,
            platform_line(),
            day_line()
        );
        Ok(Self {
            provider,
            system,
            model: config.model.clone(),
            temperature: config.temperature,
            max_tokens: config.max_tokens,
            tools: tools::specs(),
            ctx: ToolContext {
                workdir: config.workdir.clone(),
                permissions: config.permissions.clone(),
            },
        })
    }

    fn emit(tx: &mpsc::UnboundedSender<AgentEvent>, event: AgentEvent) -> Result<(), String> {
        tx.send(event).map_err(|_| "session closed".to_string())
    }

    fn call_detail(name: &str, args: &Value) -> String {
        match name {
            "bash" => args
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            "read" | "write" | "edit" => args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            "glob" => args
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            "grep" => args
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            other => {
                let compact = args.to_string();
                if compact.chars().count() > 160 {
                    let trimmed: String = compact.chars().take(160).collect();
                    format!("{other} {trimmed}…")
                } else {
                    format!("{other} {compact}")
                }
            }
        }
    }

    fn preview(text: &str, max: usize) -> String {
        let single = text.lines().next().unwrap_or("").trim();
        let mut out: String = single.chars().take(max).collect();
        if single.chars().count() > max {
            out.push('…');
        }
        if out.is_empty() {
            out.push_str("(empty)");
        }
        out
    }

    async fn decide(
        &mut self,
        tx: &mpsc::UnboundedSender<AgentEvent>,
        tool: &str,
        detail: &str,
    ) -> Result<Decision, String> {
        match self.ctx.permissions.for_tool(tool) {
            Permission::Allow => Ok(Decision::Allow),
            Permission::Deny => Ok(Decision::Deny),
            Permission::Ask => {
                let (reply, receiver) = oneshot::channel();
                Self::emit(
                    tx,
                    AgentEvent::Permission {
                        tool: tool.to_string(),
                        detail: detail.to_string(),
                        reply,
                    },
                )?;
                Ok(receiver.await.unwrap_or(Decision::Deny))
            }
        }
    }

    pub async fn run_turn(
        &mut self,
        mut messages: Vec<Message>,
        tx: &mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<Vec<Message>, String> {
        let mut generated = Vec::new();

        for iteration in 0..MAX_ITERATIONS {
            let request = ChatRequest {
                model: self.model.clone(),
                messages: messages.clone(),
                tools: self.tools.clone(),
                temperature: self.temperature,
                max_tokens: self.max_tokens,
                system: self.system.clone(),
            };

            let mut stream = self.provider.stream(request);
            let mut text = String::new();
            let mut tool_calls: Vec<ToolCall> = Vec::new();

            while let Some(item) = stream.next().await {
                match item {
                    Ok(Event::Text(delta)) => {
                        text.push_str(&delta);
                        Self::emit(tx, AgentEvent::Text(delta))?;
                    }
                    Ok(Event::Complete { tool_calls: calls }) => {
                        tool_calls = calls;
                    }
                    Err(error) => {
                        let _ = Self::emit(tx, AgentEvent::Error(error.clone()));
                        return Err(error);
                    }
                }
            }

            let assistant = if tool_calls.is_empty() {
                Message::assistant(text)
            } else {
                Message::assistant_with_tools(text, tool_calls.clone())
            };
            messages.push(assistant.clone());
            generated.push(assistant.clone());
            Self::emit(tx, AgentEvent::Message(assistant))?;

            if tool_calls.is_empty() {
                return Ok(generated);
            }

            for call in tool_calls {
                let args: Value = serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| serde_json::json!({}));
                let detail = Self::call_detail(&call.name, &args);

                Self::emit(
                    tx,
                    AgentEvent::ToolStart {
                        name: call.name.clone(),
                        detail: detail.clone(),
                    },
                )?;

                let decision = self.decide(tx, &call.name, &detail).await?;
                let (status, result) = match decision {
                    Decision::Deny => (
                        "denied",
                        format!("permission denied for `{}`", call.name),
                    ),
                    Decision::AllowAlways => {
                        self.ctx
                            .permissions
                            .set_tool(&call.name, Permission::Allow);
                        Self::emit(tx, AgentEvent::PermissionAlways { tool: call.name.clone() })?;
                        match tools::execute(&call.name, args, &self.ctx).await {
                            Ok(output) => ("ok", output),
                            Err(error) => ("error", error),
                        }
                    }
                    Decision::Allow => match tools::execute(&call.name, args, &self.ctx).await {
                        Ok(output) => ("ok", output),
                        Err(error) => ("error", error),
                    },
                };

                let preview = Self::preview(&result, 200);
                Self::emit(
                    tx,
                    AgentEvent::ToolEnd {
                        id: call.id.clone(),
                        _name: call.name.clone(),
                        status,
                        _preview: preview.clone(),
                    },
                )?;

                let truncated = tools::truncate(&result, MAX_TOOL_RESULT);
                let message = Message::tool_result(call.id.clone(), call.name.clone(), truncated);
                messages.push(message.clone());
                generated.push(message.clone());
                Self::emit(tx, AgentEvent::Message(message))?;
            }

            if iteration + 1 == MAX_ITERATIONS {
                let error = format!("stopped after {MAX_ITERATIONS} tool iterations");
                let _ = Self::emit(tx, AgentEvent::Error(error.clone()));
                return Err(error);
            }
        }

        Ok(generated)
    }
}
