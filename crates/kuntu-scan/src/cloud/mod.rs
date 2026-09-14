mod engine;
mod guard;
mod model;
mod platform;
mod policy;

pub use engine::{
  build_eviction_plan, execute_eviction_plan, execute_eviction_plan_with_concurrency,
  execute_eviction_plan_with_state, inspect_item, request_download_one, EvictionCandidate,
  EvictionOutcome, EvictionPlan, EvictionProgress, EvictionProgressState, EvictionResult,
  EvictionStatus, ItemSummary, ScanOptions, SkippedItem, DEFAULT_EVICTION_CONCURRENCY,
  MAX_EVICTION_CONCURRENCY,
};
pub use model::{
  Bytes, CloudError, DownloadState, ErrorKind, Fingerprint, ItemInfo, ItemKind, NativeError,
};
pub use platform::{CloudBackend, NativeICloudBackend};
pub use policy::{eviction_skip_reason, SkipReason};

#[cfg(test)]
mod tests;
