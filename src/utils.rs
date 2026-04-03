use anyhow::Context;
use std::path::PathBuf;

/// Find a named file by checking next to the running binary first, then the current directory.
pub fn find_file_near_binary(filename: &str) -> anyhow::Result<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(filename);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    let candidate = std::env::current_dir()
        .context("failed to get current directory")?
        .join(filename);
    if candidate.exists() {
        return Ok(candidate);
    }
    anyhow::bail!("{} not found next to binary or in current directory", filename)
}

/// Human-readable file size string (e.g. "2.4 MB").
pub fn format_file_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}
