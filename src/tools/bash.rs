use super::{optional_u64, require_str, ToolContext, MAX_RESULT_CHARS};
use serde_json::Value;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const DEFAULT_TIMEOUT: u64 = 60;
const MAX_CAPTURE_BYTES: usize = MAX_RESULT_CHARS;
const READ_CHUNK_BYTES: usize = 8192;

async fn drain<R>(pipe: Option<R>) -> Vec<u8>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut pipe = match pipe {
        Some(pipe) => pipe,
        None => return Vec::new(),
    };
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; READ_CHUNK_BYTES];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if buf.len() < MAX_CAPTURE_BYTES {
                    let take = read.min(MAX_CAPTURE_BYTES - buf.len());
                    buf.extend_from_slice(&chunk[..take]);
                }
            }
        }
    }
    buf
}

pub async fn run(args: Value, ctx: &ToolContext) -> Result<String, String> {
    let command = require_str(&args, "command")?;
    if command.trim().is_empty() {
        return Err("empty command".into());
    }
    let timeout_secs = optional_u64(&args, "timeout_secs").unwrap_or(DEFAULT_TIMEOUT).clamp(1, 600);

    let mut child = Command::new("sh");
    child
        .arg("-c")
        .arg(&command)
        .current_dir(&ctx.workdir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = child.spawn().map_err(|e| format!("failed to spawn: {e}"))?;

    let stdout_task = tokio::spawn(drain(child.stdout.take()));
    let stderr_task = tokio::spawn(drain(child.stderr.take()));

    let result = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await;

    let status_code = match result {
        Err(_) => {
            let _ = child.kill().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(format!("command timed out after {timeout_secs}s"));
        }
        Ok(Err(e)) => return Err(format!("waiting for command: {e}")),
        Ok(Ok(status)) => status,
    };

    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();

    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    let mut combined = String::new();
    if !stdout.trim().is_empty() {
        combined.push_str(stdout.trim_end());
        combined.push('\n');
    }
    if !stderr.trim().is_empty() {
        combined.push_str(stderr.trim_end());
        combined.push('\n');
    }
    if combined.is_empty() {
        combined.push_str("(no output)\n");
    }
    let status = match status_code.code() {
        Some(code) => format!("exit code {code}"),
        None => "killed by signal".to_string(),
    };
    let body = super::truncate(combined.trim_end(), MAX_RESULT_CHARS);
    Ok(format!("{body}\n— {status}"))
}
