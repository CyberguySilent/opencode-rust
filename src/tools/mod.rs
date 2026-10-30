use crate::config::Permissions;
use crate::llm::ToolSpec;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

pub mod bash;
pub mod file;
pub mod search;

pub const MAX_RESULT_CHARS: usize = 30_000;

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn canonicalize_lenient(path: &Path) -> PathBuf {
    if let Ok(clean) = path.canonicalize() {
        return clean;
    }
    let mut tail: Vec<OsString> = Vec::new();
    let mut cursor = path.to_path_buf();
    while let Some(name) = cursor.file_name().map(|n| n.to_os_string()) {
        if let Ok(clean) = cursor.canonicalize() {
            let mut out = clean;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        tail.push(name);
        if !cursor.pop() {
            break;
        }
    }
    path.to_path_buf()
}

#[derive(Clone, Debug)]
pub struct ToolContext {
    pub workdir: PathBuf,
    pub permissions: Permissions,
}

impl ToolContext {
    pub fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let candidate = Path::new(path);
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.workdir.join(candidate)
        };
        Ok(normalize_lexical(&joined))
    }

    pub fn ensure_in_workdir(&self, path: &Path) -> Result<(), String> {
        let target = canonicalize_lenient(path);
        let workdir = self
            .workdir
            .canonicalize()
            .unwrap_or_else(|_| self.workdir.clone());
        if target.starts_with(&workdir) {
            Ok(())
        } else {
            Err(format!(
                "{} is outside the working directory {}",
                target.display(),
                workdir.display()
            ))
        }
    }

    pub fn display_path(&self, path: &Path) -> String {
        path.strip_prefix(&self.workdir)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string())
    }
}

fn check_scope(name: &str, args: &Value, ctx: &ToolContext) -> Result<(), String> {
    match name {
        "read" | "write" | "edit" => {
            let raw = require_str(args, "path")?;
            let path = ctx.resolve(&raw)?;
            ctx.ensure_in_workdir(&path)
        }
        "glob" | "grep" => {
            if let Some(base) = args.get("path").and_then(|v| v.as_str()) {
                let path = ctx.resolve(base)?;
                ctx.ensure_in_workdir(&path)?;
            }
            if name == "glob" {
                let pattern = require_str(args, "pattern")?;
                if Path::new(&pattern).is_absolute() {
                    ctx.ensure_in_workdir(Path::new(&pattern))?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec::new(
            "bash",
            "Run a shell command in the project working directory. Prefer specialized tools \
             (glob, grep, read, write, edit) over raw shell when they fit.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command line to execute"},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 600,
                                      "description": "Optional timeout, defaults to 60s"}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        ),
        ToolSpec::new(
            "read",
            "Read a UTF-8 text file. Returns content with 1-based line numbers. \
             Use offset/limit to page through large files.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, absolute or relative to the working directory"},
                    "offset": {"type": "integer", "minimum": 1, "description": "First line to read (1-based)"},
                    "limit": {"type": "integer", "minimum": 1, "description": "Maximum number of lines to read"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        ToolSpec::new(
            "write",
            "Create or overwrite a file with the given content. Parent directories are created.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, absolute or relative to the working directory"},
                    "content": {"type": "string", "description": "Full file content"}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        ),
        ToolSpec::new(
            "edit",
            "Replace an exact text span inside a file. The old_string must appear exactly once \
             unless replace_all is set.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path, absolute or relative to the working directory"},
                    "old_string": {"type": "string", "description": "Exact text to replace"},
                    "new_string": {"type": "string", "description": "Replacement text"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence instead of requiring uniqueness"}
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        ),
        ToolSpec::new(
            "glob",
            "List files matching a glob pattern (for example **/*.rs, src/**/*.ts, *.toml). \
             Build outputs and VCS directories are excluded.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob pattern"},
                    "path": {"type": "string", "description": "Directory to search in, defaults to the working directory"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        ),
        ToolSpec::new(
            "grep",
            "Search file contents with a regular expression. Returns path:line:text matches. \
             Excludes build outputs and VCS directories.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression to search for"},
                    "path": {"type": "string", "description": "Directory to search in, defaults to the working directory"},
                    "include": {"type": "string", "description": "Optional file filter, e.g. *.rs or rs"}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        ),
    ]
}

pub async fn execute(name: &str, args: Value, ctx: &ToolContext) -> Result<String, String> {
    check_scope(name, &args, ctx)?;
    match name {
        "bash" => bash::run(args, ctx).await,
        "read" => file::read(args, ctx),
        "write" => file::write(args, ctx),
        "edit" => file::edit(args, ctx),
        "glob" => search::glob(args, ctx),
        "grep" => search::grep(args, ctx),
        other => Err(format!("unknown tool `{other}`")),
    }
}

pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!(
        "{kept}\n… [truncated, {} of {} chars shown]",
        max,
        text.chars().count()
    )
}

pub fn require_str(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing required argument `{key}`"))
}

pub fn optional_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(|v| v.as_u64())
}

pub fn optional_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}
