//! Platform-agnostic backend traits for file search implementations.
//!
//! These traits keep the public search API independent from macOS Spotlight,
//! SQLite, or any future Linux/Windows index providers. Platform-specific
//! crates implement the traits while callers depend on `kfs-core` shapes.

use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use crate::model::{SearchCandidate, SearchConfig, SearchQuery, SearchResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendError {
    pub message: String,
}

impl BackendError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for BackendError {}

impl From<std::io::Error> for BackendError {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRebuildStats {
    pub roots: usize,
    pub entries: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRefreshStats {
    pub roots: usize,
    pub inserted: usize,
    pub updated: usize,
    pub deleted: usize,
    pub unchanged: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRepairStats {
    pub roots: usize,
    pub dirty_roots: usize,
    pub repaired_roots: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRootStatus {
    pub path: PathBuf,
    pub entry_count: usize,
    pub generation: i64,
    pub dirty: bool,
    pub last_full_scan_at: Option<i64>,
    pub last_incremental_at: Option<i64>,
}

pub trait CandidateProvider {
    fn provider_name(&self) -> &'static str;

    fn search_candidates(
        &self,
        config: &SearchConfig,
        query: &SearchQuery,
    ) -> Result<Vec<SearchCandidate>, BackendError>;
}

pub trait MetadataIndex {
    fn rebuild_index(&mut self, config: &SearchConfig) -> Result<IndexRebuildStats, BackendError>;

    fn refresh_index(&mut self, config: &SearchConfig) -> Result<IndexRefreshStats, BackendError>;

    fn repair_index(&mut self, config: &SearchConfig) -> Result<IndexRepairStats, BackendError>;

    fn search_index(
        &self,
        config: &SearchConfig,
        query: &SearchQuery,
    ) -> Result<Vec<SearchResult>, BackendError>;

    fn status(&self) -> Result<Vec<IndexRootStatus>, BackendError>;
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::{EntryKind, SearchRoot};

    use super::*;

    #[derive(Debug)]
    struct MockProvider;

    impl CandidateProvider for MockProvider {
        fn provider_name(&self) -> &'static str {
            "mock"
        }

        fn search_candidates(
            &self,
            _config: &SearchConfig,
            _query: &SearchQuery,
        ) -> Result<Vec<SearchCandidate>, BackendError> {
            Ok(vec![SearchCandidate {
                path: PathBuf::from("/Users/alice/Dev/readme.md"),
                kind: EntryKind::File,
                provider: self.provider_name().to_string(),
            }])
        }
    }

    #[test]
    fn candidate_provider_trait_keeps_search_api_provider_neutral() {
        let provider: &dyn CandidateProvider = &MockProvider;
        let config = SearchConfig {
            roots: vec![SearchRoot::new("/Users/alice/Dev")],
        };

        let candidates = provider
            .search_candidates(&config, &SearchQuery::new("readme"))
            .unwrap();

        assert_eq!(provider.provider_name(), "mock");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].provider, "mock");
    }
}
