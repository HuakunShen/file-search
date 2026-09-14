//! Provider-independent file search core for Kunkun.
//!
//! This crate owns the domain model, path policy, ranking, and explanation
//! behavior so platform providers and future desktop adapters stay thin.

pub mod backend;
pub mod engine;
pub mod model;
pub mod policy;
pub mod ranking;

pub use backend::{
  BackendError, CandidateProvider, IndexRebuildStats, IndexRefreshStats, IndexRepairStats,
  IndexRootStatus, MetadataIndex,
};
pub use engine::SearchEngineCore;
pub use model::{
  normalize_root_path, EntryKind, ExplainResult, MatchKind, PolicyDecision, SearchCandidate,
  SearchConfig, SearchQuery, SearchResult, SearchRoot,
};
pub use policy::PathPolicy;
