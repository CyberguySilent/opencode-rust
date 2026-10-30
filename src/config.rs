use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
    Ollama,
    Custom,
}

impl ProviderKind {
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "openai" | "openai-compatible" => Ok(Self::OpenAi),
            "anthropic" | "claude" => Ok(Self::Anthropic),
            "ollama" => Ok(Self::Ollama),
            "custom" => Ok(Self::Custom),
            other => bail!(
                "unknown provider `{other}` (expected openai | anthropic | ollama | custom)"
            ),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Ollama => "ollama",
            Self::Custom => "custom",
        }
    }

    fn default_base_url(&self) -> &'static str {
        match self {
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com",
            Self::Ollama => "http://localhost:11434/v1",
            Self::Custom => "",
        }
    }

    fn default_model(&self) -> &'static str {
        match self {
            Self::OpenAi => "gpt-4o",
            Self::Anthropic => "claude-sonnet-4-5",
            Self::Ollama => "qwen3-coder:30b",
            Self::Custom => "default",
        }
    }

    fn default_key_env(&self) -> &'static str {
        match self {
            Self::OpenAi => "OPENAI_API_KEY",
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::Ollama => "",
            Self::Custom => "OPENAI_API_KEY",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Permission {
    Allow,
    Ask,
    Deny,
}

impl Permission {
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "allow" | "y" | "yes" | "auto" => Ok(Self::Allow),
            "ask" => Ok(Self::Ask),
            "deny" | "no" | "n" => Ok(Self::Deny),
            other => bail!("unknown permission `{other}` (expected allow | ask | deny)"),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Permissions {
    pub bash: Permission,
    pub read: Permission,
    pub write: Permission,
    pub edit: Permission,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            bash: Permission::Ask,
            read: Permission::Allow,
            write: Permission::Ask,
            edit: Permission::Ask,
        }
    }
}

impl Permissions {
    pub fn allow_all() -> Self {
        Self {
            bash: Permission::Allow,
            read: Permission::Allow,
            write: Permission::Allow,
            edit: Permission::Allow,
        }
    }

    pub fn for_tool(&self, tool: &str) -> Permission {
        match tool {
            "bash" => self.bash,
            "read" | "glob" | "grep" => self.read,
            "write" => self.write,
            "edit" => self.edit,
            _ => Permission::Ask,
        }
    }

    pub fn set_tool(&mut self, tool: &str, value: Permission) {
        match tool {
            "bash" => self.bash = value,
            "read" | "glob" | "grep" => self.read = value,
            "write" => self.write = value,
            "edit" => self.edit = value,
            _ => {}
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub system_prompt: Option<String>,
    pub permissions: Permissions,
    pub workdir: Option<PathBuf>,
    pub request_timeout_secs: Option<u64>,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            provider: "openai".into(),
            model: String::new(),
            base_url: None,
            api_key_env: None,
            api_key: None,
            temperature: None,
            max_tokens: None,
            system_prompt: None,
            permissions: Permissions::default(),
            workdir: None,
            request_timeout_secs: Some(180),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub provider: ProviderKind,
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub system_prompt: String,
    pub permissions: Permissions,
    pub workdir: PathBuf,
    pub request_timeout_secs: u64,
    pub session_dir: PathBuf,
}

#[derive(Default)]
pub struct CliOverrides {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub config_path: Option<PathBuf>,
    pub yolo: bool,
    pub workdir: Option<PathBuf>,
}

const DEFAULT_SYSTEM_PROMPT: &str = "You are opencode, a coding agent running in the user's terminal.\n\
You help the user understand, modify and build software in the current working directory.\n\n\
Guidelines:\n\
- Prefer small, correct, idiomatic changes; match the surrounding code style.\n\
- Use the available tools instead of guessing: read files before editing, grep before assuming.\n\
- Explain briefly what you are doing, then do it. Keep prose short.\n\
- Never invent file paths, APIs or errors. If something failed, say so.\n\
- Paths are resolved relative to the working directory shown in the status bar.";

fn config_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    out.push(PathBuf::from("opencode-rust.json"));
    out.push(PathBuf::from(".opencode-rust.json"));
    out.push(PathBuf::from(".opencode/config.json"));
    if let Some(home) = dirs::config_dir() {
        out.push(home.join("opencode-rust").join("config.json"));
    }
    out
}

fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("opencode-rust")
}

