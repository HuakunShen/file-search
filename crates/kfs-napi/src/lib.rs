//! Local Node-API binding for the Kunkun file search Rust workspace.
//!
//! The binding intentionally exposes plain JavaScript DTOs and keeps the Rust
//! search/index crates as the source of truth. Each operation opens the SQLite
//! index inside a libuv task so Node/Electron callers do not hold a shared Rust
//! database connection in JavaScript state.

use std::fmt;

use kfs_core::{
    IndexRebuildStats, IndexRefreshStats, IndexRepairStats, IndexRootStatus, MatchKind,
    SearchConfig, SearchQuery, SearchResult, SearchRoot,
};
use kfs_index_sqlite::{IndexedSearchOutcome, SqliteIndex};
use napi::bindgen_prelude::*;
use napi_derive::napi;

#[napi(object)]
#[derive(Clone, Debug)]
pub struct SearchRootInput {
    pub path: String,
    pub enabled: Option<bool>,
    pub priority: Option<i32>,
    pub include_hidden: Option<bool>,
    pub include_ignored: Option<bool>,
}

#[napi(object)]
#[derive(Clone, Debug)]
pub struct SearchRequest {
    pub roots: Vec<SearchRootInput>,
    pub query: String,
    pub limit: Option<u32>,
    pub include_hidden: Option<bool>,
    pub include_ignored: Option<bool>,
    pub extensions: Option<Vec<String>>,
}

#[napi(object)]
pub struct IndexRebuildStatsDto {
    pub roots: u32,
    pub entries: u32,
    pub skipped: u32,
    pub errors: Vec<String>,
}

#[napi(object)]
pub struct IndexRefreshStatsDto {
    pub roots: u32,
    pub inserted: u32,
    pub updated: u32,
    pub deleted: u32,
    pub unchanged: u32,
    pub skipped: u32,
    pub errors: Vec<String>,
}

#[napi(object)]
pub struct IndexRepairStatsDto {
    pub roots: u32,
    pub dirty_roots: u32,
    pub repaired_roots: u32,
    pub errors: Vec<String>,
}

#[napi(object)]
pub struct IndexRootStatusDto {
    pub path: String,
    pub entry_count: u32,
    pub generation: f64,
    pub dirty: bool,
    pub last_full_scan_at: Option<f64>,
    pub last_incremental_at: Option<f64>,
}

#[napi(object)]
pub struct SearchResultDto {
    pub path: String,
    pub score: i32,
    pub provider: String,
    pub matches: Vec<String>,
}

#[napi(object)]
pub struct IndexedSearchOutcomeDto {
    pub candidate_count: u32,
    pub results: Vec<SearchResultDto>,
}

#[napi]
pub struct FileSearchIndex {
    db_path: String,
}

#[napi]
impl FileSearchIndex {
    #[napi(constructor)]
    pub fn new(db_path: String) -> Self {
        Self { db_path }
    }

    #[napi(ts_return_type = "Promise<IndexRebuildStatsDto>")]
    pub fn rebuild(&self, roots: Vec<SearchRootInput>) -> AsyncTask<RebuildTask> {
        AsyncTask::new(RebuildTask {
            db_path: self.db_path.clone(),
            roots,
        })
    }

    #[napi(ts_return_type = "Promise<IndexRefreshStatsDto>")]
    pub fn refresh(&self, roots: Vec<SearchRootInput>) -> AsyncTask<RefreshTask> {
        AsyncTask::new(RefreshTask {
            db_path: self.db_path.clone(),
            roots,
        })
    }

    #[napi(ts_return_type = "Promise<IndexRepairStatsDto>")]
    pub fn repair(&self, roots: Vec<SearchRootInput>) -> AsyncTask<RepairTask> {
        AsyncTask::new(RepairTask {
            db_path: self.db_path.clone(),
            roots,
        })
    }

    #[napi(ts_return_type = "Promise<Array<IndexRootStatusDto>>")]
    pub fn status(&self) -> AsyncTask<StatusTask> {
        AsyncTask::new(StatusTask {
            db_path: self.db_path.clone(),
        })
    }

    #[napi(ts_return_type = "Promise<IndexedSearchOutcomeDto>")]
    pub fn search(&self, request: SearchRequest) -> AsyncTask<SearchTask> {
        AsyncTask::new(SearchTask {
            db_path: self.db_path.clone(),
            request,
        })
    }
}

pub struct RebuildTask {
    db_path: String,
    roots: Vec<SearchRootInput>,
}

impl Task for RebuildTask {
    type Output = IndexRebuildStatsDto;
    type JsValue = IndexRebuildStatsDto;

