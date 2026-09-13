//! SQLite-backed metadata index for the file search prototype.
//!
//! This crate owns a separate file-search database. It does not use the
//! desktop app database and can be rebuilt or queried by the CLI, daemon, or
//! future adapters.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use kfs_core::{
  ranking::{score_candidate, tokenize},
  BackendError, EntryKind, IndexRebuildStats, IndexRefreshStats, IndexRepairStats, IndexRootStatus,
  MatchKind, MetadataIndex, SearchCandidate, SearchConfig, SearchQuery, SearchResult, SearchRoot,
};
use kfs_crawler::{crawl, CrawlOptions};
use rusqlite::{params, params_from_iter, types::Value, Connection, OptionalExtension};

pub type RebuildStats = IndexRebuildStats;
pub type RefreshStats = IndexRefreshStats;
pub type RepairStats = IndexRepairStats;
pub type RootStatus = IndexRootStatus;

/// Monotonic schema version of the index database, stored in `PRAGMA
/// user_version`. Version 0 covers every database created before versioning
/// existed, with or without the legacy `entries.path_lower` column.
pub const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedSearchOutcome {
  pub candidate_count: usize,
  pub results: Vec<SearchResult>,
}

#[derive(Debug)]
pub struct SqliteIndex {
  conn: Connection,
}

impl MetadataIndex for SqliteIndex {
  fn rebuild_index(&mut self, config: &SearchConfig) -> Result<IndexRebuildStats, BackendError> {
    self.rebuild(config).map_err(sqlite_error)
  }

  fn refresh_index(&mut self, config: &SearchConfig) -> Result<IndexRefreshStats, BackendError> {
    self.refresh(config).map_err(sqlite_error)
  }

  fn repair_index(&mut self, config: &SearchConfig) -> Result<IndexRepairStats, BackendError> {
    self.repair(config).map_err(sqlite_error)
  }

  fn search_index(
    &self,
    config: &SearchConfig,
    query: &SearchQuery,
  ) -> Result<Vec<SearchResult>, BackendError> {
    self.search(config, query).map_err(sqlite_error)
  }

  fn status(&self) -> Result<Vec<IndexRootStatus>, BackendError> {
    SqliteIndex::status(self).map_err(sqlite_error)
  }
}

