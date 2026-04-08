use anyhow::Context;
use std::path::{Path, PathBuf};

/// Walk `dir` recursively, returning all non-metadata file paths.
/// Excludes `.partial`, `.db`, `.db-wal`, `.db-shm` files.
pub(crate) fn collect_cache_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_cache_files_inner(dir, &mut out);
    out
}

fn collect_cache_files_inner(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_cache_files_inner(&path, out);
            } else if !is_non_media_file(&path) {
                out.push(path);
            }
        }
    }
}

/// Returns true for files that should be excluded from cache accounting
/// (SQLite DB files, WAL/SHM journals, .partial copies-in-progress).
pub(crate) fn is_non_media_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.ends_with(".partial")
        || name.ends_with(".db")
        || name.ends_with(".db-wal")
        || name.ends_with(".db-shm")
}

/// Derive a unique, human-readable cache subdirectory name for a target path.
///
/// Sanitizes the full path into a dash-separated slug and appends an 8-char hex
/// hash so that targets sharing a basename (e.g. `/mnt/a/media` and `/mnt/b/media`)
/// always produce distinct names.
///
/// Example: `/mnt/a/media` → `mnt-a-media-3f2b1c4d`
pub fn mount_cache_name(target: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let slug: String = target
        .to_string_lossy()
        .trim_start_matches('/')
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    let mut hasher = DefaultHasher::new();
    target.hash(&mut hasher);
    let hash = hasher.finish() as u32;

    format!("{slug}-{hash:08x}")
}

pub fn validate_targets(targets: &[PathBuf]) -> anyhow::Result<()> {
    if targets.is_empty() {
        anyhow::bail!("target_directories is empty — add at least one path");
    }
    let mut seen = std::collections::HashSet::new();
    for target in targets {
        if !target.exists() {
            anyhow::bail!("target_directory does not exist: {}", target.display());
        }
        let canonical = target.canonicalize().unwrap_or_else(|_| target.clone());
        if !seen.insert(canonical) {
            anyhow::bail!("duplicate target_directory: {}", target.display());
        }
    }
    Ok(())
}

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
