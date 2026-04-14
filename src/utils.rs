use anyhow::Context;
use std::fs::File;
use std::os::unix::io::AsRawFd;
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

/// Acquire an exclusive flock on `/run/fscache/{instance_name}.lock`.
///
/// Returns the held `File` — keep it alive for the process lifetime.
/// The kernel automatically releases the lock when the file is dropped (process exit or crash).
/// Fails immediately with a clear error if another process already holds the lock.
pub fn acquire_instance_lock(instance_name: &str) -> anyhow::Result<File> {
    let lock_dir = PathBuf::from("/run/fscache");
    let lock_path = lock_dir.join(format!("{instance_name}.lock"));
    flock_exclusive(&lock_path).with_context(|| {
        format!("instance '{instance_name}' is already running (lock held at {})", lock_path.display())
    })
}

/// Acquire an exclusive flock on `/run/fscache/mount-{hash}.lock` for a target path.
///
/// Prevents two fscache instances from mounting the same target directory. On conflict,
/// reads `/proc/mounts` to identify which instance holds it for a clear error message.
pub fn acquire_target_lock(target: &Path) -> anyhow::Result<File> {
    let lock_name = mount_cache_name(target);
    let lock_path = PathBuf::from("/run/fscache").join(format!("mount-{lock_name}.lock"));
    flock_exclusive(&lock_path).with_context(|| {
        let detail = match find_fscache_mount_holder(target) {
            Some(name) => format!(" (held by instance '{name}')"),
            None => String::new(),
        };
        format!("target {} is already mounted by another fscache instance{detail}", target.display())
    })
}

/// Read `/proc/mounts` and return the fscache instance name mounted at `target`, if any.
///
/// Looks for entries with FSName `fscache-{name}` at the given mount point.
pub fn find_fscache_mount_holder(target: &Path) -> Option<String> {
    let mounts = std::fs::read_to_string("/proc/mounts").ok()?;
    parse_fscache_mount_holder(&mounts, target)
}

/// Open `lock_path` and acquire an exclusive non-blocking flock.
///
/// Returns the held `File` on success. Returns an error (with a generic message) if the
/// lock is already held — callers add context via `.with_context()` for friendly messages.
fn flock_exclusive(lock_path: &Path) -> anyhow::Result<File> {
    if let Some(dir) = lock_path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create lock directory {}", dir.display()))?;
    }
    let file = File::options()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_path)
        .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            anyhow::bail!("already locked");
        }
        anyhow::bail!("flock failed: {err}");
    }
    Ok(file)
}

fn parse_fscache_mount_holder(mounts: &str, target: &Path) -> Option<String> {
    let target_str = target.to_string_lossy();
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let source = fields.next()?;
        let mount_point = fields.next()?;
        if mount_point == target_str {
            if let Some(name) = source.strip_prefix("fscache-") {
                return Some(name.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ---- flock_exclusive ----

    #[test]
    fn flock_exclusive_succeeds_on_fresh_file() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("test.lock");
        assert!(flock_exclusive(&lock_path).is_ok());
    }

    #[test]
    fn flock_exclusive_conflict_within_process() {
        // Two separate open() calls on the same path produce independent file descriptions
        // and therefore independent flocks — the second must fail.
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("test.lock");
        let _held = flock_exclusive(&lock_path).expect("first lock should succeed");
        let err = flock_exclusive(&lock_path).expect_err("second lock should fail");
        assert!(err.to_string().contains("already locked"), "unexpected error: {err}");
    }

    #[test]
    fn flock_exclusive_released_on_drop() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("test.lock");
        {
            let _held = flock_exclusive(&lock_path).unwrap();
        }
        assert!(flock_exclusive(&lock_path).is_ok());
    }

    #[test]
    fn flock_exclusive_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("nested/dir/test.lock");
        assert!(flock_exclusive(&lock_path).is_ok());
        assert!(lock_path.exists());
    }

    // ---- parse_fscache_mount_holder ----

    #[test]
    fn parse_finds_matching_instance() {
        let mounts = "fscache-plex-movies /mnt/media fuse.fscache-plex-movies ro,nosuid 0 0\n\
                      sysfs /sys sysfs rw 0 0\n";
        assert_eq!(
            parse_fscache_mount_holder(mounts, Path::new("/mnt/media")),
            Some("plex-movies".to_string())
        );
    }

    #[test]
    fn parse_returns_none_for_non_fscache_mount() {
        let mounts = "//server/share /mnt/media cifs ro 0 0\n";
        assert_eq!(parse_fscache_mount_holder(mounts, Path::new("/mnt/media")), None);
    }

    #[test]
    fn parse_returns_none_when_target_absent() {
        let mounts = "fscache-plex-movies /mnt/other fuse.fscache-plex-movies ro 0 0\n";
        assert_eq!(parse_fscache_mount_holder(mounts, Path::new("/mnt/media")), None);
    }

    #[test]
    fn parse_returns_none_for_bare_fscache_fsname() {
        // Legacy fscache without instance name encoding — no dash suffix.
        let mounts = "fscache /mnt/media fuse.fscache ro 0 0\n";
        assert_eq!(parse_fscache_mount_holder(mounts, Path::new("/mnt/media")), None);
    }

    #[test]
    fn parse_handles_multiple_mounts_returns_correct_one() {
        let mounts = "fscache-plex-tv /mnt/tv fuse.fscache-plex-tv ro 0 0\n\
                      fscache-plex-movies /mnt/media fuse.fscache-plex-movies ro 0 0\n";
        assert_eq!(
            parse_fscache_mount_holder(mounts, Path::new("/mnt/media")),
            Some("plex-movies".to_string())
        );
        assert_eq!(
            parse_fscache_mount_holder(mounts, Path::new("/mnt/tv")),
            Some("plex-tv".to_string())
        );
    }

    #[test]
    fn parse_returns_none_on_empty_input() {
        assert_eq!(parse_fscache_mount_holder("", Path::new("/mnt/media")), None);
    }
}

/// Format a `SystemTime` as a local-time `HH:MM:SS` string.
pub fn fmt_time(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
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