impl SqliteIndex {
  pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    let index = Self { conn };
    index.migrate()?;
    Ok(index)
  }

  pub fn open_memory() -> rusqlite::Result<Self> {
    let conn = Connection::open_in_memory()?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    let index = Self { conn };
    index.migrate()?;
    Ok(index)
  }

  pub fn rebuild(&mut self, config: &SearchConfig) -> rusqlite::Result<RebuildStats> {
    let now = unix_now();
    let mut total_entries = 0;
    let mut total_skipped = 0;
    let mut errors = Vec::new();

    let tx = self.conn.transaction()?;
    for root in config.roots.iter().filter(|root| root.enabled) {
      let root_id = upsert_root_tx(&tx, root)?;
      let root_config = SearchConfig {
        roots: vec![root.clone()],
      };
      let options = CrawlOptions::new(root_config);
      let (entries, stats) = crawl(&options);
      let root_has_errors = !stats.errors.is_empty();
      total_skipped += stats.skipped;
      errors.extend(stats.errors);

      if root_has_errors {
        mark_root_state_dirty_tx(&tx, root_id)?;
        continue;
      }

      tx.execute("DELETE FROM entries WHERE root_id = ?1", params![root_id])?;

      for entry in entries {
        let entry_id = insert_entry_tx(&tx, root_id, &entry, now)?;
        insert_terms_tx(&tx, entry_id, &entry.path)?;
        total_entries += 1;
      }

      tx.execute(
                "INSERT INTO root_state (root_id, generation, last_full_scan_at, last_incremental_at, dirty, entry_count)
                 VALUES (?1, COALESCE((SELECT generation + 1 FROM root_state WHERE root_id = ?1), 1), ?2, NULL, 0, ?3)
                 ON CONFLICT(root_id) DO UPDATE SET
                   generation = root_state.generation + 1,
                   last_full_scan_at = excluded.last_full_scan_at,
                   dirty = 0,
                   entry_count = excluded.entry_count",
                params![root_id, now, count_entries_tx(&tx, root_id)?],
            )?;
    }
    tx.commit()?;

    Ok(RebuildStats {
      roots: config.roots.iter().filter(|root| root.enabled).count(),
      entries: total_entries,
      skipped: total_skipped,
      errors,
    })
  }

  pub fn refresh(&mut self, config: &SearchConfig) -> rusqlite::Result<RefreshStats> {
    let now = unix_now();
    let mut inserted = 0;
    let mut updated = 0;
    let mut deleted = 0;
    let mut unchanged = 0;
    let mut total_skipped = 0;
    let mut errors = Vec::new();

    let tx = self.conn.transaction()?;
    for root in config.roots.iter().filter(|root| root.enabled) {
      let root_id = upsert_root_tx(&tx, root)?;
      let mut existing = load_existing_entries_tx(&tx, root_id)?;
      let root_config = SearchConfig {
        roots: vec![root.clone()],
      };
      let options = CrawlOptions::new(root_config);
      let (entries, stats) = crawl(&options);
      let root_has_errors = !stats.errors.is_empty();
      total_skipped += stats.skipped;
      errors.extend(stats.errors);

      if root_has_errors {
        mark_root_state_dirty_tx(&tx, root_id)?;
        continue;
      }

      let mut seen_paths = HashSet::new();
      for entry in entries {
        let path_key = entry.path.to_string_lossy().into_owned();
        seen_paths.insert(path_key.clone());
        if let Some(existing_entry) = existing.remove(&path_key) {
          if entry_changed(&existing_entry, &entry) {
            update_entry_tx(&tx, existing_entry.id, &entry, now)?;
            updated += 1;
          } else {
            unchanged += 1;
          }
        } else {
          let entry_id = insert_entry_tx(&tx, root_id, &entry, now)?;
          insert_terms_tx(&tx, entry_id, &entry.path)?;
          inserted += 1;
        }
      }

      for (path, existing_entry) in existing {
        if !seen_paths.contains(&path) && !existing_entry.deleted {
          mark_entry_deleted_tx(&tx, existing_entry.id, now)?;
          deleted += 1;
        }
      }

      tx.execute(
                "INSERT INTO root_state (root_id, generation, last_full_scan_at, last_incremental_at, dirty, entry_count)
                 VALUES (?1, COALESCE((SELECT generation + 1 FROM root_state WHERE root_id = ?1), 1), NULL, ?2, ?3, ?4)
                 ON CONFLICT(root_id) DO UPDATE SET
                   generation = root_state.generation + 1,
                   last_incremental_at = excluded.last_incremental_at,
                   dirty = excluded.dirty,
                   entry_count = excluded.entry_count",
                params![root_id, now, root_has_errors, count_entries_tx(&tx, root_id)?],
            )?;
    }
    tx.commit()?;

    Ok(RefreshStats {
      roots: config.roots.iter().filter(|root| root.enabled).count(),
      inserted,
      updated,
      deleted,
      unchanged,
      skipped: total_skipped,
      errors,
    })
  }

  pub fn repair(&mut self, config: &SearchConfig) -> rusqlite::Result<RepairStats> {
    let configured_roots = config
      .roots
      .iter()
      .filter(|root| root.enabled)
      .cloned()
      .collect::<Vec<_>>();
    let configured_paths = configured_roots
      .iter()
      .map(|root| root.path.clone())
      .collect::<HashSet<_>>();
    let dirty_paths = self
      .status()?
      .into_iter()
      .filter(|status| status.dirty && configured_paths.contains(&status.path))
      .map(|status| status.path)
      .collect::<HashSet<_>>();
    let dirty_roots = dirty_paths.len();
    if dirty_roots == 0 {
      return Ok(RepairStats {
        roots: configured_roots.len(),
        dirty_roots: 0,
        repaired_roots: 0,
        errors: Vec::new(),
      });
    }

    let repair_config = SearchConfig {
      roots: configured_roots
        .into_iter()
        .filter(|root| dirty_paths.contains(&root.path))
        .collect(),
    };
    let refresh_stats = self.refresh(&repair_config)?;
    let repaired_roots = self
      .status()?
      .into_iter()
      .filter(|status| dirty_paths.contains(&status.path) && !status.dirty)
      .count();

    Ok(RepairStats {
      roots: config.roots.iter().filter(|root| root.enabled).count(),
      dirty_roots,
      repaired_roots,
      errors: refresh_stats.errors,
    })
  }

  pub fn mark_root_dirty(&mut self, root: &SearchRoot) -> rusqlite::Result<()> {
    let tx = self.conn.transaction()?;
    let root_id = upsert_root_tx(&tx, root)?;
    tx.execute(
            "INSERT INTO root_state (root_id, generation, last_full_scan_at, last_incremental_at, dirty, entry_count)
             VALUES (?1, COALESCE((SELECT generation FROM root_state WHERE root_id = ?1), 0), NULL, NULL, 1, ?2)
             ON CONFLICT(root_id) DO UPDATE SET dirty = 1",
            params![root_id, count_entries_tx(&tx, root_id)?],
        )?;
    tx.commit()
  }

  pub fn search(
    &self,
    config: &SearchConfig,
    query: &SearchQuery,
  ) -> rusqlite::Result<Vec<SearchResult>> {
    Ok(self.search_with_metrics(config, query)?.results)
  }

  pub fn search_with_metrics(
    &self,
    config: &SearchConfig,
    query: &SearchQuery,
  ) -> rusqlite::Result<IndexedSearchOutcome> {
    let mut entry_ids = matching_entry_ids(&self.conn, config, query)?;
    let candidate_count = entry_ids.len();
    if entry_ids.is_empty() {
      return Ok(IndexedSearchOutcome {
        candidate_count,
        results: Vec::new(),
      });
    }
    entry_ids.sort_unstable();

    let mut results = Vec::new();
    for entry_id in entry_ids {
      if let Some(row) = load_entry(&self.conn, entry_id)? {
        let candidate = SearchCandidate {
          path: row.path,
          kind: row.kind,
          provider: "sqlite".to_string(),
        };
        let (score, matches) = score_candidate(query, &candidate, row.root_priority);
        if score > 0 && extension_allowed(&candidate.path, query) {
          results.push(SearchResult {
            path: candidate.path,
            score,
            provider: candidate.provider,
            matches,
          });
        }
      }
    }

    results.sort_by(|left, right| {
      right
        .score
        .cmp(&left.score)
        .then_with(|| left.path.cmp(&right.path))
    });
    results.truncate(query.limit);
    Ok(IndexedSearchOutcome {
      candidate_count,
      results,
    })
  }

  pub fn status(&self) -> rusqlite::Result<Vec<RootStatus>> {
    let mut stmt = self.conn.prepare(
      "SELECT r.path, COALESCE(s.entry_count, 0), COALESCE(s.generation, 0), COALESCE(s.dirty, 0),
                    s.last_full_scan_at, s.last_incremental_at
             FROM roots r
             LEFT JOIN root_state s ON s.root_id = r.id
             ORDER BY r.path",
    )?;
    let rows = stmt.query_map([], |row| {
      let entry_count: i64 = row.get(1)?;
      Ok(RootStatus {
        path: PathBuf::from(row.get::<_, String>(0)?),
        entry_count: usize::try_from(entry_count).unwrap_or(0),
        generation: row.get(2)?,
        dirty: row.get::<_, i64>(3)? != 0,
        last_full_scan_at: row.get(4)?,
        last_incremental_at: row.get(5)?,
      })
    })?;
    rows.collect()
  }

  fn migrate(&self) -> rusqlite::Result<()> {
    let current = schema_version(&self.conn)?;
    if current > SCHEMA_VERSION {
      return Err(unsupported_schema_version(current));
    }
    self.conn.execute_batch(
      "
            CREATE TABLE IF NOT EXISTS roots (
              id INTEGER PRIMARY KEY,
              path TEXT NOT NULL UNIQUE,
              enabled INTEGER NOT NULL,
              priority INTEGER NOT NULL DEFAULT 0,
              include_hidden INTEGER NOT NULL DEFAULT 0,
              include_ignored INTEGER NOT NULL DEFAULT 0,
              updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS entries (
              id INTEGER PRIMARY KEY,
              root_id INTEGER NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
              path TEXT NOT NULL,
              path_lower TEXT NOT NULL DEFAULT '',
              name TEXT NOT NULL,
              name_lower TEXT NOT NULL,
              extension TEXT,
              kind INTEGER NOT NULL,
              size INTEGER,
              mtime INTEGER,
              hidden INTEGER NOT NULL DEFAULT 0,
              ignored INTEGER NOT NULL DEFAULT 0,
              sensitive INTEGER NOT NULL DEFAULT 0,
              deleted INTEGER NOT NULL DEFAULT 0,
              indexed_at INTEGER NOT NULL,
              UNIQUE(root_id, path)
            );

            CREATE TABLE IF NOT EXISTS terms (
              term TEXT NOT NULL,
              entry_id INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
              field INTEGER NOT NULL,
              weight INTEGER NOT NULL,
              PRIMARY KEY(term, entry_id, field)
            );

            CREATE TABLE IF NOT EXISTS root_state (
              root_id INTEGER PRIMARY KEY REFERENCES roots(id) ON DELETE CASCADE,
              generation INTEGER NOT NULL,
              last_full_scan_at INTEGER,
              last_incremental_at INTEGER,
              dirty INTEGER NOT NULL DEFAULT 0,
              entry_count INTEGER NOT NULL DEFAULT 0
            );

            CREATE INDEX IF NOT EXISTS idx_entries_root_deleted ON entries(root_id, deleted);
            CREATE INDEX IF NOT EXISTS idx_entries_path ON entries(path);
            CREATE INDEX IF NOT EXISTS idx_entries_ext ON entries(extension);
            CREATE INDEX IF NOT EXISTS idx_terms_term ON terms(term);
            CREATE INDEX IF NOT EXISTS idx_terms_entry ON terms(entry_id);
            ",
    )?;
    ensure_entries_path_lower_column(&self.conn)?;
    self.conn.execute(
      "UPDATE entries SET path_lower = lower(path) WHERE path_lower = ''",
      [],
    )?;
    self.conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_entries_root_deleted_path_lower ON entries(root_id, deleted, path_lower)",
            [],
        )?;
    if current < SCHEMA_VERSION {
      set_schema_version(&self.conn, SCHEMA_VERSION)?;
    }
    Ok(())
  }
}

