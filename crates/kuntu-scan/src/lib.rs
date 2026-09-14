//! Moved from HuakunShen/space-lens `packages/space-lens` — working tree state of
//! 544e82aa6a093d6ff2746e3d96bf5f858819df96 (2026-09-11) plus that tree's uncommitted
//! eviction-progress work (EvictionProgressState, pause/cancel, bounded-concurrency
//! tuning and its tests). Diff against the source tree to audit; the code is otherwise
//! byte-identical. Consumers alias this package back as `space-lens` to keep their
//! `space_lens::` imports unchanged.
#![deny(clippy::all)]

pub mod clean;
pub mod cloud;
pub mod scanner;
pub mod snapshot;

pub use snapshot::{
  CapabilityState, NodeFlag, NodeKind, NodeScanState, PlatformCapabilities, SnapshotEnvelope,
  SnapshotNode,
};

pub use clean::{
  build_removal_plan, delete_path, execute_removal_plan, find_candidates, CandidateOptions,
  CleanupCandidate, CleanupPreset, RemovalEntry, RemovalOutcome, RemovalPlan,
};
pub use scanner::{measure_path, scan_directory, IgnoredMode, ScanNode, ScanOptions};
