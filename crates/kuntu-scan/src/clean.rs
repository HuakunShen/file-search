use crate::scanner::{measure_path, scan_directory, IgnoredMode, ScanNode, ScanOptions};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CleanupPreset {
  Node,
  Rust,
  Gitignored,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CandidateOptions {
  pub roots: Vec<PathBuf>,
  pub presets: Vec<CleanupPreset>,
  pub ignore_hidden: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupCandidate {
  pub path: PathBuf,
  pub size: u64,
  pub reason: String,
  pub preset: CleanupPreset,
  pub ignored: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemovalEntry {
  pub path: PathBuf,
  pub size: u64,
  pub reason: String,
  pub preset: CleanupPreset,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RemovalPlan {
  pub entries: Vec<RemovalEntry>,
  pub total_size: u64,
  pub errors: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RemovalOutcome {
  pub removed: Vec<RemovalEntry>,
  pub bytes_removed: u64,
  pub errors: Vec<String>,
}

impl CandidateOptions {
  fn effective_presets(&self) -> Vec<CleanupPreset> {
    if self.presets.is_empty() {
      return vec![
        CleanupPreset::Node,
        CleanupPreset::Rust,
        CleanupPreset::Gitignored,
      ];
    }

    self.presets.clone()
  }
}

pub fn find_candidates(options: CandidateOptions) -> Vec<CleanupCandidate> {
  let mut candidates = Vec::new();
  let mut seen = HashSet::new();

  for preset in options.effective_presets() {
    match preset {
      CleanupPreset::Node => find_named_dir_candidates(
        &options.roots,
        "node_modules",
        CleanupPreset::Node,
        "Node dependency directory",
        options.ignore_hidden,
        &mut seen,
        &mut candidates,
      ),
      CleanupPreset::Rust => find_named_dir_candidates(
        &options.roots,
        "target",
        CleanupPreset::Rust,
        "Cargo build output directory",
        options.ignore_hidden,
        &mut seen,
        &mut candidates,
      ),
      CleanupPreset::Gitignored => {
        find_gitignored_candidates(&options, &mut seen, &mut candidates);
      }
    }
  }

  candidates.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
  candidates
}

pub fn build_removal_plan(candidates: Vec<CleanupCandidate>) -> RemovalPlan {
  let entries = candidates
    .into_iter()
    .map(|candidate| RemovalEntry {
      path: candidate.path,
      size: candidate.size,
      reason: candidate.reason,
      preset: candidate.preset,
    })
    .collect::<Vec<_>>();
  let total_size = entries.iter().map(|entry| entry.size).sum();

  RemovalPlan {
    entries,
    total_size,
    errors: Vec::new(),
  }
}

pub fn execute_removal_plan(plan: &RemovalPlan) -> RemovalOutcome {
  let mut outcome = RemovalOutcome::default();

  for entry in &plan.entries {
    match remove_path(&entry.path) {
      Ok(()) => {
        outcome.bytes_removed = outcome.bytes_removed.saturating_add(entry.size);
        outcome.removed.push(entry.clone());
      }
      Err(error) => outcome
        .errors
        .push(format!("{}: {}", entry.path.display(), error)),
    }
  }

  outcome
}

fn find_named_dir_candidates(
  roots: &[PathBuf],
  dir_name: &str,
  preset: CleanupPreset,
  reason: &str,
  ignore_hidden: bool,
  seen: &mut HashSet<PathBuf>,
  candidates: &mut Vec<CleanupCandidate>,
) {
  for root in roots {
    visit_named_dir_candidate(
      root,
      dir_name,
      preset,
      reason,
      ignore_hidden,
      seen,
      candidates,
    );
  }
}

fn visit_named_dir_candidate(
  path: &Path,
  dir_name: &str,
  preset: CleanupPreset,
  reason: &str,
  ignore_hidden: bool,
  seen: &mut HashSet<PathBuf>,
  candidates: &mut Vec<CleanupCandidate>,
) {
  if ignore_hidden && is_hidden(path) {
    return;
  }

  let metadata = match std::fs::symlink_metadata(path) {
    Ok(metadata) => metadata,
    Err(_) => return,
  };

  if !metadata.is_dir() {
    return;
  }

  if path.file_name().and_then(|name| name.to_str()) == Some(dir_name) {
    push_candidate(path, preset, reason, false, seen, candidates);
    return;
  }

  let entries = match std::fs::read_dir(path) {
    Ok(entries) => entries,
    Err(_) => return,
  };

  for entry in entries.flatten() {
    visit_named_dir_candidate(
      &entry.path(),
      dir_name,
      preset,
      reason,
      ignore_hidden,
      seen,
      candidates,
    );
  }
}

fn find_gitignored_candidates(
  options: &CandidateOptions,
  seen: &mut HashSet<PathBuf>,
  candidates: &mut Vec<CleanupCandidate>,
) {
  let trees = scan_directory(ScanOptions {
    directories: options.roots.clone(),
    ignore_hidden: options.ignore_hidden,
    full_path: true,
    respect_gitignore: true,
    ignored_mode: IgnoredMode::Summarize,
  });

  for tree in trees {
    collect_ignored_nodes(&tree, seen, candidates);
  }
}

fn collect_ignored_nodes(
  node: &ScanNode,
  seen: &mut HashSet<PathBuf>,
  candidates: &mut Vec<CleanupCandidate>,
) {
  if node.ignored {
    push_candidate(
      &node.path,
      CleanupPreset::Gitignored,
      "Path matched by .gitignore",
      true,
      seen,
      candidates,
    );
    return;
  }

  for child in &node.children {
    collect_ignored_nodes(child, seen, candidates);
  }
}

fn push_candidate(
  path: &Path,
  preset: CleanupPreset,
  reason: &str,
  ignored: bool,
  seen: &mut HashSet<PathBuf>,
  candidates: &mut Vec<CleanupCandidate>,
) {
  let path = path.to_path_buf();
  if !seen.insert(path.clone()) {
    return;
  }

  candidates.push(CleanupCandidate {
    size: measure_path(&path),
    path,
    reason: reason.to_string(),
    preset,
    ignored,
  });
}

fn remove_path(path: &Path) -> std::io::Result<()> {
  let metadata = std::fs::symlink_metadata(path)?;
  if metadata.is_dir() {
    std::fs::remove_dir_all(path)
  } else {
    std::fs::remove_file(path)
  }
}

pub fn delete_path(path: &Path) -> std::io::Result<()> {
  remove_path(path)
}

fn is_hidden(path: &Path) -> bool {
  path
    .file_name()
    .and_then(|name| name.to_str())
    .is_some_and(|name| name.starts_with('.'))
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs::{create_dir_all, remove_dir_all, write};
  use std::time::{SystemTime, UNIX_EPOCH};

  #[test]
  fn finds_cleanup_candidates_from_presets() {
    let root = fixture("candidates");
    create_dir_all(root.join("app/node_modules/pkg")).unwrap();
    create_dir_all(root.join("app/target/debug")).unwrap();
    create_dir_all(root.join("app/ignored")).unwrap();
    write(root.join("app/.gitignore"), "ignored/\n*.log\n").unwrap();
    write(
      root.join("app/node_modules/pkg/index.js"),
      "module.exports = 1\n",
    )
    .unwrap();
    write(root.join("app/target/debug/app"), "binary\n").unwrap();
    write(root.join("app/ignored/cache.bin"), "cache\n").unwrap();
    write(root.join("app/debug.log"), "log\n").unwrap();

    let candidates = find_candidates(CandidateOptions {
      roots: vec![root.join("app")],
      presets: vec![],
      ignore_hidden: false,
    });

    assert!(candidates
      .iter()
      .any(|candidate| candidate.path.ends_with("node_modules")));
    assert!(candidates
      .iter()
      .any(|candidate| candidate.path.ends_with("target")));
    assert!(candidates
      .iter()
      .any(|candidate| candidate.path.ends_with("ignored")));
    assert!(candidates
      .iter()
      .any(|candidate| candidate.path.ends_with("debug.log")));

    remove_dir_all(root).unwrap();
  }

  #[test]
  fn removal_plan_is_dry_run_until_executed() {
    let root = fixture("removal");
    create_dir_all(root.join("node_modules/pkg")).unwrap();
    write(
      root.join("node_modules/pkg/index.js"),
      "module.exports = 1\n",
    )
    .unwrap();

    let candidates = find_candidates(CandidateOptions {
      roots: vec![root.clone()],
      presets: vec![CleanupPreset::Node],
      ignore_hidden: false,
    });
    let plan = build_removal_plan(candidates);

    assert_eq!(plan.entries.len(), 1);
    assert!(root.join("node_modules").exists());

    let outcome = execute_removal_plan(&plan);

    assert_eq!(outcome.errors, Vec::<String>::new());
    assert_eq!(outcome.removed.len(), 1);
    assert!(!root.join("node_modules").exists());

    remove_dir_all(root).unwrap();
  }

  #[test]
  fn deletes_an_arbitrary_file_or_directory_path() {
    let root = fixture("arbitrary-delete");
    create_dir_all(root.join("folder/nested")).unwrap();
    write(root.join("folder/nested/file.txt"), "delete me\n").unwrap();

    delete_path(&root.join("folder")).unwrap();

    assert!(!root.join("folder").exists());
    remove_dir_all(root).unwrap();
  }

  fn fixture(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap()
      .as_nanos();
    let root = std::env::temp_dir().join(format!("space-lens-{name}-{nanos}"));
    create_dir_all(&root).unwrap();
    root
  }
}