#[derive(Debug)]
struct EntryRow {
  path: PathBuf,
  kind: EntryKind,
  root_priority: i32,
}

#[derive(Debug, Clone)]
struct ExistingEntry {
  id: i64,
  kind: EntryKind,
  size: Option<i64>,
  mtime: Option<i64>,
  hidden: bool,
  ignored: bool,
  sensitive: bool,
  deleted: bool,
}

fn upsert_root_tx(tx: &rusqlite::Transaction<'_>, root: &SearchRoot) -> rusqlite::Result<i64> {
  let now = unix_now();
  tx.execute(
    "INSERT INTO roots (path, enabled, priority, include_hidden, include_ignored, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(path) DO UPDATE SET
           enabled = excluded.enabled,
           priority = excluded.priority,
           include_hidden = excluded.include_hidden,
           include_ignored = excluded.include_ignored,
           updated_at = excluded.updated_at",
    params![
      root.path.to_string_lossy(),
      root.enabled,
      root.priority,
      root.include_hidden,
      root.include_ignored,
      now
    ],
  )?;
  tx.query_row(
    "SELECT id FROM roots WHERE path = ?1",
    params![root.path.to_string_lossy()],
    |row| row.get(0),
  )
}

fn load_existing_entries_tx(
  tx: &rusqlite::Transaction<'_>,
  root_id: i64,
) -> rusqlite::Result<HashMap<String, ExistingEntry>> {
  let mut stmt = tx.prepare(
    "SELECT path, id, kind, size, mtime, hidden, ignored, sensitive, deleted
         FROM entries
         WHERE root_id = ?1",
  )?;
  let rows = stmt.query_map(params![root_id], |row| {
    Ok((
      row.get::<_, String>(0)?,
      ExistingEntry {
        id: row.get(1)?,
        kind: int_to_kind(row.get(2)?),
        size: row.get(3)?,
        mtime: row.get(4)?,
        hidden: row.get::<_, i64>(5)? != 0,
        ignored: row.get::<_, i64>(6)? != 0,
        sensitive: row.get::<_, i64>(7)? != 0,
        deleted: row.get::<_, i64>(8)? != 0,
      },
    ))
  })?;

  let mut entries = HashMap::new();
  for row in rows {
    let (path, entry) = row?;
    entries.insert(path, entry);
  }
  Ok(entries)
}

