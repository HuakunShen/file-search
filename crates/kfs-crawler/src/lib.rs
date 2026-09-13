//! Explicit-root filesystem crawler for the Kunkun file search prototype.
//!
//! The crawler only walks roots supplied by the caller and applies `kfs-core`
//! policy before yielding entries. Denied directories are not descended into,
//! which keeps generated and sensitive trees out of the index.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use kfs_core::{EntryKind, PathPolicy, PolicyDecision, SearchConfig, SearchQuery};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrawledEntry {
  pub path: PathBuf,
  pub kind: EntryKind,
  pub size: Option<u64>,
  pub mtime: Option<i64>,
  pub decision: PolicyDecision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrawlStats {
  pub visited: usize,
  pub yielded: usize,
  pub skipped: usize,
  pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CrawlOptions {
  pub config: SearchConfig,
  pub query: SearchQuery,
  pub max_depth: Option<usize>,
}

impl CrawlOptions {
  pub fn new(config: SearchConfig) -> Self {
    Self {
      config,
      query: SearchQuery::new(""),
      max_depth: None,
    }
  }
}

pub fn crawl(options: &CrawlOptions) -> (Vec<CrawledEntry>, CrawlStats) {
  let policy = PathPolicy::new(options.config.clone());
  let mut entries = Vec::new();
  let mut stats = CrawlStats {
    visited: 0,
    yielded: 0,
    skipped: 0,
    errors: Vec::new(),
  };

  for root in options.config.roots.iter().filter(|root| root.enabled) {
    crawl_one_root(
      &root.path,
      0,
      options.max_depth,
      &policy,
      &options.query,
      &mut entries,
      &mut stats,
    );
  }

  (entries, stats)
}

fn crawl_one_root(
  path: &Path,
  depth: usize,
  max_depth: Option<usize>,
  policy: &PathPolicy,
  query: &SearchQuery,
  entries: &mut Vec<CrawledEntry>,
  stats: &mut CrawlStats,
) {
  stats.visited += 1;
  let decision = policy.evaluate(path, Some(query));
  if !decision.allowed {
    stats.skipped += 1;
    return;
  }

  let metadata = match fs::symlink_metadata(path) {
    Ok(metadata) => metadata,
    Err(err) => {
      stats.errors.push(format!("{}: {err}", path.display()));
      return;
    }
  };

  let kind = if metadata.is_dir() {
    EntryKind::Directory
  } else if metadata.is_file() {
    EntryKind::File
  } else {
    EntryKind::Other
  };
  let mtime = metadata
    .modified()
    .ok()
    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
    .and_then(|duration| i64::try_from(duration.as_secs()).ok());
  let size = metadata.is_file().then_some(metadata.len());

  entries.push(CrawledEntry {
    path: decision.path.clone(),
    kind,
    size,
    mtime,
    decision,
  });
  stats.yielded += 1;

  if metadata.is_dir() && max_depth.is_none_or(|max_depth| depth < max_depth) {
    let read_dir = match fs::read_dir(path) {
      Ok(read_dir) => read_dir,
      Err(err) => {
        stats.errors.push(format!("{}: {err}", path.display()));
        return;
      }
    };
    for child in read_dir {
      match child {
        Ok(child) => crawl_one_root(
          &child.path(),
          depth + 1,
          max_depth,
          policy,
          query,
          entries,
          stats,
        ),
        Err(err) => stats.errors.push(format!("{}: {err}", path.display())),
      }
    }
  }
}

pub fn remove_dir_all_if_exists(path: &Path) -> io::Result<()> {
  match fs::remove_dir_all(path) {
    Ok(()) => Ok(()),
    Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
    Err(err) => Err(err),
  }
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::time::{SystemTime, UNIX_EPOCH};

  use kfs_core::{SearchConfig, SearchRoot};

  use super::*;

  fn temp_dir(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap()
      .as_nanos();
    std::env::temp_dir().join(format!("kfs-crawler-{name}-{nonce}"))
  }

  #[test]
  fn crawl_yields_allowed_files_and_skips_ignored_directories() {
    let root = temp_dir("skip-ignored");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(root.join("node_modules/pkg/index.js"), "ignored\n").unwrap();

    let options = CrawlOptions::new(SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    });
    let (entries, stats) = crawl(&options);
    remove_dir_all_if_exists(&root).unwrap();

    assert!(stats.visited >= 3);
    assert!(stats.skipped >= 1);
    assert!(entries
      .iter()
      .any(|entry| entry.path.ends_with("src/main.rs")));
    assert!(!entries
      .iter()
      .any(|entry| entry.path.ends_with("node_modules/pkg/index.js")));
  }

  #[test]
  fn crawl_does_not_descend_sensitive_directories() {
    let root = temp_dir("sensitive");
    fs::create_dir_all(root.join(".ssh")).unwrap();
    fs::write(root.join(".ssh/id_rsa"), "secret\n").unwrap();

    let options = CrawlOptions::new(SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    });
    let (entries, stats) = crawl(&options);
    remove_dir_all_if_exists(&root).unwrap();

    assert!(stats.skipped >= 1);
    assert!(!entries.iter().any(|entry| entry.path.ends_with("id_rsa")));
  }
}
