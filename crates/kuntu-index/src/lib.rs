//! SQLite-compatible metadata index for the file search prototype, backed by
//! the `turso` engine (an in-process Rust rewrite of SQLite).
//!
//! This crate owns a separate file-search database. It does not use the
//! desktop app database and can be rebuilt or queried by the CLI, daemon, or
//! future adapters. The public API is synchronous; turso's async API is
//! driven through a shared executor (`block_on`), because every caller (CLI,
//! daemon, NAPI libuv workers) is synchronous.
//!
//! Engine note: `turso` is pinned to the same generation xross-store uses;
//! upgrades must move both sides together or xross builds compile two
//! different SQL engines.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use kuntu_core::{
  ranking::{score_candidate, tokenize},
  BackendError, EntryKind, IndexRebuildStats, IndexRefreshStats, IndexRepairStats, IndexRootStatus,
  MatchKind, MetadataIndex, SearchCandidate, SearchConfig, SearchQuery, SearchResult, SearchRoot,
};
use kuntu_crawler::{crawl, CrawlOptions};
use turso::{params, params_from_iter, Value};

pub type RebuildStats = IndexRebuildStats;
pub type RefreshStats = IndexRefreshStats;
pub type RepairStats = IndexRepairStats;
pub type RootStatus = IndexRootStatus;

/// Monotonic schema version of the index database, stored in `PRAGMA
/// user_version`.
///
/// History: 1 was the rusqlite era. 2 marks the turso engine. Databases from
/// version 0 (pre-versioning) or 1 are recreated rather than migrated
/// in-place — the index is derived data and rebuilding from source
/// directories never touches user data, while turso-era builds avoid
/// turso's `ALTER TABLE ADD COLUMN` bug cluster. Databases from a newer
/// version are refused with `IndexError::SchemaTooNew`.
pub const SCHEMA_VERSION: i64 = 2;

/// Synchronous facade over turso's async API. Every public method runs on
/// the caller's thread through a minimal executor; turso's base crate does
/// not require tokio (its futures are waker-based).
fn block_on<F: std::future::Future>(future: F) -> F::Output {
  futures::executor::block_on(future)
}

/// Error surface of the index. Callers only rely on `Display`; the typed
/// variants exist so the crate and its tests can distinguish recoverable
/// states (recreate the file and rebuild) from real engine failures.
#[derive(Debug)]
pub enum IndexError {
  Engine(turso::Error),
  Io(std::io::Error),
  SchemaTooNew { found: i64, supported: i64 },
  RecreateRequired,
  RecreateFailed(Box<IndexError>),
  UnrecognizedDatabase { path: PathBuf },
}

impl std::fmt::Display for IndexError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      IndexError::Engine(error) => write!(formatter, "index engine error: {error}"),
      IndexError::Io(error) => write!(formatter, "index io error: {error}"),
      IndexError::SchemaTooNew { found, supported } => write!(
        formatter,
        "file-search index schema version {found} is newer than supported version {supported}; recreate the index file and rebuild"
      ),
      IndexError::RecreateRequired => write!(
        formatter,
        "index database predates the turso engine and must be recreated"
      ),
      IndexError::UnrecognizedDatabase { path } => write!(
        formatter,
        "{} has user tables but is not a file-search index; move it aside or choose another --db, it was not modified",
        path.display()
      ),
      IndexError::RecreateFailed(error) => write!(
        formatter,
        "index recreation after legacy detection failed: {error}"
      ),
    }
  }
}

impl std::error::Error for IndexError {}

impl From<turso::Error> for IndexError {
  fn from(error: turso::Error) -> Self {
    IndexError::Engine(error)
  }
}

impl From<std::io::Error> for IndexError {
  fn from(error: std::io::Error) -> Self {
    IndexError::Io(error)
  }
}

pub type Result<T> = std::result::Result<T, IndexError>;