fn insert_entry_tx(
  tx: &rusqlite::Transaction<'_>,
  root_id: i64,
  entry: &kfs_crawler::CrawledEntry,
  indexed_at: i64,
) -> rusqlite::Result<i64> {
  let name = entry
    .path
    .file_name()
    .and_then(|value| value.to_str())
    .unwrap_or_default()
    .to_string();
  let extension = entry
    .path
    .extension()
    .and_then(|value| value.to_str())
    .map(|value| value.to_ascii_lowercase());
  tx.execute(
        "INSERT INTO entries
         (root_id, path, path_lower, name, name_lower, extension, kind, size, mtime, hidden, ignored, sensitive, deleted, indexed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 0, ?13)",
        params![
            root_id,
            entry.path.to_string_lossy(),
            entry.path.to_string_lossy().to_ascii_lowercase(),
            name,
            name.to_ascii_lowercase(),
            extension,
            kind_to_int(entry.kind),
            entry_size_i64(entry),
            entry.mtime,
            entry.decision.hidden,
            entry.decision.ignored,
            entry.decision.sensitive,
            indexed_at
        ],
    )?;
  Ok(tx.last_insert_rowid())
}

fn update_entry_tx(
  tx: &rusqlite::Transaction<'_>,
  entry_id: i64,
  entry: &kfs_crawler::CrawledEntry,
  indexed_at: i64,
) -> rusqlite::Result<()> {
  let name = entry
    .path
    .file_name()
    .and_then(|value| value.to_str())
    .unwrap_or_default()
    .to_string();
  let extension = entry
    .path
    .extension()
    .and_then(|value| value.to_str())
    .map(|value| value.to_ascii_lowercase());
  tx.execute(
    "UPDATE entries
         SET path = ?1,
             path_lower = ?2,
             name = ?3,
             name_lower = ?4,
             extension = ?5,
             kind = ?6,
             size = ?7,
             mtime = ?8,
             hidden = ?9,
             ignored = ?10,
             sensitive = ?11,
             deleted = 0,
             indexed_at = ?12
         WHERE id = ?13",
    params![
      entry.path.to_string_lossy(),
      entry.path.to_string_lossy().to_ascii_lowercase(),
      name,
      name.to_ascii_lowercase(),
      extension,
      kind_to_int(entry.kind),
      entry_size_i64(entry),
      entry.mtime,
      entry.decision.hidden,
      entry.decision.ignored,
      entry.decision.sensitive,
      indexed_at,
      entry_id
    ],
  )?;
  tx.execute("DELETE FROM terms WHERE entry_id = ?1", params![entry_id])?;
  insert_terms_tx(tx, entry_id, &entry.path)
}

fn mark_entry_deleted_tx(
  tx: &rusqlite::Transaction<'_>,
  entry_id: i64,
  indexed_at: i64,
) -> rusqlite::Result<()> {
  tx.execute(
    "UPDATE entries SET deleted = 1, indexed_at = ?1 WHERE id = ?2",
    params![indexed_at, entry_id],
  )?;
  tx.execute("DELETE FROM terms WHERE entry_id = ?1", params![entry_id])?;
  Ok(())
}

fn insert_terms_tx(
  tx: &rusqlite::Transaction<'_>,
  entry_id: i64,
  path: &Path,
) -> rusqlite::Result<()> {
  let name = path
    .file_name()
    .and_then(|value| value.to_str())
    .unwrap_or_default();
  for term in tokenize(name) {
    tx.execute(
      "INSERT OR IGNORE INTO terms (term, entry_id, field, weight) VALUES (?1, ?2, 1, 100)",
      params![term, entry_id],
    )?;
  }
  if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
    tx.execute(
      "INSERT OR IGNORE INTO terms (term, entry_id, field, weight) VALUES (?1, ?2, 2, 50)",
      params![extension.to_ascii_lowercase(), entry_id],
    )?;
  }
  for component in path
    .components()
    .filter_map(|component| component.as_os_str().to_str())
  {
    for term in tokenize(component) {
      tx.execute(
        "INSERT OR IGNORE INTO terms (term, entry_id, field, weight) VALUES (?1, ?2, 3, 10)",
        params![term, entry_id],
      )?;
    }
  }
  Ok(())
}