pub fn default_session_dir() -> PathBuf {
    data_dir().join("sessions")
}

fn load_file(path: &Path) -> Result<FileConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing config {}", path.display()))
}

fn env_or(key_env: Option<&str>, api_key: Option<&str>) -> Option<String> {
    if let Some(direct) = api_key {
        if !direct.is_empty() {
            return Some(direct.to_string());
        }
    }
    if let Some(name) = key_env {
        if !name.is_empty() {
            if let Ok(v) = std::env::var(name) {
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    None
}

impl Config {
    pub fn apply_provider(&mut self, kind: ProviderKind) {
        let previous_default = self.provider.default_model();
        self.provider = kind;
        let base = kind.default_base_url();
        if !base.is_empty() {
            self.base_url = base.to_string();
        }
        if self.api_key.is_none() {
            let key_env = kind.default_key_env();
            self.api_key =
                env_or(Some(key_env), None).or_else(|| std::env::var("OPENCODE_API_KEY").ok());
        }
        if self.model.is_empty() || self.model == previous_default {
            self.model = kind.default_model().to_string();
        }
    }

    pub fn load(overrides: CliOverrides) -> Result<Self> {
        let file = if let Some(explicit) = overrides.config_path.clone() {
            load_file(&explicit)?
        } else {
            let mut found = None;
            for candidate in config_candidates() {
                if candidate.is_file() {
                    found = Some(candidate);
                    break;
                }
            }
            match found {
                Some(path) => load_file(&path)?,
                None => FileConfig::default(),
            }
        };

        let provider = match overrides.provider {
            Some(p) => ProviderKind::parse(&p)?,
            None => ProviderKind::parse(&file.provider)?,
        };

        let model = overrides
            .model
            .clone()
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| {
                if !file.model.is_empty() {
                    file.model.clone()
                } else {
                    provider.default_model().to_string()
                }
            });

        let base_url = overrides
            .base_url
            .clone()
            .or(file.base_url.clone())
            .unwrap_or_else(|| provider.default_base_url().to_string())
            .trim_end_matches('/')
            .to_string();

        if provider == ProviderKind::Custom && base_url.is_empty() {
            bail!("provider `custom` requires a base_url (config or --base-url)");
        }

        let api_key_env = file
            .api_key_env
            .clone()
            .unwrap_or_else(|| provider.default_key_env().to_string());
        let api_key = env_or(Some(&api_key_env), file.api_key.as_deref())
            .or_else(|| std::env::var("OPENCODE_API_KEY").ok());

        let workdir = overrides
            .workdir
            .clone()
            .or_else(|| file.workdir.clone())
            .map(|p| {
                if p.is_absolute() {
                    p
                } else {
                        std::env::current_dir()
                            .unwrap_or_else(|_| PathBuf::from("."))
                            .join(p)
                    }
            })
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

        let system_prompt = file
            .system_prompt
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());

        let permissions = if overrides.yolo {
            Permissions::allow_all()
        } else {
            file.permissions.clone()
        };

        Ok(Self {
            provider,
            model,
            base_url,
            api_key,
            temperature: file.temperature,
            max_tokens: file.max_tokens,
            system_prompt,
            permissions,
            workdir,
            request_timeout_secs: file.request_timeout_secs.unwrap_or(180),
            session_dir: default_session_dir(),
        })
    }

    pub fn describe(&self) -> String {
        let key = if self.api_key.is_some() {
            "set"
        } else {
            "missing"
        };
        format!(
            "{} · {} · {} · key:{key} · workdir:{}",
            self.provider.label(),
            self.model,
            self.base_url,
            self.workdir.display()
        )
    }
}
