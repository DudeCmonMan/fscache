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

    pub fn get_path(&self, ino: u64) -> Option<&Path> {
        self.by_ino.get(&ino).map(|e| e.path.as_path())
    }

    pub fn remove_path(&mut self, path: &Path) {
        if path == Path::new("") {
            return;
        }
        if let Some(ino) = self.by_path.remove(path) {
            self.by_ino.remove(&ino);
        }
    }

    pub fn rename_path(&mut self, old: &Path, new: &Path) {
        if old == Path::new("") {
            return;
        }
        self.remove_path(new);
        if let Some(ino) = self.by_path.remove(old) {
            if let Some(entry) = self.by_ino.get_mut(&ino) {
                entry.path = new.to_path_buf();
            }
            self.by_path.insert(new.to_path_buf(), ino);
        }

        let descendants: Vec<_> = self
            .by_path
            .keys()
            .filter(|path| path.starts_with(old) && *path != old)
            .cloned()
            .collect();
        for old_child in descendants {
            if let Ok(suffix) = old_child.strip_prefix(old) {
                let new_child = new.join(suffix);
                if let Some(ino) = self.by_path.remove(&old_child) {
                    if let Some(entry) = self.by_ino.get_mut(&ino) {
                        entry.path = new_child.clone();
                    }
                    self.by_path.insert(new_child, ino);
                }
            }
        }
    }

    pub fn remove_subtree(&mut self, path: &Path) {
        if path == Path::new("") {
            return;
        }
        let paths: Vec<_> = self
            .by_path
            .keys()
            .filter(|candidate| candidate.starts_with(path))
            .cloned()
            .collect();
        for path in paths {
            self.remove_path(&path);
        }
    }
    pub fn get_path_ino(&self, path: &Path) -> Option<u64> {
        self.by_path.get(path).copied()
    }

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