fn entry_changed(existing: &ExistingEntry, entry: &kfs_crawler::CrawledEntry) -> bool {
  existing.deleted
    || existing.kind != entry.kind
    || existing.size != entry_size_i64(entry)
    || existing.mtime != entry.mtime
    || existing.hidden != entry.decision.hidden
    || existing.ignored != entry.decision.ignored
    || existing.sensitive != entry.decision.sensitive
}

fn entry_size_i64(entry: &kfs_crawler::CrawledEntry) -> Option<i64> {
  entry.size.and_then(|size| i64::try_from(size).ok())
}

fn count_entries_tx(tx: &rusqlite::Transaction<'_>, root_id: i64) -> rusqlite::Result<i64> {
  tx.query_row(
    "SELECT COUNT(*) FROM entries WHERE root_id = ?1 AND deleted = 0",
    params![root_id],
    |row| row.get(0),
  )
}

fn mark_root_state_dirty_tx(tx: &rusqlite::Transaction<'_>, root_id: i64) -> rusqlite::Result<()> {
  tx.execute(
        "INSERT INTO root_state (root_id, generation, last_full_scan_at, last_incremental_at, dirty, entry_count)
         VALUES (?1, COALESCE((SELECT generation FROM root_state WHERE root_id = ?1), 0), NULL, NULL, 1, ?2)
         ON CONFLICT(root_id) DO UPDATE SET
           dirty = 1,
           entry_count = excluded.entry_count",
        params![root_id, count_entries_tx(tx, root_id)?],
    )?;
  Ok(())
}

fn matching_entry_ids(
  conn: &Connection,
  config: &SearchConfig,
  query: &SearchQuery,
) -> rusqlite::Result<Vec<i64>> {
  let root_ids = root_ids_for_config(conn, config)?;
  if root_ids.is_empty() {
    return Ok(Vec::new());
  }

  let query_terms = tokenize(&query.query);
  let root_filter = placeholders(root_ids.len());
  let root_values = root_ids
    .iter()
    .copied()
    .map(Value::from)
    .collect::<Vec<_>>();
  let mut intersection: Option<HashSet<i64>> = None;
  for term in query_terms {
    let (term_clause, mut values) = term_prefix_clause(&term);
    values.extend(root_values.iter().cloned());
    let sql = format!(
      "SELECT DISTINCT e.id
             FROM terms t
             JOIN entries e ON e.id = t.entry_id
             WHERE {term_clause} AND e.deleted = 0 AND e.root_id IN ({root_filter})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let ids = stmt
      .query_map(params_from_iter(values.iter()), |row| row.get::<_, i64>(0))?
      .collect::<rusqlite::Result<HashSet<_>>>()?;
    intersection = Some(match intersection {
      Some(existing) => existing.intersection(&ids).copied().collect(),
      None => ids,
    });
  }

  let Some(ids) = intersection else {
    return all_entry_ids_for_roots(conn, &root_filter, &root_values);
  };
  if ids.is_empty() {
    return all_entry_ids_for_roots(conn, &root_filter, &root_values);
  }
  Ok(ids.into_iter().collect())
}

fn all_entry_ids_for_roots(
  conn: &Connection,
  root_filter: &str,
  root_values: &[Value],
) -> rusqlite::Result<Vec<i64>> {
  let sql = format!("SELECT id FROM entries WHERE deleted = 0 AND root_id IN ({root_filter})");
  let mut stmt = conn.prepare(&sql)?;
  let ids = stmt
    .query_map(params_from_iter(root_values.iter()), |row| {
      row.get::<_, i64>(0)
    })?
    .collect::<rusqlite::Result<Vec<_>>>()?;
  Ok(ids)
}

fn placeholders(count: usize) -> String {
  std::iter::repeat_n("?", count)
    .collect::<Vec<_>>()
    .join(",")
}

fn term_prefix_clause(term: &str) -> (String, Vec<Value>) {
  if let Some(upper_bound) = ascii_prefix_upper_bound(term) {
    (
      "t.term >= ? AND t.term < ?".to_string(),
      vec![Value::from(term.to_string()), Value::from(upper_bound)],
    )
  } else {
    (
      "t.term LIKE ?".to_string(),
      vec![Value::from(format!("{term}%"))],
    )
  }
}

fn ascii_prefix_upper_bound(prefix: &str) -> Option<String> {
  if prefix.is_empty() || !prefix.is_ascii() {
    return None;
  }
  let mut bytes = prefix.as_bytes().to_vec();
  for index in (0..bytes.len()).rev() {
    if bytes[index] < 0x7f {
      bytes[index] += 1;
      bytes.truncate(index + 1);
      return String::from_utf8(bytes).ok();
    }
  }
  None
}

fn schema_version(conn: &Connection) -> rusqlite::Result<i64> {
  conn.query_row("PRAGMA user_version", [], |row| row.get(0))
}

fn set_schema_version(conn: &Connection, version: i64) -> rusqlite::Result<()> {
  conn.execute_batch(&format!("PRAGMA user_version = {version};"))
}

/// A database written by a newer schema must never be modified in place.
/// Callers recover by recreating the index file and rebuilding from source
/// directories, which never touches user data.
fn unsupported_schema_version(found: i64) -> rusqlite::Error {
  rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_NOTADB),
        Some(format!(
            "file-search index schema version {found} is newer than supported version {SCHEMA_VERSION}; recreate the index file and rebuild"
        )),
    )
}