    fn compute(&mut self) -> Result<Self::Output> {
        let config = config_from_roots(&self.roots);
        let mut index = open_index(&self.db_path)?;
        let stats = index.rebuild(&config).map_err(napi_error)?;
        Ok(stats.into())
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

pub struct RefreshTask {
    db_path: String,
    roots: Vec<SearchRootInput>,
}

impl Task for RefreshTask {
    type Output = IndexRefreshStatsDto;
    type JsValue = IndexRefreshStatsDto;

    fn compute(&mut self) -> Result<Self::Output> {
        let config = config_from_roots(&self.roots);
        let mut index = open_index(&self.db_path)?;
        let stats = index.refresh(&config).map_err(napi_error)?;
        Ok(stats.into())
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

pub struct RepairTask {
    db_path: String,
    roots: Vec<SearchRootInput>,
}

impl Task for RepairTask {
    type Output = IndexRepairStatsDto;
    type JsValue = IndexRepairStatsDto;

    fn compute(&mut self) -> Result<Self::Output> {
        let config = config_from_roots(&self.roots);
        let mut index = open_index(&self.db_path)?;
        let stats = index.repair(&config).map_err(napi_error)?;
        Ok(stats.into())
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

pub struct StatusTask {
    db_path: String,
}

impl Task for StatusTask {
    type Output = Vec<IndexRootStatusDto>;
    type JsValue = Vec<IndexRootStatusDto>;

    fn compute(&mut self) -> Result<Self::Output> {
        let index = open_index(&self.db_path)?;
        let status = index.status().map_err(napi_error)?;
        Ok(status.into_iter().map(Into::into).collect())
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

pub struct SearchTask {
    db_path: String,
    request: SearchRequest,
}

impl Task for SearchTask {
    type Output = IndexedSearchOutcomeDto;
    type JsValue = IndexedSearchOutcomeDto;

    fn compute(&mut self) -> Result<Self::Output> {
        let index = open_index(&self.db_path)?;
        let config = config_from_roots(&self.request.roots);
        let query = query_from_request(&self.request);
        let outcome = index
            .search_with_metrics(&config, &query)
            .map_err(napi_error)?;
        Ok(outcome.into())
    }

    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        Ok(output)
    }
}

fn open_index(db_path: &str) -> Result<SqliteIndex> {
    SqliteIndex::open(db_path).map_err(napi_error)
}

fn config_from_roots(roots: &[SearchRootInput]) -> SearchConfig {
    SearchConfig {
        roots: roots
            .iter()
            .map(|input| {
                let mut root = SearchRoot::new(&input.path);
                if let Some(enabled) = input.enabled {
                    root.enabled = enabled;
                }
                if let Some(priority) = input.priority {
                    root.priority = priority;
                }
                if let Some(include_hidden) = input.include_hidden {
                    root.include_hidden = include_hidden;
                }
                if let Some(include_ignored) = input.include_ignored {
                    root.include_ignored = include_ignored;
                }
                root
            })
            .collect(),
    }
}

fn query_from_request(request: &SearchRequest) -> SearchQuery {
    let mut query = SearchQuery::new(&request.query);
    if let Some(limit) = request.limit {
        query.limit = limit as usize;
    }
    if let Some(include_hidden) = request.include_hidden {
        query.include_hidden = include_hidden;
    }
    if let Some(include_ignored) = request.include_ignored {
        query.include_ignored = include_ignored;
    }
    if let Some(extensions) = &request.extensions {
        query.extensions = extensions.clone();
    }
    query
}

fn napi_error(error: impl fmt::Display) -> Error {
    Error::new(Status::GenericFailure, error.to_string())
}

fn count_to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn option_i64_to_f64(value: Option<i64>) -> Option<f64> {
    value.map(|value| value as f64)
}

fn match_kind_name(kind: MatchKind) -> String {
    format!("{kind:?}")
}

impl From<IndexRebuildStats> for IndexRebuildStatsDto {
    fn from(stats: IndexRebuildStats) -> Self {
        Self {
            roots: count_to_u32(stats.roots),
            entries: count_to_u32(stats.entries),
            skipped: count_to_u32(stats.skipped),
            errors: stats.errors,
        }
    }
}

impl From<IndexRefreshStats> for IndexRefreshStatsDto {
    fn from(stats: IndexRefreshStats) -> Self {
        Self {
            roots: count_to_u32(stats.roots),
            inserted: count_to_u32(stats.inserted),
            updated: count_to_u32(stats.updated),
            deleted: count_to_u32(stats.deleted),
            unchanged: count_to_u32(stats.unchanged),
            skipped: count_to_u32(stats.skipped),
            errors: stats.errors,
        }
    }
}

impl From<IndexRepairStats> for IndexRepairStatsDto {
    fn from(stats: IndexRepairStats) -> Self {
        Self {
            roots: count_to_u32(stats.roots),
            dirty_roots: count_to_u32(stats.dirty_roots),
            repaired_roots: count_to_u32(stats.repaired_roots),
            errors: stats.errors,
        }
    }
}

impl From<IndexRootStatus> for IndexRootStatusDto {
    fn from(status: IndexRootStatus) -> Self {
        Self {
            path: status.path.to_string_lossy().into_owned(),
            entry_count: count_to_u32(status.entry_count),
            generation: status.generation as f64,
            dirty: status.dirty,
            last_full_scan_at: option_i64_to_f64(status.last_full_scan_at),
            last_incremental_at: option_i64_to_f64(status.last_incremental_at),
        }
    }
}

impl From<SearchResult> for SearchResultDto {
    fn from(result: SearchResult) -> Self {
        Self {
            path: result.path.to_string_lossy().into_owned(),
            score: result.score,
            provider: result.provider,
            matches: result.matches.into_iter().map(match_kind_name).collect(),
        }
    }
}

impl From<IndexedSearchOutcome> for IndexedSearchOutcomeDto {
    fn from(outcome: IndexedSearchOutcome) -> Self {
        Self {
            candidate_count: count_to_u32(outcome.candidate_count),
            results: outcome.results.into_iter().map(Into::into).collect(),
        }
    }
}
