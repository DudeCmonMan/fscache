use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

use crate::backing_store::BackingStore;

static SEASON_EP_RE: OnceLock<Regex> = OnceLock::new();
static SEASON_DIR_RE: OnceLock<Regex> = OnceLock::new();

fn season_ep_re() -> &'static Regex {
    SEASON_EP_RE.get_or_init(|| Regex::new(r"(?i)[Ss](\d{1,2})[Ee](\d{1,3})").unwrap())
}

fn season_dir_re() -> &'static Regex {
    SEASON_DIR_RE.get_or_init(|| Regex::new(r"(?i)^Season\s+0*(\d+)$").unwrap())
}

/// Parse `SxxExx` (or `sxxexx`) from a filename.  Returns `(season, episode)`.
pub fn parse_season_episode(name: &str) -> Option<(u32, u32)> {
    let cap = season_ep_re().captures(name)?;
    Some((cap[1].parse().ok()?, cap[2].parse().ok()?))
}

/// Parse a "Season N" directory name.  Returns the season number.
pub fn parse_season_dir(name: &str) -> Option<u32> {
    let cap = season_dir_re().captures(name)?;
    cap[1].parse().ok()
}

/// Return the "show root" for deferred-event deduplication: the parent directory
/// for episode files (groups same-show events together), or the full path for
/// non-episode files.
pub fn show_root(rel_path: &Path) -> PathBuf {
    rel_path.parent().unwrap_or(Path::new("")).to_path_buf()
}

/// Find up to `lookahead` episodes that follow `rel_path` in the backing store.
///
/// Phase 1: same season, higher episode numbers in the same directory.
/// Phase 2: first episodes of the next season if the lookahead window is not yet full.
///   - Structured layout: `Season X/` subdirectories under a shared show directory.
///   - Flat layout: all seasons in one directory.
pub fn find_next_episodes(rel_path: &Path, bs: &BackingStore, lookahead: usize) -> Vec<PathBuf> {
    let name = match rel_path.file_name() {
        Some(n) => n.to_string_lossy().into_owned(),
        None => return vec![],
    };
    let (season, episode) = match parse_season_episode(&name) {
        Some(se) => se,
        None => return vec![],
    };
    let dir = rel_path.parent().unwrap_or(Path::new(""));

    // Phase 1: same-season, higher-episode files in the current directory.
    let entries = bs.list_dir(dir);
    let mut candidates: Vec<(u32, u32, PathBuf)> = entries
        .into_iter()
        .filter_map(|entry_name| {
            let s = entry_name.to_string_lossy();
            let (s_num, e_num) = parse_season_episode(&s)?;
            if s_num == season && e_num > episode {
                Some((s_num, e_num, dir.join(&*entry_name)))
            } else {
                None
            }
        })
        .collect();
    candidates.sort_by_key(|(s, e, _)| (*s, *e));
    candidates.truncate(lookahead);

    // Phase 2: cross-season, if we still need more episodes.
    if candidates.len() < lookahead {
        let needed = lookahead - candidates.len();
        let parent_dir_name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        if parse_season_dir(&parent_dir_name).is_some() {
            // Structured layout: Season X folders under a show directory.
            let show_dir = dir.parent().unwrap_or(Path::new(""));
            let show_entries = bs.list_dir(show_dir);

            let mut next_seasons: Vec<(u32, PathBuf)> = show_entries
                .into_iter()
                .filter_map(|entry_name| {
                    let s_num = parse_season_dir(&entry_name.to_string_lossy())?;
                    if s_num > season {
                        Some((s_num, show_dir.join(&*entry_name)))
                    } else {
                        None
                    }
                })
                .collect();
            next_seasons.sort_by_key(|(s, _)| *s);

            'outer: for (_, season_dir) in next_seasons {
                let season_entries = bs.list_dir(&season_dir);
                let mut eps: Vec<(u32, u32, PathBuf)> = season_entries
                    .into_iter()
                    .filter_map(|entry_name| {
                        let s = entry_name.to_string_lossy();
                        let (s_num, e_num) = parse_season_episode(&s)?;
                        Some((s_num, e_num, season_dir.join(&*entry_name)))
                    })
                    .collect();
                eps.sort_by_key(|(s, e, _)| (*s, *e));
                for ep in eps {
                    if candidates.len() >= lookahead {
                        break 'outer;
                    }
                    candidates.push(ep);
                }
            }
        } else {
            // Flat layout: all seasons in one directory. Scan for higher-season episodes.
            let flat_entries = bs.list_dir(dir);
            let mut flat_candidates: Vec<(u32, u32, PathBuf)> = flat_entries
                .into_iter()
                .filter_map(|entry_name| {
                    let s = entry_name.to_string_lossy();
                    let (s_num, e_num) = parse_season_episode(&s)?;
                    if s_num > season {
                        Some((s_num, e_num, dir.join(&*entry_name)))
                    } else {
                        None
                    }
                })
                .collect();
            flat_candidates.sort_by_key(|(s, e, _)| (*s, *e));
            for ep in flat_candidates.into_iter().take(needed) {
                candidates.push(ep);
            }
        }
    }

    candidates.into_iter().take(lookahead).map(|(_, _, p)| p).collect()
}