fn ensure_entries_path_lower_column(conn: &Connection) -> rusqlite::Result<()> {
  if !column_exists(conn, "entries", "path_lower")? {
    conn.execute(
      "ALTER TABLE entries ADD COLUMN path_lower TEXT NOT NULL DEFAULT ''",
      [],
    )?;
  }
  Ok(())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
  let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
  let columns = stmt
    .query_map([], |row| row.get::<_, String>(1))?
    .collect::<rusqlite::Result<HashSet<_>>>()?;
  Ok(columns.contains(column))
}

fn root_ids_for_config(conn: &Connection, config: &SearchConfig) -> rusqlite::Result<Vec<i64>> {
  let mut ids = Vec::new();
  for root in config.roots.iter().filter(|root| root.enabled) {
    if let Some(id) = conn
      .query_row(
        "SELECT id FROM roots WHERE path = ?1",
        params![root.path.to_string_lossy()],
        |row| row.get::<_, i64>(0),
      )
      .optional()?
    {
      ids.push(id);
    }
  }
  Ok(ids)
}

fn load_entry(conn: &Connection, entry_id: i64) -> rusqlite::Result<Option<EntryRow>> {
  conn
    .query_row(
      "SELECT e.path, e.kind, r.priority
         FROM entries e
         JOIN roots r ON r.id = e.root_id
         WHERE e.id = ?1 AND e.deleted = 0",
      params![entry_id],
      |row| {
        Ok(EntryRow {
          path: PathBuf::from(row.get::<_, String>(0)?),
          kind: int_to_kind(row.get(1)?),
          root_priority: row.get(2)?,
        })
      },
    )
    .optional()
}

fn extension_allowed(path: &Path, query: &SearchQuery) -> bool {
  query.extensions.is_empty()
    || path
      .extension()
      .and_then(|value| value.to_str())
      .is_some_and(|extension| {
        query.extensions.iter().any(|allowed| {
          allowed
            .trim_start_matches('.')
            .eq_ignore_ascii_case(extension)
        })
      })
}

fn kind_to_int(kind: EntryKind) -> i64 {
  match kind {
    EntryKind::File => 1,
    EntryKind::Directory => 2,
    EntryKind::Other => 3,
  }
}

fn int_to_kind(value: i64) -> EntryKind {
  match value {
    1 => EntryKind::File,
    2 => EntryKind::Directory,
    _ => EntryKind::Other,
  }
}

fn unix_now() -> i64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .ok()
    .and_then(|duration| i64::try_from(duration.as_secs()).ok())
    .unwrap_or(0)
}

fn sqlite_error(error: rusqlite::Error) -> BackendError {
  BackendError::new(error.to_string())
}

