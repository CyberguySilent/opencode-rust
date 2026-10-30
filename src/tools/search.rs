use super::{require_str, ToolContext};
use serde_json::Value;
use std::path::{Path, PathBuf};

const MAX_MATCHES: usize = 100;
const MAX_RESULTS: usize = 200;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_LINE_CHARS: usize = 300;
const MAX_WALK_DEPTH: usize = 64;

const IGNORED_DIRS: [&str; 12] = [
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    "dist",
    "build",
    ".next",
    ".cache",
];

fn is_ignored(name: &str) -> bool {
    IGNORED_DIRS.contains(&name)
}

pub fn glob(args: Value, ctx: &ToolContext) -> Result<String, String> {
    let pattern = require_str(&args, "pattern")?;
    let base = match args.get("path").and_then(|p| p.as_str()) {
        Some(p) => ctx.resolve(p)?,
        None => ctx.workdir.clone(),
    };

    let full_pattern = if Path::new(&pattern).is_absolute() {
        pattern.clone()
    } else {
        base.join(&pattern).to_string_lossy().into_owned()
    };

    let entries = glob::glob(&full_pattern).map_err(|e| format!("invalid glob pattern: {e}"))?;

    let mut matches = Vec::new();
    for entry in entries {
        let Ok(path) = entry else { continue };
        if std::fs::symlink_metadata(&path)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(true)
        {
            continue;
        }
        if path.is_dir() {
            continue;
        }
        if path
            .components()
            .any(|c| c.as_os_str().to_str().map(is_ignored).unwrap_or(false))
        {
            continue;
        }
        matches.push(ctx.display_path(&path));
        if matches.len() >= MAX_RESULTS {
            break;
        }
    }
    matches.sort();

    if matches.is_empty() {
        return Ok(format!("no files matched `{pattern}`"));
    }
    let truncated = matches.len() >= MAX_RESULTS;
    let mut out = matches.join("\n");
    if truncated {
        out.push_str(&format!("\n… [showing first {MAX_RESULTS} matches]"));
    }
    Ok(out)
}

fn matches_filter(name: &str, include: Option<&str>) -> bool {
    let Some(filter) = include else { return true };
    let filter = filter.trim();
    if filter.is_empty() {
        return true;
    }
    if let Some(ext) = filter.strip_prefix("*.") {
        return name.ends_with(ext);
    }
    if !filter.contains('*') && !filter.contains('.') {
        return name.ends_with(filter);
    }
    match glob::Pattern::new(filter) {
        Ok(pattern) => pattern.matches(name),
        Err(_) => name.contains(filter),
    }
}

fn walk(dir: &Path, include: Option<&str>, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() > 5000 {
            return;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if file_type.is_dir() {
            if is_ignored(&name) {
                continue;
            }
            walk(&path, include, out, depth + 1);
        } else if file_type.is_file() && matches_filter(&name, include) {
            out.push(path);
        }
    }
}

fn read_text(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

pub fn grep(args: Value, ctx: &ToolContext) -> Result<String, String> {
    let pattern = require_str(&args, "pattern")?;
    let include = args.get("include").and_then(|v| v.as_str());
    let base = match args.get("path").and_then(|p| p.as_str()) {
        Some(p) => ctx.resolve(p)?,
        None => ctx.workdir.clone(),
    };

    let regex = regex::RegexBuilder::new(&pattern)
        .case_insensitive(args.get("ignore_case").and_then(|v| v.as_bool()).unwrap_or(false))
        .build()
        .map_err(|e| format!("invalid regex: {e}"))?;

    if !base.exists() {
        return Err(format!("{} does not exist", base.display()));
    }

    let mut files = Vec::new();
    if base.is_file() {
        files.push(base);
    } else {
        walk(&base, include, &mut files, 0);
        files.sort();
    }

    let mut matches = Vec::new();
    let mut scanned = 0usize;
    for path in &files {
        let Some(content) = read_text(path) else {
            continue;
        };
        scanned += 1;
        for (i, line) in content.lines().enumerate() {
            if regex.is_match(line) {
                let text: String = line.chars().take(MAX_LINE_CHARS).collect();
                matches.push(format!("{}:{}:{}", ctx.display_path(path), i + 1, text.trim_end()));
                if matches.len() >= MAX_MATCHES {
                    break;
                }
            }
        }
        if matches.len() >= MAX_MATCHES {
            break;
        }
    }

    if matches.is_empty() {
        return Ok(format!(
            "no matches for `{pattern}` in {} file(s)",
            files.len()
        ));
    }
    let mut out = matches.join("\n");
    if matches.len() >= MAX_MATCHES {
        out.push_str(&format!("\n… [showing first {MAX_MATCHES} matches, {scanned} files scanned]"));
    }
    Ok(out)
}