/// The candidate work bound for one search. A broad indexed prefix stops
/// yielding candidates here and reports [`CandidateCoverage::Truncated`]
/// instead of claiming complete coverage over an unbounded root.
pub const MAX_KFS_CANDIDATE_ROWS: usize = 50_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateCoverage {
  /// Every candidate the configured roots could offer was ranked.
  Complete,
  /// The candidate work bound was reached; results are deterministic but
  /// partial and never claim to be the best over the whole root.
  Truncated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedSearchOutcome {
  /// Candidates the SQL stages produced, after the work bound.
  pub candidate_count: usize,
  pub results: Vec<SearchResult>,
  /// More valid results existed than the requested limit.
  pub truncated: bool,
  pub coverage: CandidateCoverage,
}

#[derive(Debug)]
pub struct KuntuIndex {
  conn: turso::Connection,
}

impl MetadataIndex for KuntuIndex {
  fn rebuild_index(
    &mut self,
    config: &SearchConfig,
  ) -> std::result::Result<IndexRebuildStats, BackendError> {
    self.rebuild(config).map_err(index_error_to_backend)
  }

  fn refresh_index(
    &mut self,
    config: &SearchConfig,
  ) -> std::result::Result<IndexRefreshStats, BackendError> {
    self.refresh(config).map_err(index_error_to_backend)
  }

  fn repair_index(
    &mut self,
    config: &SearchConfig,
  ) -> std::result::Result<IndexRepairStats, BackendError> {
    self.repair(config).map_err(index_error_to_backend)
  }

  fn search_index(
    &self,
    config: &SearchConfig,
    query: &SearchQuery,
  ) -> std::result::Result<Vec<SearchResult>, BackendError> {
    self.search(config, query).map_err(index_error_to_backend)
  }

  fn status(&self) -> std::result::Result<Vec<IndexRootStatus>, BackendError> {
    KuntuIndex::status(self).map_err(index_error_to_backend)
  }
}

impl KuntuIndex {
  pub fn open(path: impl AsRef<Path>) -> Result<Self> {
    block_on(Self::open_async(path.as_ref()))
  }

  pub fn open_memory() -> Result<Self> {
    block_on(async {
      let database = turso::Builder::new_local(":memory:").build().await?;
      let conn = database.connect()?;
      let index = KuntuIndex { conn };
      index.enable_foreign_key_cascades().await?;
      index.create_schema().await?;
      index.set_schema_version(SCHEMA_VERSION).await?;
      Ok(index)
    })
  }

  async fn open_async(path: &Path) -> Result<Self> {
    // One retry: a legacy (pre-turso) or unreadable database file is deleted
    // and recreated, because the index is derived data. A second failure is
    // real and propagates.
    let mut attempts = 0;
    loop {
      attempts += 1;
      match Self::open_once(path).await {
        Ok(index) => return Ok(index),
        Err(IndexError::RecreateRequired) if attempts == 1 => {
          remove_database_files(path);
        }
        Err(IndexError::RecreateRequired) => {
          return Err(IndexError::RecreateFailed(Box::new(
            IndexError::RecreateRequired,
          )));
        }
        Err(error) => return Err(error),
      }
    }
  }

  async fn open_once(path: &Path) -> Result<Self> {
    // turso's builder takes a string; rather than alias a database to a
    // lossy-converted path (distinct byte paths collapsing onto one), refuse
    // what cannot be represented exactly.
    let Some(path_text) = path.to_str() else {
      return Err(IndexError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("index database path is not valid UTF-8: {}", path.display()),
      )));
    };
    let database = turso::Builder::new_local(path_text).build().await?;
    let conn = database.connect()?;
    let index = KuntuIndex { conn };
    index.enable_foreign_key_cascades().await?;

    let version = index.schema_version().await?;
    if version > SCHEMA_VERSION {
      return Err(IndexError::SchemaTooNew {
        found: version,
        supported: SCHEMA_VERSION,
      });
    }
    if version == 0 {
      // A fresh file reports version 0 with no tables. Version 0 with the
      // complete KFS schema is a pre-versioning rusqlite database: recreate.
      // Anything else with user tables is not ours and must never be touched
      // automatically — `roots` alone proves nothing.
      let has_tables = any_table_exists(&index.conn).await?;
      if has_tables {
        let kuntu_tables = ["roots", "entries", "terms", "root_state"];
        let mut complete = true;
        for table in kuntu_tables {
          if !table_exists(&index.conn, table).await? {
            complete = false;
            break;
          }
        }
        if complete {
          return Err(IndexError::RecreateRequired);
        }
        return Err(IndexError::UnrecognizedDatabase {
          path: path.to_path_buf(),
        });
      }
    }

    index.create_schema().await?;
    if version < SCHEMA_VERSION {
      index.set_schema_version(SCHEMA_VERSION).await?;
    }
    Ok(index)
  }

  /// The schema leans on `terms.entry_id ... ON DELETE CASCADE`; the engine
  /// only honors that while the per-connection pragma is on. Without it a
  /// rebuild's bulk entry delete leaves orphan terms behind, and row id reuse
  /// then aliases old terms onto new files.
  async fn enable_foreign_key_cascades(&self) -> Result<()> {
    self.conn.execute("PRAGMA foreign_keys = ON", ()).await?;
    Ok(())
  }

  async fn create_schema(&self) -> Result<()> {
    self.conn.execute_batch(SCHEMA_DDL).await?;
    Ok(())
  }

  async fn schema_version(&self) -> Result<i64> {
    let mut rows = self.conn.query("PRAGMA user_version", ()).await?;
    let row = rows
      .next()
      .await?
      .expect("PRAGMA user_version returns a row");
    Ok(row.get::<i64>(0)?)
  }

  async fn set_schema_version(&self, version: i64) -> Result<()> {
    self
      .conn
      .execute_batch(&format!("PRAGMA user_version = {version};"))
      .await?;
    Ok(())
  }

  pub fn rebuild(&mut self, config: &SearchConfig) -> Result<RebuildStats> {
    let now = unix_now();
    let mut total_entries = 0;
    let mut total_skipped = 0;
    let mut errors = Vec::new();

    block_on(async {
      let tx = self.conn.transaction().await?;
      for root in config.roots.iter().filter(|root| root.enabled) {
        let root_id = upsert_root_tx(&tx, root).await?;
        let root_config = SearchConfig {
          roots: vec![root.clone()],
        };
        let options = CrawlOptions::new(root_config);
        let (entries, stats) = crawl(&options);
        let root_has_errors = !stats.errors.is_empty();
        total_skipped += stats.skipped;
        errors.extend(stats.errors);

        if root_has_errors {
          mark_root_state_dirty_tx(&tx, root_id).await?;
          continue;
        }

        tx.execute("DELETE FROM entries WHERE root_id = ?1", params![root_id])
          .await?;

        for entry in entries {
          let entry_id = insert_entry_tx(&tx, root_id, &entry, now).await?;
          insert_terms_tx(&tx, entry_id, &entry.path).await?;
          total_entries += 1;
        }

        let count = count_entries_tx(&tx, root_id).await?;
        tx.execute(
          "INSERT INTO root_state (root_id, generation, last_full_scan_at, last_incremental_at, dirty, entry_count)
           VALUES (?1, COALESCE((SELECT generation + 1 FROM root_state WHERE root_id = ?1), 1), ?2, NULL, 0, ?3)
           ON CONFLICT(root_id) DO UPDATE SET
             generation = root_state.generation + 1,
             last_full_scan_at = excluded.last_full_scan_at,
             dirty = 0,
             entry_count = excluded.entry_count",
          params![root_id, now, count],
        )
        .await?;
      }
      tx.commit().await?;
      Ok::<(), IndexError>(())
    })?;

    Ok(RebuildStats {
      roots: config.roots.iter().filter(|root| root.enabled).count(),
      entries: total_entries,
      skipped: total_skipped,
      errors,
    })
  }

  pub fn refresh(&mut self, config: &SearchConfig) -> Result<RefreshStats> {
    let now = unix_now();
    let mut inserted = 0;
    let mut updated = 0;
    let mut deleted = 0;
    let mut unchanged = 0;
    let mut total_skipped = 0;
    let mut errors = Vec::new();

    block_on(async {
      let tx = self.conn.transaction().await?;
      for root in config.roots.iter().filter(|root| root.enabled) {
        let root_id = upsert_root_tx(&tx, root).await?;
        let mut existing = load_existing_entries_tx(&tx, root_id).await?;
        let root_config = SearchConfig {
          roots: vec![root.clone()],
        };
        let options = CrawlOptions::new(root_config);
        let (entries, stats) = crawl(&options);
        let root_has_errors = !stats.errors.is_empty();
        total_skipped += stats.skipped;
        errors.extend(stats.errors);

        if root_has_errors {
          mark_root_state_dirty_tx(&tx, root_id).await?;
          continue;
        }

        let mut seen_paths = HashSet::new();
        for entry in entries {
          let path_key = entry.path.to_string_lossy().into_owned();
          seen_paths.insert(path_key.clone());
          if let Some(existing_entry) = existing.remove(&path_key) {
            if entry_changed(&existing_entry, &entry) {
              update_entry_tx(&tx, existing_entry.id, &entry, now).await?;
              updated += 1;
            } else {
              unchanged += 1;
            }
          } else {
            let entry_id = insert_entry_tx(&tx, root_id, &entry, now).await?;
            insert_terms_tx(&tx, entry_id, &entry.path).await?;
            inserted += 1;
          }
        }

        for (path, existing_entry) in existing {
          if !seen_paths.contains(&path) && !existing_entry.deleted {
            mark_entry_deleted_tx(&tx, existing_entry.id, now).await?;
            deleted += 1;
          }
        }

        let count = count_entries_tx(&tx, root_id).await?;
        tx.execute(
          "INSERT INTO root_state (root_id, generation, last_full_scan_at, last_incremental_at, dirty, entry_count)
           VALUES (?1, COALESCE((SELECT generation + 1 FROM root_state WHERE root_id = ?1), 1), NULL, ?2, ?3, ?4)
           ON CONFLICT(root_id) DO UPDATE SET
             generation = root_state.generation + 1,
             last_incremental_at = excluded.last_incremental_at,
             dirty = excluded.dirty,
             entry_count = excluded.entry_count",
          params![root_id, now, root_has_errors, count],
        )
        .await?;
      }
      tx.commit().await?;
      Ok::<(), IndexError>(())
    })?;

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

  pub fn repair(&mut self, config: &SearchConfig) -> Result<RepairStats> {
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

  pub fn mark_root_dirty(&mut self, root: &SearchRoot) -> Result<()> {
    block_on(async {
      let tx = self.conn.transaction().await?;
      let root_id = upsert_root_tx(&tx, root).await?;
      let count = count_entries_tx(&tx, root_id).await?;
      tx.execute(
        "INSERT INTO root_state (root_id, generation, last_full_scan_at, last_incremental_at, dirty, entry_count)
         VALUES (?1, COALESCE((SELECT generation FROM root_state WHERE root_id = ?1), 0), NULL, NULL, 1, ?2)
         ON CONFLICT(root_id) DO UPDATE SET dirty = 1",
        params![root_id, count],
      )
      .await?;
      tx.commit().await?;
      Ok::<(), IndexError>(())
    })
  }

  pub fn search(&self, config: &SearchConfig, query: &SearchQuery) -> Result<Vec<SearchResult>> {
    Ok(self.search_with_metrics(config, query)?.results)
  }

  pub fn search_with_metrics(
    &self,
    config: &SearchConfig,
    query: &SearchQuery,
  ) -> Result<IndexedSearchOutcome> {
    block_on(async {
      let empty = IndexedSearchOutcome {
        candidate_count: 0,
        results: Vec::new(),
        truncated: false,
        coverage: CandidateCoverage::Complete,
      };
      if !query.include_files && !query.include_directories {
        return Ok(empty);
      }
      let root_ids = root_ids_for_config(&self.conn, config).await?;
      if root_ids.is_empty() {
        return Ok(empty);
      }
      let query_terms = tokenize(&query.query);
      if query_terms.is_empty() {
        // A term-less or fully-unknown query is a miss: it returns an empty
        // complete result and never falls back to scanning the whole root.
        return Ok(empty);
      }

      // Candidate selection runs in two stages. Stage 1 resolves each query
      // token to a bounded, distinct entry-id set straight off the terms
      // primary key (a range seek — the production-proven shape; turso's
      // planner full-scans the joined form). The id sets are intersected in
      // Rust. Stage 2 loads the intersection in chunks with every scope,
      // kind and policy predicate in SQL, so nothing outside the configured
      // scope, kind or policy can become a candidate, and no stage scans a
      // whole root. There is no non-indexed fallback: a token that matches
      // nothing ends the search with empty complete coverage.
      let mut intersection: Option<std::collections::BTreeSet<i64>> = None;
      let mut any_token_saturated = false;

      // A repeated token would repeat an up-to-50k-row scan without
      // changing the intersection or the ranking.
      let unique_terms: std::collections::BTreeSet<&String> = query_terms.iter().collect();
      for term in unique_terms {
        let (term_clause, mut values) = term_prefix_clause(term);
        let term_clause = term_clause.replace("t.term", "term_index.term");
        let sql = format!(
          "SELECT DISTINCT term_index.entry_id FROM terms AS term_index WHERE {term_clause} ORDER BY term_index.entry_id LIMIT ?"
        );
        values.push(Value::from(MAX_KFS_CANDIDATE_ROWS as i64 + 1));
        let mut rows = self.conn.query(&sql, params_from_iter(values)).await?;
        let mut ids = std::collections::BTreeSet::new();
        while let Some(row) = rows.next().await? {
          if ids.len() == MAX_KFS_CANDIDATE_ROWS {
            any_token_saturated = true;
            break;
          }
          ids.insert(row.get::<i64>(0)?);
        }

        intersection = Some(match intersection {
          None => ids,
          Some(previous) => previous
            .intersection(&ids)
            .copied()
            .collect::<std::collections::BTreeSet<i64>>(),
        });
        if intersection.as_ref().is_some_and(|ids| ids.is_empty()) {
          break;
        }
      }

      let intersected = intersection.unwrap_or_default();
      let id_list: Vec<i64> = intersected.into_iter().collect();

      let mut clauses = vec!["entries_main.deleted = 0".to_string()];
      clauses.push(format!(
        "entries_main.root_id IN ({})",
        placeholders(root_ids.len())
      ));
      match (query.include_files, query.include_directories) {
        (true, true) => {}
        (true, false) => {
          clauses.push(format!(
            "entries_main.kind = {}",
            kind_to_int(EntryKind::File)
          ));
        }
        (false, true) => {
          clauses.push(format!(
            "entries_main.kind = {}",
            kind_to_int(EntryKind::Directory)
          ));
        }
        (false, false) => unreachable!("handled by the early return above"),
      }
      // A row is visible when it is clean, or the query relaxes the flag,
      // or its own root was configured to allow it (the crawler only indexed
      // rows its root permitted).
      clauses.push("(entries_main.hidden = 0 OR ? OR roots_pri.include_hidden = 1)".to_string());
      clauses.push("(entries_main.ignored = 0 OR ? OR roots_pri.include_ignored = 1)".to_string());
      clauses.push("entries_main.sensitive = 0".to_string());
      if !query.extensions.is_empty() {
        clauses.push(format!(
          "entries_main.extension IN ({})",
          placeholders(query.extensions.len())
        ));
      }
      let predicate_clause = clauses.join(" AND ");

      let mut candidates: Vec<CandidateRow> = Vec::new();
      for chunk in id_list.chunks(400) {
        let sql = format!(
          "SELECT entries_main.path, entries_main.kind, roots_pri.priority, entries_main.size, entries_main.mtime
             FROM entries AS entries_main
             JOIN roots AS roots_pri ON roots_pri.id = entries_main.root_id
             WHERE entries_main.id IN ({}) AND {predicate_clause}",
          placeholders(chunk.len())
        );
        let mut values: Vec<Value> = chunk.iter().copied().map(Value::from).collect();
        values.extend(root_ids.iter().copied().map(Value::from));
        values.push(Value::from(query.include_hidden));
        values.push(Value::from(query.include_ignored));
        if !query.extensions.is_empty() {
          values.extend(
            query
              .extensions
              .iter()
              .map(|extension| Value::from(extension.trim_start_matches('.').to_ascii_lowercase())),
          );
        }
        let mut rows = self.conn.query(&sql, params_from_iter(values)).await?;
        while let Some(row) = rows.next().await? {
          let mtime: Option<i64> = row.get(4)?;
          candidates.push(CandidateRow {
            path: PathBuf::from(row.get::<String>(0)?),
            kind: int_to_kind(row.get::<i64>(1)?),
            root_priority: row.get::<i32>(2)?,
            byte_size: u64::try_from(row.get::<i64>(3).unwrap_or(-1)).ok(),
            modified_unix_seconds: u64::try_from(mtime.unwrap_or(-1)).ok(),
          });
        }
      }
      let candidate_count = candidates.len();
      let candidate_saturated = any_token_saturated;

      let mut results = Vec::new();
      for row in candidates {
        let candidate = SearchCandidate {
          path: row.path,
          kind: row.kind,
          provider: "sqlite".to_string(),
        };
        let (score, matches) = score_candidate(query, &candidate, row.root_priority);
        if score > 0 {
          results.push(SearchResult {
            path: candidate.path,
            score,
            provider: candidate.provider,
            matches,
            kind: row.kind,
            byte_size: row.byte_size,
            modified_unix_seconds: row.modified_unix_seconds,
          });
        }
      }

      results.sort_by(|left, right| {
        right
          .score
          .cmp(&left.score)
          .then_with(|| left.path.cmp(&right.path))
      });
      let truncated = results.len() > query.limit;
      results.truncate(query.limit);
      let coverage = if candidate_saturated {
        CandidateCoverage::Truncated
      } else {
        CandidateCoverage::Complete
      };
      Ok(IndexedSearchOutcome {
        candidate_count,
        results,
        truncated,
        coverage,
      })
    })
  }

  pub fn status(&self) -> Result<Vec<RootStatus>> {
    block_on(async {
      let mut rows = self.conn.query(
        "SELECT r.path, COALESCE(s.entry_count, 0), COALESCE(s.generation, 0), COALESCE(s.dirty, 0),
                s.last_full_scan_at, s.last_incremental_at
         FROM roots r
         LEFT JOIN root_state s ON s.root_id = r.id
         ORDER BY r.path",
        (),
      )
      .await?;
      let mut collected = Vec::new();
      while let Some(row) = rows.next().await? {
        collected.push(RootStatus {
          path: PathBuf::from(row.get::<String>(0)?),
          entry_count: usize::try_from(row.get::<i64>(1)?).unwrap_or(0),
          generation: row.get::<i64>(2)?,
          dirty: row.get::<i64>(3)? != 0,
          last_full_scan_at: row.get::<Option<i64>>(4)?,
          last_incremental_at: row.get::<Option<i64>>(5)?,
        });
      }
      Ok(collected)
    })
  }
}

#[derive(Debug)]
struct CandidateRow {
  path: PathBuf,
  kind: EntryKind,
  root_priority: i32,
  byte_size: Option<u64>,
  modified_unix_seconds: Option<u64>,
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

async fn upsert_root_tx(
  tx: &turso::transaction::Transaction<'_>,
  root: &SearchRoot,
) -> Result<i64> {
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
  )
  .await?;
  let mut rows = tx
    .query(
      "SELECT id FROM roots WHERE path = ?1",
      params![root.path.to_string_lossy()],
    )
    .await?;
  let row = rows.next().await?.expect("root row exists after upsert");
  Ok(row.get::<i64>(0)?)
}

async fn load_existing_entries_tx(
  tx: &turso::transaction::Transaction<'_>,
  root_id: i64,
) -> Result<HashMap<String, ExistingEntry>> {
  let mut rows = tx
    .query(
      "SELECT path, id, kind, size, mtime, hidden, ignored, sensitive, deleted
           FROM entries
           WHERE root_id = ?1",
      params![root_id],
    )
    .await?;

  let mut entries = HashMap::new();
  while let Some(row) = rows.next().await? {
    let path: String = row.get(0)?;
    entries.insert(
      path,
      ExistingEntry {
        id: row.get(1)?,
        kind: int_to_kind(row.get(2)?),
        size: row.get(3)?,
        mtime: row.get(4)?,
        hidden: row.get::<i64>(5)? != 0,
        ignored: row.get::<i64>(6)? != 0,
        sensitive: row.get::<i64>(7)? != 0,
        deleted: row.get::<i64>(8)? != 0,
      },
    );
  }
  Ok(entries)
}

async fn insert_entry_tx(
  tx: &turso::transaction::Transaction<'_>,
  root_id: i64,
  entry: &kuntu_crawler::CrawledEntry,
  indexed_at: i64,
) -> Result<i64> {
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
  let name_lower = name.to_ascii_lowercase();
  tx.execute(
        "INSERT INTO entries
         (root_id, path, path_lower, name, name_lower, extension, kind, size, mtime, hidden, ignored, sensitive, deleted, indexed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 0, ?13)",
        params![
            root_id,
            entry.path.to_string_lossy(),
            entry.path.to_string_lossy().to_ascii_lowercase(),
            name,
            name_lower,
            extension,
            kind_to_int(entry.kind),
            entry_size_i64(entry),
            entry.mtime,
            entry.decision.hidden,
            entry.decision.ignored,
            entry.decision.sensitive,
            indexed_at
        ],
    )
    .await?;
  // The (root_id, path) pair is unique, so the id of the row this call just
  // wrote is recovered explicitly rather than through a rowid cursor API.
  let mut rows = tx
    .query(
      "SELECT id FROM entries WHERE root_id = ?1 AND path = ?2",
      params![root_id, entry.path.to_string_lossy()],
    )
    .await?;
  let row = rows.next().await?.expect("inserted entry row exists");
  Ok(row.get::<i64>(0)?)
}

async fn update_entry_tx(
  tx: &turso::transaction::Transaction<'_>,
  entry_id: i64,
  entry: &kuntu_crawler::CrawledEntry,
  indexed_at: i64,
) -> Result<()> {
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
  let name_lower = name.to_ascii_lowercase();
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
      name_lower,
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
  )
  .await?;
  tx.execute("DELETE FROM terms WHERE entry_id = ?1", params![entry_id])
    .await?;
  insert_terms_tx(tx, entry_id, &entry.path).await
}

