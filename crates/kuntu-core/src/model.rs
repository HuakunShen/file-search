//! Shared data model for provider-independent file search.
//!
//! These types are intentionally plain Rust structs so CLI, daemon, desktop,
//! and future native bindings can all adapt them without pulling UI concerns
//! into the search core.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRoot {
  pub path: PathBuf,
  pub enabled: bool,
  pub priority: i32,
  pub include_hidden: bool,
  pub include_ignored: bool,
}

impl SearchRoot {
  pub fn new(path: impl Into<PathBuf>) -> Self {
    Self {
      path: normalize_root_path(path.into()),
      enabled: true,
      priority: 0,
      include_hidden: false,
      include_ignored: false,
    }
  }

  pub fn disabled(mut self) -> Self {
    self.enabled = false;
    self
  }

  pub fn with_priority(mut self, priority: i32) -> Self {
    self.priority = priority;
    self
  }

  pub fn include_hidden(mut self, include_hidden: bool) -> Self {
    self.include_hidden = include_hidden;
    self
  }

  pub fn include_ignored(mut self, include_ignored: bool) -> Self {
    self.include_ignored = include_ignored;
    self
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchConfig {
  pub roots: Vec<SearchRoot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
  pub query: String,
  pub limit: usize,
  pub include_hidden: bool,
  pub include_ignored: bool,
  /// Xross search scopes results to files or directories before ranking.
  /// Both default to true, which preserves the unfiltered behaviour; when
  /// exactly one is set, entries of the other kind are excluded at the SQL
  /// candidate stage and never crowd out the requested kind.
  pub include_files: bool,
  pub include_directories: bool,
  pub extensions: Vec<String>,
}

impl SearchQuery {
  pub fn new(query: impl Into<String>) -> Self {
    Self {
      query: query.into(),
      limit: 20,
      include_hidden: false,
      include_ignored: false,
      include_files: true,
      include_directories: true,
      extensions: Vec::new(),
    }
  }

  pub fn with_limit(mut self, limit: usize) -> Self {
    self.limit = limit;
    self
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
  File,
  Directory,
  Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchCandidate {
  pub path: PathBuf,
  pub kind: EntryKind,
  pub provider: String,
}

impl SearchCandidate {
  pub fn new(path: impl Into<PathBuf>) -> Self {
    Self {
      path: path.into(),
      kind: EntryKind::Other,
      provider: "test".to_string(),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchKind {
  Empty,
  ExactBasename,
  BasenamePrefix,
  BasenameToken,
  PathComponent,
  Substring,
  /// Query characters matched the filename in order after separators were removed.
  Fuzzy,
  Extension,
  RootPriority,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
  pub path: PathBuf,
  pub score: i32,
  pub provider: String,
  pub matches: Vec<MatchKind>,
  /// Kind as the provider observed it. The in-memory engine knows it from the
  /// walk; index rows carry their crawled kind.
  pub kind: EntryKind,
  /// Crawled size in bytes, when the provider had it.
  pub byte_size: Option<u64>,
  /// Crawled modification time, when the provider had it.
  pub modified_unix_seconds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDecision {
  pub path: PathBuf,
  pub allowed: bool,
  pub root: Option<PathBuf>,
  pub root_priority: i32,
  pub hidden: bool,
  pub ignored: bool,
  pub sensitive: bool,
  pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainResult {
  pub path: PathBuf,
  pub allowed: bool,
  pub root: Option<PathBuf>,
  pub hidden: bool,
  pub ignored: bool,
  pub sensitive: bool,
  pub reasons: Vec<String>,
  pub score: Option<i32>,
  pub matches: Vec<MatchKind>,
}

pub fn expand_tilde(path: PathBuf) -> PathBuf {
  let Some(text) = path.to_str() else {
    return path;
  };
  if text == "~" {
    if let Some(home) = std::env::var_os("HOME") {
      return PathBuf::from(home);
    }
  }
  if let Some(rest) = text.strip_prefix("~/") {
    if let Some(home) = std::env::var_os("HOME") {
      return PathBuf::from(home).join(rest);
    }
  }
  path
}

pub fn normalize_root_path(path: PathBuf) -> PathBuf {
  let expanded = expand_tilde(path);
  let absolute = if expanded.is_absolute() {
    expanded
  } else if let Ok(current_dir) = std::env::current_dir() {
    current_dir.join(expanded)
  } else {
    expanded
  };
  let canonical = std::fs::canonicalize(&absolute).unwrap_or(absolute);
  strip_windows_verbatim_prefix(canonical)
}

/// Windows `canonicalize` returns extended-length verbatim paths
/// (`\\?\C:\...`); the prefix is rustc-internal noise that leaks into
/// statuses, results and every string comparison against a caller path.
fn strip_windows_verbatim_prefix(path: PathBuf) -> PathBuf {
  let Some(text) = path.to_str() else {
    return path;
  };
  if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
    PathBuf::from(format!(r"\\{unc}"))
  } else if let Some(drive) = text.strip_prefix(r"\\?\") {
    PathBuf::from(drive)
  } else {
    path
  }
}

pub(crate) fn lowercase_extension(path: &std::path::Path) -> Option<String> {
  path
    .extension()
    .and_then(|value| value.to_str())
    .map(|value| value.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn search_root_normalizes_relative_paths_to_absolute_paths() {
    let root = SearchRoot::new("relative-project");

    assert!(root.path.is_absolute());
    assert!(root.path.ends_with("relative-project"));
  }
}
