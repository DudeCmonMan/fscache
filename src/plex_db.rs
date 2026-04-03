use std::path::{Path, PathBuf};
use rusqlite::{Connection, params};

/// Read-only access to the Plex SQLite library database.
///
/// Targets Plex's schema:
///   metadata_items(id, metadata_type, title, index, parent_id, grandparent_id)
///     type 2 = season, type 4 = episode
///   media_items(id, metadata_item_id)
///   media_parts(id, media_item_id, file)  — `file` is the absolute path on disk
pub struct PlexDb {
    conn: Connection,
    target_dir: PathBuf,
}

struct EpisodeInfo {
    season_id: i64,
    episode_index: i64,
    show_id: i64,
}

impl PlexDb {
    pub fn open(db_path: &Path, target_dir: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Ok(Self { conn, target_dir: target_dir.to_path_buf() })
    }

    /// Open an in-memory database. Used in tests.
    pub fn open_in_memory(target_dir: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Ok(Self { conn, target_dir: target_dir.to_path_buf() })
    }

    /// Execute arbitrary SQL. Used in tests to populate schema/data.
    pub fn exec(&self, sql: &str) -> anyhow::Result<()> {
        self.conn.execute_batch(sql)?;
        Ok(())
    }

    /// Return up to `lookahead` relative paths for episodes that come after `rel_path`.
    /// Searches the same season first, then subsequent seasons.
    pub fn next_episodes(&self, rel_path: &Path, lookahead: usize) -> Vec<PathBuf> {
        if lookahead == 0 {
            return vec![];
        }
        let abs = format!(
            "{}/{}",
            self.target_dir.to_string_lossy().trim_end_matches('/'),
            rel_path.to_string_lossy().trim_start_matches('/')
        );
        let info = match self.find_episode(&abs) {
            Some(i) => i,
            None => return vec![],
        };

        let mut result = Vec::new();

        // Same season, higher episode index
        let same = self.episodes_after(info.season_id, info.episode_index, lookahead);
        result.extend(same);

        // Cross-season if we still need more
        if result.len() < lookahead {
            let season_idx = self.season_index(info.season_id).unwrap_or(0);
            for next_season_id in self.seasons_after(info.show_id, season_idx) {
                if result.len() >= lookahead {
                    break;
                }
                let needed = lookahead - result.len();
                let eps = self.episodes_after(next_season_id, -1, needed);
                result.extend(eps);
            }
        }

        result
    }

    fn find_episode(&self, abs_file: &str) -> Option<EpisodeInfo> {
        self.conn
            .query_row(
                r#"SELECT mi.id, mi."index", mi.parent_id, mi.grandparent_id
                   FROM metadata_items mi
                   INNER JOIN media_items med ON med.metadata_item_id = mi.id
                   INNER JOIN media_parts mp  ON mp.media_item_id = med.id
                   WHERE mp.file = ?1 AND mi.metadata_type = 4
                   LIMIT 1"#,
                params![abs_file],
                |row| {
                    Ok(EpisodeInfo {
                        season_id: row.get(2)?,
                        episode_index: row.get(1)?,
                        show_id: row.get(3)?,
                    })
                },
            )
            .ok()
    }

    fn episodes_after(&self, season_id: i64, after_index: i64, limit: usize) -> Vec<PathBuf> {
        if limit == 0 {
            return vec![];
        }
        let mut stmt = match self.conn.prepare(
            r#"SELECT mp.file
               FROM metadata_items mi
               INNER JOIN media_items med ON med.metadata_item_id = mi.id
               INNER JOIN media_parts mp  ON mp.media_item_id = med.id
               WHERE mi.parent_id = ?1 AND mi."index" > ?2 AND mi.metadata_type = 4
               ORDER BY mi."index" ASC
               LIMIT ?3"#,
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };

        stmt.query_map(params![season_id, after_index, limit as i64], |row| {
            row.get::<_, String>(0)
        })
        .ok()
        .map(|rows| rows.flatten().map(|abs| self.strip_prefix(&abs)).collect())
        .unwrap_or_default()
    }

    fn season_index(&self, season_id: i64) -> Option<i64> {
        self.conn
            .query_row(
                r#"SELECT "index" FROM metadata_items WHERE id = ?1 AND metadata_type = 2"#,
                params![season_id],
                |row| row.get(0),
            )
            .ok()
    }

    fn seasons_after(&self, show_id: i64, after_index: i64) -> Vec<i64> {
        let mut stmt = match self.conn.prepare(
            r#"SELECT id FROM metadata_items
               WHERE parent_id = ?1 AND metadata_type = 2 AND "index" > ?2
               ORDER BY "index" ASC"#,
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        stmt.query_map(params![show_id, after_index], |row| row.get(0))
            .ok()
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    fn strip_prefix(&self, abs: &str) -> PathBuf {
        let base = self.target_dir.to_string_lossy();
        let base = base.trim_end_matches('/');
        PathBuf::from(abs.strip_prefix(base).unwrap_or(abs).trim_start_matches('/'))
    }
}