async fn mark_entry_deleted_tx(
  tx: &turso::transaction::Transaction<'_>,
  entry_id: i64,
  indexed_at: i64,
) -> Result<()> {
  tx.execute(
    "UPDATE entries SET deleted = 1, indexed_at = ?1 WHERE id = ?2",
    params![indexed_at, entry_id],
  )
  .await?;
  tx.execute("DELETE FROM terms WHERE entry_id = ?1", params![entry_id])
    .await?;
  Ok(())
}

async fn insert_terms_tx(
  tx: &turso::transaction::Transaction<'_>,
  entry_id: i64,
  path: &Path,
) -> Result<()> {
  let name = path
    .file_name()
    .and_then(|value| value.to_str())
    .unwrap_or_default();
  for term in tokenize(name) {
    tx.execute(
      "INSERT OR IGNORE INTO terms (term, entry_id, field, weight) VALUES (?1, ?2, 1, 100)",
      params![term, entry_id],
    )
    .await?;
  }
  if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
    tx.execute(
      "INSERT OR IGNORE INTO terms (term, entry_id, field, weight) VALUES (?1, ?2, 2, 50)",
      params![extension.to_ascii_lowercase(), entry_id],
    )
    .await?;
  }
  for component in path
    .components()
    .filter_map(|component| component.as_os_str().to_str())
  {
    for term in tokenize(component) {
      tx.execute(
        "INSERT OR IGNORE INTO terms (term, entry_id, field, weight) VALUES (?1, ?2, 3, 10)",
        params![term, entry_id],
      )
      .await?;
    }
  }
  Ok(())
}