pub fn match_names(matches: &[MatchKind]) -> Vec<&'static str> {
  matches
    .iter()
    .map(|kind| match kind {
      MatchKind::Empty => "Empty",
      MatchKind::ExactBasename => "ExactBasename",
      MatchKind::BasenamePrefix => "BasenamePrefix",
      MatchKind::BasenameToken => "BasenameToken",
      MatchKind::PathComponent => "PathComponent",
      MatchKind::Substring => "Substring",
      MatchKind::Fuzzy => "Fuzzy",
      MatchKind::Extension => "Extension",
      MatchKind::RootPriority => "RootPriority",
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use std::fs;

  use kfs_crawler::remove_dir_all_if_exists;

  use super::*;

  fn temp_dir(name: &str) -> PathBuf {
    let nonce = unix_now();
    std::env::temp_dir().join(format!("kfs-index-{name}-{nonce}-{}", std::process::id()))
  }

  #[test]
  fn rebuild_indexes_allowed_files_and_searches_terms() {
    let root = temp_dir("rebuild");
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
    fs::write(root.join("docs/auth-providers.md"), "auth\n").unwrap();
    fs::write(root.join("node_modules/pkg/auth-providers.md"), "ignored\n").unwrap();

    let mut index = SqliteIndex::open_memory().unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    };
    let stats = index.rebuild(&config).unwrap();
    let results = index
      .search(&config, &SearchQuery::new("auth providers"))
      .unwrap();
    remove_dir_all_if_exists(&root).unwrap();

    assert!(stats.entries >= 2);
    assert!(stats.skipped >= 1);
    assert_eq!(results.len(), 1);
    assert!(results[0].path.ends_with("docs/auth-providers.md"));
    assert_eq!(results[0].provider, "sqlite");
  }

  #[test]
  fn rebuild_preserves_existing_entries_and_marks_dirty_when_crawl_errors() {
    let root = temp_dir("rebuild-error");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("preserved-file.md"), "old\n").unwrap();

    let mut index = SqliteIndex::open_memory().unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    };
    index.rebuild(&config).unwrap();
    remove_dir_all_if_exists(&root).unwrap();

    let stats = index.rebuild(&config).unwrap();
    let status = index.status().unwrap();
    let results = index
      .search(&config, &SearchQuery::new("preserved file"))
      .unwrap();

    assert!(!stats.errors.is_empty());
    assert_eq!(status.len(), 1);
    assert!(status[0].dirty);
    assert_eq!(results.len(), 1);
  }

  #[test]
  fn status_reports_rebuilt_root() {
    let root = temp_dir("status");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();

    let mut index = SqliteIndex::open_memory().unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    };
    let expected_root = config.roots[0].path.clone();
    index.rebuild(&config).unwrap();
    let status = index.status().unwrap();
    remove_dir_all_if_exists(&root).unwrap();

    assert_eq!(status.len(), 1);
    assert_eq!(status[0].path, expected_root);
    assert!(status[0].entry_count >= 1);
    assert!(!status[0].dirty);
  }

  #[test]
  fn refresh_indexes_added_files_and_marks_missing_files_deleted() {
    let root = temp_dir("refresh");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("old-file.md"), "old\n").unwrap();

    let mut index = SqliteIndex::open_memory().unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    };
    index.rebuild(&config).unwrap();
    fs::remove_file(root.join("old-file.md")).unwrap();
    fs::write(root.join("new-file.md"), "new\n").unwrap();

    let stats = index.refresh(&config).unwrap();
    let old_results = index
      .search(&config, &SearchQuery::new("old file"))
      .unwrap();
    let new_results = index
      .search(&config, &SearchQuery::new("new file"))
      .unwrap();
    remove_dir_all_if_exists(&root).unwrap();

    assert!(stats.inserted >= 1);
    assert!(stats.deleted >= 1);
    assert!(old_results.is_empty());
    assert_eq!(new_results.len(), 1);
  }

  #[test]
  fn refresh_preserves_existing_entries_and_marks_dirty_when_crawl_errors() {
    let root = temp_dir("refresh-error");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("still-indexed.md"), "old\n").unwrap();

    let mut index = SqliteIndex::open_memory().unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    };
    index.rebuild(&config).unwrap();
    remove_dir_all_if_exists(&root).unwrap();

    let stats = index.refresh(&config).unwrap();
    let status = index.status().unwrap();
    let results = index
      .search(&config, &SearchQuery::new("still indexed"))
      .unwrap();

    assert!(!stats.errors.is_empty());
    assert_eq!(stats.deleted, 0);
    assert_eq!(status.len(), 1);
    assert!(status[0].dirty);
    assert_eq!(results.len(), 1);
  }

  #[test]
  fn refresh_updates_changed_file_metadata() {
    let root = temp_dir("refresh-update");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("same-file.md"), "a\n").unwrap();

    let mut index = SqliteIndex::open_memory().unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    };
    index.rebuild(&config).unwrap();
    fs::write(root.join("same-file.md"), "longer content\n").unwrap();

    let stats = index.refresh(&config).unwrap();
    remove_dir_all_if_exists(&root).unwrap();

    assert!(stats.updated >= 1);
  }

  #[test]
  fn repair_refreshes_dirty_roots() {
    let root = temp_dir("repair");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("repair-file.md"), "repair\n").unwrap();

    let mut index = SqliteIndex::open_memory().unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    };
    index.rebuild(&config).unwrap();
    index.mark_root_dirty(&config.roots[0]).unwrap();
    assert!(index.status().unwrap()[0].dirty);

    let stats = index.repair(&config).unwrap();
    let status = index.status().unwrap();
    remove_dir_all_if_exists(&root).unwrap();

    assert_eq!(stats.dirty_roots, 1);
    assert_eq!(stats.repaired_roots, 1);
    assert!(!status[0].dirty);
  }

  #[test]
  fn search_with_metrics_reports_narrowed_candidate_count() {
    let root = temp_dir("metrics");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("needleunique-target.md"), "needle\n").unwrap();
    fs::write(root.join("other-target.md"), "other\n").unwrap();

    let mut index = SqliteIndex::open_memory().unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    };
    index.rebuild(&config).unwrap();
    let outcome = index
      .search_with_metrics(&config, &SearchQuery::new("needleunique"))
      .unwrap();
    remove_dir_all_if_exists(&root).unwrap();

    assert_eq!(outcome.candidate_count, 1);
    assert_eq!(outcome.results.len(), 1);
    assert!(outcome.results[0].path.ends_with("needleunique-target.md"));
  }

  #[test]
  fn indexed_search_preserves_compact_substring_fallback() {
    let root = temp_dir("substring");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "lib\n").unwrap();

    let mut index = SqliteIndex::open_memory().unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    };
    index.rebuild(&config).unwrap();
    let outcome = index
      .search_with_metrics(&config, &SearchQuery::new("srclib"))
      .unwrap();
    remove_dir_all_if_exists(&root).unwrap();

    assert_eq!(outcome.results.len(), 1);
    assert!(outcome.results[0].path.ends_with("src/lib.rs"));
  }

  #[test]
  fn indexed_search_finds_compacted_ordered_queries_across_filename_tokens() {
    let root = temp_dir("ordered-fuzzy");
    fs::create_dir_all(&root).unwrap();
    fs::write(
      root.join("AWS Certified Solutions Architect Associate SAA-C03.pdf"),
      "pdf\n",
    )
    .unwrap();

    let mut index = SqliteIndex::open_memory().unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    };
    index.rebuild(&config).unwrap();
    let outcome = index
      .search_with_metrics(&config, &SearchQuery::new("awspdf"))
      .unwrap();
    remove_dir_all_if_exists(&root).unwrap();

    assert_eq!(outcome.results.len(), 1);
    assert!(outcome.results[0]
      .path
      .ends_with("AWS Certified Solutions Architect Associate SAA-C03.pdf"));
  }

  #[test]
  fn ascii_prefix_upper_bound_advances_last_ascii_byte() {
    assert_eq!(ascii_prefix_upper_bound("abc"), Some("abd".to_string()));
    assert_eq!(ascii_prefix_upper_bound("abz"), Some("ab{".to_string()));
    assert_eq!(ascii_prefix_upper_bound("é"), None);
  }

  #[test]
  fn migrate_adds_path_lower_before_creating_path_lower_index() {
    let conn = Connection::open_in_memory().unwrap();
    conn
      .execute_batch(
        "
            CREATE TABLE entries (
              id INTEGER PRIMARY KEY,
              root_id INTEGER NOT NULL,
              path TEXT NOT NULL,
              name TEXT NOT NULL,
              name_lower TEXT NOT NULL,
              extension TEXT,
              kind INTEGER NOT NULL,
              size INTEGER,
              mtime INTEGER,
              hidden INTEGER NOT NULL DEFAULT 0,
              ignored INTEGER NOT NULL DEFAULT 0,
              sensitive INTEGER NOT NULL DEFAULT 0,
              deleted INTEGER NOT NULL DEFAULT 0,
              indexed_at INTEGER NOT NULL,
              UNIQUE(root_id, path)
            );
            ",
      )
      .unwrap();
    let index = SqliteIndex { conn };

    index.migrate().unwrap();

    let has_path_lower = column_exists(&index.conn, "entries", "path_lower").unwrap();
    assert!(has_path_lower);
  }

  #[test]
  fn migrate_upgrades_legacy_database_and_stamps_schema_version() {
    let conn = Connection::open_in_memory().unwrap();
    conn
      .execute_batch(
        "
            CREATE TABLE roots (
              id INTEGER PRIMARY KEY,
              path TEXT NOT NULL UNIQUE,
              enabled INTEGER NOT NULL,
              priority INTEGER NOT NULL DEFAULT 0,
              include_hidden INTEGER NOT NULL DEFAULT 0,
              include_ignored INTEGER NOT NULL DEFAULT 0,
              updated_at INTEGER NOT NULL
            );
            CREATE TABLE entries (
              id INTEGER PRIMARY KEY,
              root_id INTEGER NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
              path TEXT NOT NULL,
              name TEXT NOT NULL,
              name_lower TEXT NOT NULL,
              extension TEXT,
              kind INTEGER NOT NULL,
              size INTEGER,
              mtime INTEGER,
              hidden INTEGER NOT NULL DEFAULT 0,
              ignored INTEGER NOT NULL DEFAULT 0,
              sensitive INTEGER NOT NULL DEFAULT 0,
              deleted INTEGER NOT NULL DEFAULT 0,
              indexed_at INTEGER NOT NULL,
              UNIQUE(root_id, path)
            );
            CREATE TABLE terms (
              term TEXT NOT NULL,
              entry_id INTEGER NOT NULL REFERENCES entries(id) ON DELETE CASCADE,
              field INTEGER NOT NULL,
              weight INTEGER NOT NULL,
              PRIMARY KEY(term, entry_id, field)
            );
            CREATE TABLE root_state (
              root_id INTEGER PRIMARY KEY REFERENCES roots(id) ON DELETE CASCADE,
              generation INTEGER NOT NULL,
              last_full_scan_at INTEGER,
              last_incremental_at INTEGER,
              dirty INTEGER NOT NULL DEFAULT 0,
              entry_count INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO roots (id, path, enabled, priority, updated_at)
              VALUES (1, '/tmp/legacy', 1, 0, 0);
            INSERT INTO entries (id, root_id, path, name, name_lower, kind, indexed_at)
              VALUES (1, 1, '/tmp/legacy/Report.PDF', 'Report.PDF', 'report.pdf', 0, 0);
            ",
      )
      .unwrap();
    let index = SqliteIndex { conn };

    index.migrate().unwrap();

    assert_eq!(schema_version(&index.conn).unwrap(), SCHEMA_VERSION);
    let lowered: String = index
      .conn
      .query_row("SELECT path_lower FROM entries WHERE id = 1", [], |row| {
        row.get(0)
      })
      .unwrap();
    assert_eq!(lowered, "/tmp/legacy/report.pdf");
    let original: String = index
      .conn
      .query_row("SELECT path FROM entries WHERE id = 1", [], |row| {
        row.get(0)
      })
      .unwrap();
    assert_eq!(original, "/tmp/legacy/Report.PDF");
  }

  #[test]
  fn open_memory_stamps_current_schema_version() {
    let index = SqliteIndex::open_memory().unwrap();
    assert_eq!(schema_version(&index.conn).unwrap(), SCHEMA_VERSION);
  }

  #[test]
  fn migrate_refuses_newer_schema_without_touching_the_database() {
    let conn = Connection::open_in_memory().unwrap();
    set_schema_version(&conn, SCHEMA_VERSION + 1).unwrap();
    let index = SqliteIndex { conn };

    let error = index.migrate().unwrap_err();

    match error {
      rusqlite::Error::SqliteFailure(code, message) => {
        assert_eq!(code.code, rusqlite::ffi::ErrorCode::NotADatabase);
        assert!(message.unwrap().contains("newer than supported"));
      }
      other => panic!("expected schema version failure, got {other}"),
    }
    assert_eq!(schema_version(&index.conn).unwrap(), SCHEMA_VERSION + 1);
  }
}
