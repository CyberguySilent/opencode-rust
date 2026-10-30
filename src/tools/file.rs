use super::{optional_bool, optional_u64, require_str, ToolContext};
use serde_json::Value;
use std::path::Path;

const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;
const DEFAULT_LINES: usize = 2000;
const MAX_LINES: usize = 4000;

fn read_raw(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.is_dir() {
        return Err(format!("{} is a directory", path.display()));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{} is too large ({} bytes, limit is {} bytes)",
            path.display(),
            meta.len(),
            MAX_FILE_BYTES
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return Err(format!("{} looks like a binary file", path.display()));
    }
    String::from_utf8(bytes).map_err(|e| format!("{} is not valid UTF-8: {e}", path.display()))
}

pub fn read(args: Value, ctx: &ToolContext) -> Result<String, String> {
    let raw_path = require_str(&args, "path")?;
    let path = ctx.resolve(&raw_path)?;
    let content = read_raw(&path)?;
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();

    let offset = optional_u64(&args, "offset").unwrap_or(1).max(1) as usize;
    let limit = optional_u64(&args, "limit")
        .map(|l| l as usize)
        .unwrap_or(DEFAULT_LINES)
        .min(MAX_LINES)
        .max(1);

    if offset > total && total > 0 {
        return Err(format!(
            "offset {offset} is past the end of {} ({} lines)",
            ctx.display_path(&path),
            total
        ));
    }

    let start = offset.saturating_sub(1).min(total);
    let end = (start + limit).min(total);
    let width = end.max(1).to_string().len().max(4);

    let mut out = String::new();
    if start > 0 || end < total {
        out.push_str(&format!(
            "file {} — lines {}-{} of {}\n",
            ctx.display_path(&path),
            start + 1,
            end,
            total
        ));
    }
    for (i, line) in lines[start..end].iter().enumerate() {
        out.push_str(&format!("{:>width$}| {}\n", start + i + 1, line, width = width));
    }
    if out.is_empty() {
        out.push_str("(empty file)\n");
    }
    Ok(out.trim_end().to_string())
}

pub fn write(args: Value, ctx: &ToolContext) -> Result<String, String> {
    let raw_path = require_str(&args, "path")?;
    let content = require_str(&args, "content")?;
    let path = ctx.resolve(&raw_path)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(&path, content.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!(
        "Wrote {} bytes to {}",
        content.len(),
        ctx.display_path(&path)
    ))
}

pub fn edit(args: Value, ctx: &ToolContext) -> Result<String, String> {
    let raw_path = require_str(&args, "path")?;
    let old_string = require_str(&args, "old_string")?;
    let new_string = require_str(&args, "new_string")?;
    let replace_all = optional_bool(&args, "replace_all");

    if old_string == new_string {
        return Err("old_string and new_string are identical".into());
    }
    if old_string.is_empty() {
        return Err("old_string must not be empty".into());
    }

    let path = ctx.resolve(&raw_path)?;
    let content = read_raw(&path)?;
    let occurrences = content.matches(&old_string).count();

    if occurrences == 0 {
        return Err(format!(
            "old_string not found in {}",
            ctx.display_path(&path)
        ));
    }
    if occurrences > 1 && !replace_all {
        return Err(format!(
            "old_string matches {occurrences} times in {}; provide more context to make it unique or set replace_all",
            ctx.display_path(&path)
        ));
    }

    let updated = if replace_all {
        content.replace(&old_string, &new_string)
    } else {
        content.replacen(&old_string, &new_string, 1)
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(&path, updated.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;

    let replaced = if replace_all { occurrences } else { 1 };
    Ok(format!(
        "Replaced {replaced} occurrence{} in {}",
        if replaced == 1 { "" } else { "s" },
        ctx.display_path(&path)
    ))
}