fn entry_changed(existing: &ExistingEntry, entry: &kuntu_crawler::CrawledEntry) -> bool {
  existing.deleted
    || existing.kind != entry.kind
    || existing.size != entry_size_i64(entry)
    || existing.mtime != entry.mtime
    || existing.hidden != entry.decision.hidden
    || existing.ignored != entry.decision.ignored
    || existing.sensitive != entry.decision.sensitive
}

fn entry_size_i64(entry: &kuntu_crawler::CrawledEntry) -> Option<i64> {
  entry.size.and_then(|size| i64::try_from(size).ok())
}

async fn count_entries_tx(tx: &turso::transaction::Transaction<'_>, root_id: i64) -> Result<i64> {
  let mut rows = tx
    .query(
      "SELECT COUNT(*) FROM entries WHERE root_id = ?1 AND deleted = 0",
      params![root_id],
    )
    .await?;
  let row = rows.next().await?.expect("count returns a row");
  Ok(row.get::<i64>(0)?)
}

async fn mark_root_state_dirty_tx(
  tx: &turso::transaction::Transaction<'_>,
  root_id: i64,
) -> Result<()> {
  let count = count_entries_tx(tx, root_id).await?;
  tx.execute(
        "INSERT INTO root_state (root_id, generation, last_full_scan_at, last_incremental_at, dirty, entry_count)
         VALUES (?1, COALESCE((SELECT generation FROM root_state WHERE root_id = ?1), 0), NULL, NULL, 1, ?2)
         ON CONFLICT(root_id) DO UPDATE SET
           dirty = 1,
           entry_count = excluded.entry_count",
        params![root_id, count],
    )
    .await?;
  Ok(())
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

async fn root_ids_for_config(conn: &turso::Connection, config: &SearchConfig) -> Result<Vec<i64>> {
  let mut ids = Vec::new();
  for root in config.roots.iter().filter(|root| root.enabled) {
    let mut rows = conn
      .query(
        "SELECT id FROM roots WHERE path = ?1",
        params![root.path.to_string_lossy()],
      )
      .await?;
    if let Some(row) = rows.next().await? {
      ids.push(row.get::<i64>(0)?);
    }
  }
  Ok(ids)
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

async fn table_exists(conn: &turso::Connection, table: &str) -> Result<bool> {
  let mut rows = conn
    .query(
      "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1",
      params![table],
    )
    .await?;
  Ok(rows.next().await?.is_some())
}

async fn any_table_exists(conn: &turso::Connection) -> Result<bool> {
  let mut rows = conn
    .query(
      "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
      (),
    )
    .await?;
  Ok(rows.next().await?.is_some())
}

fn remove_database_files(path: &Path) {
  let raw = path.to_string_lossy().to_string();
  for candidate in [raw.clone(), format!("{raw}-wal"), format!("{raw}-shm")] {
    let _ = std::fs::remove_file(candidate);
  }
}

fn index_error_to_backend(error: IndexError) -> BackendError {
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

const SCHEMA_DDL: &str = "
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
CREATE INDEX IF NOT EXISTS idx_entries_root_deleted_path_lower ON entries(root_id, deleted, path_lower);
";

#[cfg(test)]
mod tests {
  use std::fs;

  use kuntu_crawler::remove_dir_all_if_exists;

  use super::*;

  fn temp_dir(name: &str) -> PathBuf {
    let nonce = unix_now();
    std::env::temp_dir().join(format!("kuntu-index-{name}-{nonce}-{}", std::process::id()))
  }

  #[test]
  fn rebuild_indexes_allowed_files_and_searches_terms() {
    let root = temp_dir("rebuild");
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
    fs::write(root.join("docs/auth-providers.md"), "auth\n").unwrap();
    fs::write(root.join("node_modules/pkg/auth-providers.md"), "ignored\n").unwrap();

    let mut index = KuntuIndex::open_memory().unwrap();
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

    let mut index = KuntuIndex::open_memory().unwrap();
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

    let mut index = KuntuIndex::open_memory().unwrap();
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

    let mut index = KuntuIndex::open_memory().unwrap();
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

    let mut index = KuntuIndex::open_memory().unwrap();
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

    let mut index = KuntuIndex::open_memory().unwrap();
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

    let mut index = KuntuIndex::open_memory().unwrap();
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

    let mut index = KuntuIndex::open_memory().unwrap();
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
  fn compact_single_token_miss_returns_empty_complete_without_root_scan() {
    // Plan 0062 Task 1: the full-root fallback is gone. A compacted query
    // whose token matches no indexed term is a miss, not a scan; ordered
    // fuzzy matching is no longer promised by the index.
    let root = temp_dir("substring");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "lib\n").unwrap();

    let mut index = KuntuIndex::open_memory().unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    };
    index.rebuild(&config).unwrap();
    let outcome = index
      .search_with_metrics(&config, &SearchQuery::new("srclib"))
      .unwrap();
    remove_dir_all_if_exists(&root).unwrap();

    assert_eq!(outcome.candidate_count, 0);
    assert_eq!(outcome.results, Vec::new());
    assert!(!outcome.truncated);
    assert_eq!(outcome.coverage, CandidateCoverage::Complete);
  }

  #[test]
  fn compacted_ordered_queries_across_tokens_are_empty_complete() {
    // Same no-fallback contract for the ordered-fuzzy shape: "awspdf" is one
    // token that matches no indexed term, so the answer is empty and complete.
    let root = temp_dir("ordered-fuzzy");
    fs::create_dir_all(&root).unwrap();
    fs::write(
      root.join("AWS Certified Solutions Architect Associate SAA-C03.pdf"),
      "pdf\n",
    )
    .unwrap();

    let mut index = KuntuIndex::open_memory().unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    };
    index.rebuild(&config).unwrap();
    let outcome = index
      .search_with_metrics(&config, &SearchQuery::new("awspdf"))
      .unwrap();
    remove_dir_all_if_exists(&root).unwrap();

    assert_eq!(outcome.candidate_count, 0);
    assert_eq!(outcome.results, Vec::new());
    assert_eq!(outcome.coverage, CandidateCoverage::Complete);
  }

  /// Plant one entry (and its basename terms) directly, bypassing the
  /// crawler: the point is that stale or hostile rows already in the store
  /// cannot leak through search.
  fn plant_entry(
    index: &KuntuIndex,
    root: &Path,
    relative: &str,
    kind_int: i64,
    policy_flags: (bool, bool, bool),
    terms: &[&str],
  ) {
    let (hidden, ignored, sensitive) = policy_flags;
    let root = kuntu_core::normalize_root_path(root.to_path_buf());
    let full = root.join(relative);
    let name = full.file_name().unwrap().to_string_lossy().to_string();
    let name_lower = name.to_ascii_lowercase();
    let extension = name
      .rsplit_once('.')
      .map(|(_, extension)| extension.to_ascii_lowercase());
    block_on(async {
      let root_id: i64 = {
        let mut rows = index
          .conn
          .query(
            "SELECT id FROM roots WHERE path = ?1",
            params![root.to_string_lossy()],
          )
          .await
          .unwrap();
        rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap()
      };
      index
        .conn
        .execute(
          "INSERT INTO entries (root_id, path, path_lower, name, name_lower, extension, kind, size, mtime, hidden, ignored, sensitive, deleted, indexed_at)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 4, NULL, ?8, ?9, ?10, 0, 0)",
          params![
            root_id,
            full.to_string_lossy(),
            full.to_string_lossy().to_ascii_lowercase(),
            name,
            name_lower,
            extension,
            kind_int,
            hidden,
            ignored,
            sensitive,
          ],
        )
        .await
        .unwrap();
      let entry_id: i64 = {
        let mut rows = index
          .conn
          .query(
            "SELECT id FROM entries WHERE root_id = ?1 AND path = ?2",
            params![root_id, full.to_string_lossy()],
          )
          .await
          .unwrap();
        rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap()
      };
      for term in terms {
        index
          .conn
          .execute(
            "INSERT OR IGNORE INTO terms (term, entry_id, field, weight) VALUES (?1, ?2, 1, 100)",
            params![term, entry_id],
          )
          .await
          .unwrap();
      }
    });
  }

  #[test]
  fn search_default_query_excludes_hidden_ignored_and_sensitive_rows() {
    let root = temp_dir("policy-search");
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(root.join("docs/plan-report.md"), "doc\n").unwrap();

    let mut index = KuntuIndex::open_memory().unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    };
    index.rebuild(&config).unwrap();

    // Plant policy-forbidden rows that outrank the legitimate hit: same
    // basename (same score) on lexically earlier paths, so a post-window
    // filter would visibly return them before the allowed file.
    plant_entry(
      &index,
      &root,
      ".cached/plan-report.md",
      1,
      (true, false, false),
      &["plan", "report"],
    );
    plant_entry(
      &index,
      &root,
      "ignored_dir/plan-report.md",
      1,
      (false, true, false),
      &["plan", "report"],
    );
    plant_entry(
      &index,
      &root,
      ".ssh/plan-report.md",
      1,
      (false, false, true),
      &["plan", "report"],
    );

    let outcome = index
      .search_with_metrics(&config, &SearchQuery::new("plan-report").with_limit(1))
      .unwrap();
    remove_dir_all_if_exists(&root).unwrap();

    assert_eq!(outcome.candidate_count, 1);
    assert_eq!(outcome.results.len(), 1);
    assert!(outcome.results[0].path.ends_with("docs/plan-report.md"));
  }

  #[test]
  fn default_query_still_sees_hidden_rows_of_a_hidden_allowing_root() {
    let root = temp_dir("root-allowance");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("open-plan.md"), "a\n").unwrap();
    fs::write(root.join(".hidden-plan.md"), "b\n").unwrap();

    let mut index = KuntuIndex::open_memory().unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root).include_hidden(true)],
    };
    index.rebuild(&config).unwrap();
    let outcome = index
      .search_with_metrics(&config, &SearchQuery::new("plan").with_limit(10))
      .unwrap();
    remove_dir_all_if_exists(&root).unwrap();

    assert_eq!(
      outcome.results.len(),
      2,
      "the root's allowance must survive the query flag"
    );
  }

  #[test]
  fn dotted_and_bare_extension_filters_agree() {
    let root = temp_dir("dotted-ext");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("plan-report.md"), "a\n").unwrap();

    let mut index = KuntuIndex::open_memory().unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    };
    index.rebuild(&config).unwrap();

    let mut dotted = SearchQuery::new("plan-report");
    dotted.extensions = vec![".md".to_string()];
    let dotted_outcome = index.search_with_metrics(&config, &dotted).unwrap();

    let mut bare = SearchQuery::new("plan-report");
    bare.extensions = vec!["md".to_string()];
    let bare_outcome = index.search_with_metrics(&config, &bare).unwrap();
    remove_dir_all_if_exists(&root).unwrap();

    assert_eq!(
      dotted_outcome.results.len(),
      1,
      "leading dot must be normalized"
    );
    assert_eq!(dotted_outcome.results, bare_outcome.results);
  }

  #[test]
  fn search_scope_confines_candidates_to_configured_roots() {
    let root_a = temp_dir("scope-a");
    let root_b = temp_dir("scope-b");
    fs::create_dir_all(root_a.join("docs")).unwrap();
    fs::create_dir_all(&root_b).unwrap();
    let root_a = kuntu_core::normalize_root_path(root_a);
    let root_b = kuntu_core::normalize_root_path(root_b);
    fs::write(root_a.join("docs/plan-report.md"), "a\n").unwrap();
    // Root B holds the stronger match: exact basename "plan".
    fs::write(root_b.join("plan.md"), "b\n").unwrap();

    let mut index = KuntuIndex::open_memory().unwrap();
    let index_config = SearchConfig {
      roots: vec![SearchRoot::new(&root_a), SearchRoot::new(&root_b)],
    };
    index.rebuild(&index_config).unwrap();
    let query_config = SearchConfig {
      roots: vec![SearchRoot::new(&root_a)],
    };
    let outcome = index
      .search_with_metrics(&query_config, &SearchQuery::new("plan").with_limit(10))
      .unwrap();
    remove_dir_all_if_exists(&root_a).unwrap();
    remove_dir_all_if_exists(&root_b).unwrap();

    assert_eq!(outcome.candidate_count, 1);
    assert_eq!(outcome.results.len(), 1);
    assert!(outcome.results[0].path.starts_with(&root_a));
  }

  #[test]
  fn files_only_query_returns_file_behind_directory_window() {
    let root = temp_dir("kind-window");
    let mut index = KuntuIndex::open_memory().unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    };
    index.rebuild(&config).unwrap();

    // 101 directories matching at the same rank as the file, planted so the
    // file sorts behind all of them under the old post-window behaviour.
    for index_number in 0..101 {
      plant_entry(
        &index,
        &root,
        &format!("plan-dir-{index_number:03}"),
        2,
        (false, false, false),
        &["plan"],
      );
    }
    plant_entry(
      &index,
      &root,
      "plan-file.md",
      1,
      (false, false, false),
      &["plan"],
    );

    let files_only = SearchQuery {
      include_directories: false,
      ..SearchQuery::new("plan").with_limit(10)
    };
    let outcome = index.search_with_metrics(&config, &files_only).unwrap();
    assert_eq!(outcome.results.len(), 1);
    assert!(outcome.results[0].path.ends_with("plan-file.md"));

    let unfiltered = SearchQuery::new("plan").with_limit(5);
    let outcome = index.search_with_metrics(&config, &unfiltered).unwrap();
    assert_eq!(outcome.results.len(), 5);
    remove_dir_all_if_exists(&root).unwrap();
  }

  #[test]
  fn no_term_hit_returns_empty_complete_without_scanning_the_root() {
    let root = temp_dir("no-term");
    fs::create_dir_all(&root).unwrap();
    for index_number in 0..50 {
      fs::write(root.join(format!("document-{index_number:02}.txt")), "x\n").unwrap();
    }

    let mut index = KuntuIndex::open_memory().unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    };
    index.rebuild(&config).unwrap();
    let outcome = index
      .search_with_metrics(&config, &SearchQuery::new("zzqqx"))
      .unwrap();
    remove_dir_all_if_exists(&root).unwrap();

    assert_eq!(outcome.candidate_count, 0);
    assert_eq!(outcome.results, Vec::new());
    assert!(!outcome.truncated);
    assert_eq!(outcome.coverage, CandidateCoverage::Complete);
  }

  fn run_bounded_fixture(name: &str, rows: i64, miss_ceiling_secs: u64, broad_ceiling_secs: u64) {
    let root = temp_dir(name);
    fs::create_dir_all(&root).unwrap();
    let root = kuntu_core::normalize_root_path(root);
    // File-backed on purpose: an in-memory turso store costs kilobytes per
    // row, and a fixture at this scale is exactly where that shows. The
    // database lives outside the crawled root so the rebuild does not index
    // the fixture's own store.
    let db_dir = temp_dir(&format!("{name}-db"));
    fs::create_dir_all(&db_dir).unwrap();
    let db_path = db_dir.join("fixture.sqlite");
    let mut index = KuntuIndex::open(&db_path).unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    };
    index.rebuild(&config).unwrap();

    let total_rows: i64 = rows;
    // Values are literals generated by this test, so large multi-row
    // statements keep the fixture to a few hundred round-trips.
    let batch_rows: i64 = 200;
    block_on(async {
      let tx = index.conn.transaction().await.unwrap();
      let root_id: i64 = {
        let mut rows = tx
          .query(
            "SELECT id FROM roots WHERE path = ?1",
            params![root.to_string_lossy()],
          )
          .await
          .unwrap();
        rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap()
      };
      // The rebuild indexed the (empty) root directory itself; the fixture
      // owns the whole entries table.
      tx.execute("DELETE FROM entries", ()).await.unwrap();
      let mut batch_start: i64 = 0;
      while batch_start < total_rows {
        let batch_end = (batch_start + batch_rows).min(total_rows);
        let mut entries_sql = String::from(
          "INSERT INTO entries (id, root_id, path, path_lower, name, name_lower, extension, kind, size, mtime, hidden, ignored, sensitive, deleted, indexed_at) VALUES ",
        );
        let mut terms_sql =
          String::from("INSERT OR IGNORE INTO terms (term, entry_id, field, weight) VALUES ");
        for ordinal in batch_start..batch_end {
          // Row ids start at 1: turso treats an explicit 0 as "assign one".
          let entry_id = ordinal + 1;
          if ordinal > batch_start {
            entries_sql.push(',');
            terms_sql.push(',');
          }
          let path = format!("dir{}/batchfile-{ordinal}.dat", ordinal % 997);
          let path_lower = path.to_ascii_lowercase();
          let name = format!("batchfile-{ordinal}.dat");
          entries_sql.push_str(&format!(
            "({entry_id}, {root_id}, '{path}', '{path_lower}', '{name}', '{name}', 'dat', 1, 8, NULL, 0, 0, 0, 0, 0)"
          ));
          terms_sql.push_str(&format!("('batchfile', {entry_id}, 1, 100)"));
        }
        tx.execute(&entries_sql, ()).await.unwrap();
        tx.execute(&terms_sql, ()).await.unwrap();
        batch_start = batch_end;
      }
      tx.commit().await.unwrap();
    });

    let miss_started = std::time::Instant::now();
    let miss = index
      .search_with_metrics(&config, &SearchQuery::new("zzqqx"))
      .unwrap();
    let miss_elapsed = miss_started.elapsed();
    assert_eq!(miss.candidate_count, 0);
    assert_eq!(miss.results, Vec::new());
    assert_eq!(miss.coverage, CandidateCoverage::Complete);

    let broad_started = std::time::Instant::now();
    let broad = index
      .search_with_metrics(&config, &SearchQuery::new("batchfile").with_limit(100))
      .unwrap();
    let broad_elapsed = broad_started.elapsed();

    assert_eq!(broad.candidate_count, MAX_KFS_CANDIDATE_ROWS);
    assert_eq!(broad.coverage, CandidateCoverage::Truncated);
    assert!(broad.truncated);
    assert_eq!(broad.results.len(), 100);
    // Observed ceilings are recorded in the release notes; these guards only
    // catch a regression back to the unbounded full-root fallback.
    assert!(
      miss_elapsed.as_secs() < miss_ceiling_secs,
      "empty miss must not scan the root; took {miss_elapsed:?}"
    );
    assert!(
      broad_elapsed.as_secs() < broad_ceiling_secs,
      "bounded broad query took {broad_elapsed:?}"
    );
    remove_database_files(&db_path);
    let _ = remove_dir_all_if_exists(&db_dir);
    let _ = remove_dir_all_if_exists(&root);
  }

  /// Quick-scale guard: still larger than the 50,000-candidate work bound so
  /// the broad query must report truncation. Ignored because a debug-profile
  /// turso costs ~1.5 ms per joined candidate row; run via `just benchmark`.
  #[test]
  #[ignore = "bounded-fixture benchmark; run in release mode via just benchmark"]
  fn broad_prefix_and_miss_stay_bounded_at_60k_rows() {
    run_bounded_fixture("bounded-60k", 60_000, 5, 60);
  }

  /// The plan's one-million-row benchmark. Debug-profile turso inserts grow
  /// superlinearly past ~200k rows, so this runs ignored; measure with
  /// `cargo test --release -p kuntu-index one_million -- --ignored --nocapture`.
  ///
  /// KNOWN ENGINE GAP, measured 2026-09-14 on turso 0.7.2 (release, M-series
  /// Mac): the planner does not seek the terms primary key for a range
  /// predicate — it full-scans the terms table, so a miss costs ~7.5 s at 1M
  /// rows instead of the O(1) a seek would give. The scan predates this
  /// change (v0.2.x ran the same range shape), and the no-fallback contract
  /// this test guards (empty result, candidate_count 0) holds regardless.
  #[test]
  #[ignore = "one-million-row benchmark; run in release mode"]
  fn one_million_row_root_stays_bounded_on_miss_and_broad_prefix() {
    // The broad ceiling absorbs the deterministic-window ORDER BY over a
    // pathological single-token store (1M entries sharing one term); real
    // stores spread tokens, and the 60k guard's 60 s covers that shape.
    run_bounded_fixture("million", 1_000_000, 30, 300);
  }

  #[test]
  fn equal_scores_break_ties_by_path_deterministically() {
    let root_a = temp_dir("tie-a");
    let root_b = temp_dir("tie-b");
    fs::create_dir_all(&root_a).unwrap();
    fs::create_dir_all(&root_b).unwrap();
    fs::write(root_a.join("same-name.md"), "a\n").unwrap();
    fs::write(root_b.join("same-name.md"), "b\n").unwrap();

    let mut index = KuntuIndex::open_memory().unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root_a), SearchRoot::new(&root_b)],
    };
    index.rebuild(&config).unwrap();

    let first = index
      .search_with_metrics(&config, &SearchQuery::new("same-name").with_limit(10))
      .unwrap();
    let second = index
      .search_with_metrics(&config, &SearchQuery::new("same-name").with_limit(10))
      .unwrap();
    remove_dir_all_if_exists(&root_a).unwrap();
    remove_dir_all_if_exists(&root_b).unwrap();

    let first_paths: Vec<_> = first.results.iter().map(|r| r.path.clone()).collect();
    let second_paths: Vec<_> = second.results.iter().map(|r| r.path.clone()).collect();
    assert_eq!(first_paths, second_paths);
    let mut sorted = first_paths.clone();
    sorted.sort();
    assert_eq!(first_paths, sorted, "equal scores tie-break by path");
  }

  #[test]
  fn non_ascii_names_rank_deterministically() {
    let root = temp_dir("non-ascii");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("计划书A.md"), "a\n").unwrap();
    fs::write(root.join("计划书B.md"), "b\n").unwrap();

    let mut index = KuntuIndex::open_memory().unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    };
    index.rebuild(&config).unwrap();

    let first = index
      .search_with_metrics(&config, &SearchQuery::new("计划").with_limit(10))
      .unwrap();
    let second = index
      .search_with_metrics(&config, &SearchQuery::new("计划").with_limit(10))
      .unwrap();
    remove_dir_all_if_exists(&root).unwrap();

    assert_eq!(first.results.len(), 2);
    let first_paths: Vec<_> = first.results.iter().map(|r| r.path.clone()).collect();
    let second_paths: Vec<_> = second.results.iter().map(|r| r.path.clone()).collect();
    assert_eq!(first_paths, second_paths);
  }

  #[test]
  fn ascii_prefix_upper_bound_advances_last_ascii_byte() {
    assert_eq!(ascii_prefix_upper_bound("abc"), Some("abd".to_string()));
    assert_eq!(ascii_prefix_upper_bound("abz"), Some("ab{".to_string()));
    assert_eq!(ascii_prefix_upper_bound("é"), None);
  }

  #[test]
  fn open_memory_stamps_current_schema_version() {
    let index = KuntuIndex::open_memory().unwrap();
    assert_eq!(block_on(index.schema_version()).unwrap(), SCHEMA_VERSION);
  }

  #[test]
  fn v1_database_stamps_forward_in_place() {
    let path = temp_dir("v1-forward");
    // Create a database, then pretend it came from the rusqlite era (v1).
    {
      let index = KuntuIndex::open(&path).unwrap();
      block_on(index.set_schema_version(1)).unwrap();
    }
    let index = KuntuIndex::open(&path).unwrap();
    assert_eq!(block_on(index.schema_version()).unwrap(), SCHEMA_VERSION);
    remove_database_files(&path);
  }

  #[test]
  fn legacy_v0_database_is_recreated_on_open() {
    let path = temp_dir("v0-recreate");
    // Simulate a pre-versioning rusqlite-era database: schema exists, a row is
    // indexed, but the version stamp is 0.
    {
      let index = KuntuIndex::open(&path).unwrap();
      block_on(async {
        index
          .conn
          .execute(
            "INSERT INTO roots (id, path, enabled, updated_at) VALUES (1, '/tmp/legacy', 1, 0)",
            (),
          )
          .await
          .unwrap();
        index.conn
          .execute(
            "INSERT INTO entries (root_id, path, path_lower, name, name_lower, kind, indexed_at)
             VALUES (1, '/tmp/legacy/Report.PDF', '/tmp/legacy/report.pdf', 'Report.PDF', 'report.pdf', 1, 0)",
            (),
          )
          .await
          .unwrap();
      });
      block_on(index.set_schema_version(0)).unwrap();
    }
    let index = KuntuIndex::open(&path).unwrap();
    assert_eq!(block_on(index.schema_version()).unwrap(), SCHEMA_VERSION);
    let status = index.status().unwrap();
    assert!(status.is_empty());
    remove_database_files(&path);
  }

  #[test]
  fn open_refuses_newer_schema_without_touching_the_database() {
    let path = temp_dir("refuse-newer");
    {
      let index = KuntuIndex::open(&path).unwrap();
      block_on(index.set_schema_version(SCHEMA_VERSION + 1)).unwrap();
    }
    let error = KuntuIndex::open(&path).unwrap_err();
    match error {
      IndexError::SchemaTooNew { found, supported } => {
        assert_eq!(found, SCHEMA_VERSION + 1);
        assert_eq!(supported, SCHEMA_VERSION);
      }
      other => panic!("expected schema version failure, got {other}"),
    }
    // The file is untouched: a follow-up open refuses the same way.
    let second = KuntuIndex::open(&path).unwrap_err();
    assert!(matches!(second, IndexError::SchemaTooNew { .. }));
    remove_database_files(&path);
  }

  #[test]
  fn deleting_entries_cascades_to_terms() {
    // The rebuild's bulk `DELETE FROM entries` relies on the terms foreign
    // key; without the pragma on, row id reuse aliases stale terms onto new
    // files and searches return unrelated results.
    let root = temp_dir("cascade");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("cascade-target.md"), "cascade\n").unwrap();

    let mut index = KuntuIndex::open_memory().unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    };
    index.rebuild(&config).unwrap();

    let terms_before: i64 = block_on(async {
      let mut rows = index
        .conn
        .query("SELECT COUNT(*) FROM terms", ())
        .await
        .unwrap();
      rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap()
    });
    assert!(terms_before > 0);

    block_on(async {
      index.conn.execute("DELETE FROM entries", ()).await.unwrap();
    });

    let terms_after: i64 = block_on(async {
      let mut rows = index
        .conn
        .query("SELECT COUNT(*) FROM terms", ())
        .await
        .unwrap();
      rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap()
    });
    assert_eq!(terms_after, 0);
    remove_dir_all_if_exists(&root).unwrap();
  }

  #[test]
  fn foreign_v0_database_is_refused_and_left_untouched() {
    let path = temp_dir("foreign-db");
    {
      let index = KuntuIndex::open(&path).unwrap();
      block_on(async {
        index
          .conn
          .execute(
            "CREATE TABLE user_data (id INTEGER PRIMARY KEY, note TEXT)",
            (),
          )
          .await
          .unwrap();
        index
          .conn
          .execute("INSERT INTO user_data VALUES (1, 'precious')", ())
          .await
          .unwrap();
        index.conn.execute("DROP TABLE roots", ()).await.unwrap();
        index.conn.execute("DROP TABLE entries", ()).await.unwrap();
        index.conn.execute("DROP TABLE terms", ()).await.unwrap();
        index
          .conn
          .execute("DROP TABLE root_state", ())
          .await
          .unwrap();
      });
      block_on(index.set_schema_version(0)).unwrap();
    }
    let error = KuntuIndex::open(&path).unwrap_err();
    match error {
      IndexError::UnrecognizedDatabase { path: reported } => {
        assert_eq!(reported, path);
      }
      other => panic!("expected unrecognized-database refusal, got {other}"),
    }
    // The refusal is non-destructive: the stranger's rows survive.
    let readable: i64 = block_on(async {
      let database = turso::Builder::new_local(path.to_str().unwrap())
        .build()
        .await
        .unwrap();
      let conn = database.connect().unwrap();
      let mut rows = conn
        .query("SELECT COUNT(*) FROM user_data", ())
        .await
        .unwrap();
      rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap()
    });
    assert_eq!(readable, 1);
    remove_database_files(&path);
  }

  #[cfg(unix)]
  #[test]
  fn open_refuses_non_utf8_database_paths_instead_of_aliasing() {
    use std::os::unix::ffi::OsStrExt;

    let path = std::env::temp_dir()
      .join(format!("kuntu-index-nonutf8-{}", std::process::id()))
      .with_extension(std::ffi::OsStr::from_bytes(b"not-\xff-utf8"));
    let error = KuntuIndex::open(&path).unwrap_err();
    assert!(matches!(error, IndexError::Io(_)));
    assert!(
      !path.exists(),
      "a database must not be created at a lossy alias"
    );
  }
}
