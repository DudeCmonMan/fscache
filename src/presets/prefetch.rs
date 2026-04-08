use std::path::{Path, PathBuf};

use regex::Regex;

use crate::preset::{CacheAction, CachePreset, ProcessInfo, RuleContext};

pub fn parse_mode(s: &str) -> anyhow::Result<PrefetchMode> {
    match s {
        "cache-hit-only"           => Ok(PrefetchMode::CacheHitOnly),
        "cache-neighbors"          => Ok(PrefetchMode::CacheNeighbors),
        "cache-parent-recursively" => Ok(PrefetchMode::CacheParentRecursively),
        other => anyhow::bail!(
            "unknown prefetch.mode {:?} — expected \"cache-hit-only\", \
             \"cache-neighbors\", or \"cache-parent-recursively\"",
            other
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefetchMode {
    /// Cache only the accessed file (equivalent to old cache-on-miss).
    CacheHitOnly,
    CacheNeighbors,
    CacheParentRecursively,
}

pub struct Prefetch {
    mode: PrefetchMode,
    max_depth: usize,
    blocklist: Vec<String>,
    whitelist: Vec<Regex>,
    blacklist: Vec<Regex>,
}

impl Prefetch {
    /// Construct and validate. All regex patterns are compiled upfront; returns Err if any
    /// pattern is invalid so the daemon refuses to start rather than silently misbehaving.
    pub fn new(
        mode: PrefetchMode,
        max_depth: usize,
        blocklist: Vec<String>,
        whitelist_patterns: &[String],
        blacklist_patterns: &[String],
    ) -> anyhow::Result<Self> {
        let compile = |patterns: &[String]| -> anyhow::Result<Vec<Regex>> {
            patterns
                .iter()
                .map(|p| {
                    Regex::new(p).map_err(|e| {
                        anyhow::anyhow!("invalid regex pattern {:?}: {}", p, e)
                    })
                })
                .collect()
        };

        Ok(Self {
            mode,
            max_depth,
            blocklist,
            whitelist: compile(whitelist_patterns)?,
            blacklist: compile(blacklist_patterns)?,
        })
    }

    /// Blacklist is checked first; then whitelist (empty whitelist = allow all).
    fn passes_filter(&self, filename: &str) -> bool {
        if self.blacklist.iter().any(|r| r.is_match(filename)) {
            return false;
        }
        if self.whitelist.is_empty() {
            return true;
        }
        self.whitelist.iter().any(|r| r.is_match(filename))
    }

    fn collect_neighbors(&self, path: &Path, ctx: &RuleContext) -> Vec<PathBuf> {
        let parent = path.parent().unwrap_or(Path::new(""));
        ctx.backing_store
            .list_dir(parent)
            .into_iter()
            .filter_map(|name| {
                let name_str = name.to_string_lossy();
                if !self.passes_filter(&name_str) {
                    return None;
                }
                let candidate = parent.join(&name);
                // Skip subdirectories — neighbors are flat-sibling files only.
                if ctx.backing_store.is_dir(&candidate) {
                    return None;
                }
                Some(candidate)
            })
            .collect()
    }

    fn collect_recursive(&self, dir: &Path, ctx: &RuleContext, depth: usize) -> Vec<PathBuf> {
        if depth == 0 {
            return vec![];
        }
        let mut results = Vec::new();
        for name in ctx.backing_store.list_dir(dir) {
            let candidate = dir.join(&name);
            if ctx.backing_store.is_dir(&candidate) {
                results.extend(self.collect_recursive(&candidate, ctx, depth - 1));
            } else {
                let name_str = name.to_string_lossy();
                if self.passes_filter(&name_str) {
                    results.push(candidate);
                }
            }
        }
        results
    }

    fn gather_files(&self, path: &Path, ctx: &RuleContext) -> Vec<CacheAction> {
        let files = match self.mode {
            PrefetchMode::CacheHitOnly => vec![path.to_path_buf()],
            PrefetchMode::CacheNeighbors => self.collect_neighbors(path, ctx),
            PrefetchMode::CacheParentRecursively => {
                let parent = path.parent().unwrap_or(Path::new(""));
                self.collect_recursive(parent, ctx, self.max_depth)
            }
        };
        if files.is_empty() {
            vec![]
        } else {
            vec![CacheAction::Cache(files)]
        }
    }
}

impl CachePreset for Prefetch {
    fn name(&self) -> &str {
        "prefetch"
    }

    fn should_filter(&self, process: &ProcessInfo) -> bool {
        if self.blocklist.is_empty() {
            return false;
        }
        process.is_blocked_by(&self.blocklist)
    }

    fn on_miss(&self, path: &Path, ctx: &RuleContext) -> Vec<CacheAction> {
        self.gather_files(path, ctx)
    }

    fn on_hit(&self, path: &Path, ctx: &RuleContext) -> Vec<CacheAction> {
        self.gather_files(path, ctx)
    }
}
