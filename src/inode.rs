use std::collections::HashMap;
use std::path::{Path, PathBuf};

use fuser::INodeNo;

/// Root inode is always 1 in FUSE.
const ROOT_INO: u64 = 1;

struct InodeEntry {
    path: PathBuf,
    refcount: u64,
}

/// Bidirectional map between FUSE inode numbers and relative file paths.
///
/// Inode 1 is always the root, mapped to an empty path ("").
pub struct InodeTable {
    by_ino: HashMap<u64, InodeEntry>,
    by_path: HashMap<PathBuf, u64>,
    next_ino: u64,
}

impl InodeTable {
    pub fn new() -> Self {
        let mut by_ino = HashMap::new();
        let mut by_path = HashMap::new();
        by_ino.insert(
            ROOT_INO,
            InodeEntry {
                path: PathBuf::new(),
                refcount: u64::MAX, // root is never forgotten
            },
        );
        by_path.insert(PathBuf::new(), ROOT_INO);
        Self {
            by_ino,
            by_path,
            next_ino: 2,
        }
    }

    pub fn root_ino() -> INodeNo {
        INodeNo(ROOT_INO)
    }

    /// Relative path for an inode, or None if not found.
    pub fn get_path(&self, ino: u64) -> Option<&Path> {
        self.by_ino.get(&ino).map(|e| e.path.as_path())
    }

    /// Inode for a path, if already allocated.
    pub fn get_path_ino(&self, path: &Path) -> Option<u64> {
        self.by_path.get(path).copied()
    }

    /// Get or allocate an inode for a path, incrementing its refcount.
    pub fn get_or_create(&mut self, path: &Path) -> u64 {
        if let Some(&ino) = self.by_path.get(path) {
            self.by_ino.get_mut(&ino).unwrap().refcount =
                self.by_ino[&ino].refcount.saturating_add(1);
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.by_ino.insert(
            ino,
            InodeEntry {
                path: path.to_path_buf(),
                refcount: 1,
            },
        );
        self.by_path.insert(path.to_path_buf(), ino);
        ino
    }

    /// Decrement refcount. Removes entry when it reaches zero. Root is never removed.
    pub fn forget(&mut self, ino: u64, nlookup: u64) {
        if ino == ROOT_INO {
            return;
        }
        if let Some(entry) = self.by_ino.get_mut(&ino) {
            entry.refcount = entry.refcount.saturating_sub(nlookup);
            if entry.refcount == 0 {
                let path = entry.path.clone();
                self.by_ino.remove(&ino);
                self.by_path.remove(&path);
            }
        }
    }
}
