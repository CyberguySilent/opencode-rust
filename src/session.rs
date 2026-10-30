use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<u64>,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            ts: Some(now()),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            ts: Some(now()),
        }
    }

    pub fn assistant_with_tools(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
            tool_name: None,
            ts: Some(now()),
        }
    }

    pub fn tool_result(id: impl Into<String>, name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(id.into()),
            tool_name: Some(name.into()),
            ts: Some(now()),
        }
    }

    pub fn preview(&self, max: usize) -> String {        let mut text = self.content.replace('\n', " ");
        if text.chars().count() > max {
            text = text.chars().take(max).collect();
            text.push('…');
        }
        text
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub messages: Vec<Message>,
}

impl Session {
    pub fn new() -> Self {
        let ts = now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            title: "new session".into(),
            created_at: ts,
            updated_at: ts,
            messages: Vec::new(),
        }
    }

    pub fn push(&mut self, message: Message) {
        if self.title == "new session" && message.role == Role::User {
            self.title = message.preview(40);
        }
        self.updated_at = now();
        self.messages.push(message);
    }

    pub fn clear(&mut self) {
        self.messages.clear();
        self.updated_at = now();
    }

    pub fn file_path(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.json", self.id))
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub count: usize,
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Result<Self> {
        if !dir.exists() {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("creating session dir {}", dir.display()))?;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("setting permissions on {}", dir.display()))?;
        }
        Ok(Self { dir })
    }

    pub fn save(&self, session: &Session) -> Result<()> {
        let path = session.file_path(&self.dir);
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_string_pretty(session)?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.write_all(data.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        drop(file);
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    pub fn load(&self, id: &str) -> Result<Option<Session>> {
        let path = self.dir.join(format!("{id}.json"));
        if !path.is_file() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path)?;
        Ok(Some(serde_json::from_str(&raw)?))
    }

    pub fn latest(&self) -> Result<Option<Session>> {
        Ok(self.list()?.first().and_then(|m| self.load(&m.id).ok().flatten()))
    }

    pub fn list(&self) -> Result<Vec<SessionMeta>> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(_) => return Ok(out),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(session) = serde_json::from_str::<Session>(&raw) else {
                continue;
            };
            out.push(SessionMeta {
                id: session.id,
                title: session.title,
                created_at: session.created_at,
                updated_at: session.updated_at,
                count: session.messages.len(),
            });
        }
        out.sort_by_key(|meta| std::cmp::Reverse(meta.updated_at));
        Ok(out)
    }
}
