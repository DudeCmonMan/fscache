use std::path::Path;

use crate::preset::{CacheAction, CachePreset, ProcessInfo, RuleContext};

/// Simple preset: cache every file on first miss, with optional process blocklist.
///
/// Fires an AccessEvent on every cache miss. Blocked processes (and their
/// children) are filtered — their opens do not trigger prediction.
pub struct CacheOnMiss {
    pub blocklist: Vec<String>,
}

impl CacheOnMiss {
    pub fn new(blocklist: Vec<String>) -> Self {
        Self { blocklist }
    }
}

impl CachePreset for CacheOnMiss {
    fn name(&self) -> &str {
        "cache_on_miss"
    }

    fn should_filter(&self, process: &ProcessInfo) -> bool {
        if self.blocklist.is_empty() {
            return false;
        }
        process.is_blocked_by(&self.blocklist)
    }

    fn on_miss(&self, path: &Path, _ctx: &RuleContext) -> Vec<CacheAction> {
        vec![CacheAction::Cache(vec![path.to_path_buf()])]
    }
}
