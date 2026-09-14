use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rayon::iter::ParallelBridge;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IgnoredMode {
  Exclude,
  Summarize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScanOptions {
  pub directories: Vec<PathBuf>,
  pub ignore_hidden: bool,
  pub full_path: bool,
  pub respect_gitignore: bool,
  pub ignored_mode: IgnoredMode,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScanNode {
  pub name: String,
  pub path: PathBuf,
  pub size: u64,
  pub children: Vec<ScanNode>,
  pub depth: u32,
  pub ignored: bool,
  pub collapsed: bool,
}

type IgnoreStack = Vec<Arc<Gitignore>>;
type SeenInodes = Arc<Mutex<HashSet<(u64, u64)>>>;

pub fn scan_directory(options: ScanOptions) -> Vec<ScanNode> {
  let seen_inodes = Arc::new(Mutex::new(HashSet::new()));

  options
    .directories
    .iter()
    .filter_map(|directory| scan_path(directory, 0, &[], false, false, &options, &seen_inodes))
    .collect()
}

pub fn measure_path(path: &Path) -> u64 {
  let seen_inodes = Arc::new(Mutex::new(HashSet::new()));
  summarize_path(path, &seen_inodes)
}

fn scan_path(
  path: &Path,
  depth: u32,
  ignore_stack: &[Arc<Gitignore>],
  ignored: bool,
  collapsed: bool,
  options: &ScanOptions,
  seen_inodes: &SeenInodes,
) -> Option<ScanNode> {
  let metadata = std::fs::symlink_metadata(path).ok()?;
  let own_size = unique_allocated_size(path, &metadata, seen_inodes)?;
  let is_dir = metadata.is_dir();

  if !is_dir {
    return Some(ScanNode {
      name: display_name(path, options.full_path),
      path: path.to_path_buf(),
      size: own_size,
      children: vec![],
      depth,
      ignored,
      collapsed,
    });
  }

  let current_stack = if options.respect_gitignore && !collapsed {
    append_gitignore(path, ignore_stack)
  } else {
    ignore_stack.to_vec()
  };

  if collapsed {
    return Some(ScanNode {
      name: display_name(path, options.full_path),
      path: path.to_path_buf(),
      size: own_size + summarize_dir_children(path, seen_inodes),
      children: vec![],
      depth,
      ignored,
      collapsed,
    });
  }

  let children = match std::fs::read_dir(path) {
    Ok(entries) => entries
      .par_bridge()
      .filter_map(|entry| {
        let entry = entry.ok()?;
        let entry_path = entry.path();
        let file_type = entry.file_type().ok()?;
        let is_entry_dir = file_type.is_dir();

        if options.ignore_hidden && is_hidden(&entry_path) {
          return None;
        }

        let is_ignored =
          options.respect_gitignore && is_gitignored(&entry_path, is_entry_dir, &current_stack);
        if is_ignored && options.ignored_mode == IgnoredMode::Exclude {
          return None;
        }

        let collapse_child =
          is_ignored && options.ignored_mode == IgnoredMode::Summarize && is_entry_dir;

        scan_path(
          &entry_path,
          if is_entry_dir { depth + 1 } else { depth },
          &current_stack,
          is_ignored,
          collapse_child,
          options,
          seen_inodes,
        )
      })
      .collect::<Vec<_>>(),
    Err(_) => vec![],
  };

  let children_size = children.iter().map(|child| child.size).sum::<u64>();

  Some(ScanNode {
    name: display_name(path, options.full_path),
    path: path.to_path_buf(),
    size: own_size + children_size,
    children,
    depth,
    ignored,
    collapsed,
  })
}

fn summarize_path(path: &Path, seen_inodes: &SeenInodes) -> u64 {
  let metadata = match std::fs::symlink_metadata(path) {
    Ok(metadata) => metadata,
    Err(_) => return 0,
  };
  let own_size = unique_allocated_size(path, &metadata, seen_inodes).unwrap_or(0);

  if metadata.is_dir() {
    own_size + summarize_dir_children(path, seen_inodes)
  } else {
    own_size
  }
}

fn append_gitignore(path: &Path, ignore_stack: &[Arc<Gitignore>]) -> IgnoreStack {
  let gitignore_path = path.join(".gitignore");
  if !gitignore_path.exists() {
    return ignore_stack.to_vec();
  }

  let mut builder = GitignoreBuilder::new(path);
  let _ = builder.add(&gitignore_path);

  match builder.build() {
    Ok(gitignore) => {
      let mut next = ignore_stack.to_vec();
      next.push(Arc::new(gitignore));
      next
    }
    Err(_) => ignore_stack.to_vec(),
  }
}

fn is_gitignored(path: &Path, is_dir: bool, ignore_stack: &[Arc<Gitignore>]) -> bool {
  let mut ignored = false;

  for gitignore in ignore_stack {
    let matched = gitignore.matched(path, is_dir);
    if matched.is_ignore() {
      ignored = true;
    } else if matched.is_whitelist() {
      ignored = false;
    }
  }

  ignored
}

fn summarize_dir_children(path: &Path, seen_inodes: &SeenInodes) -> u64 {
  match std::fs::read_dir(path) {
    Ok(entries) => entries
      .par_bridge()
      .filter_map(|entry| {
        let entry = entry.ok()?;
        let entry_path = entry.path();
        let metadata = std::fs::symlink_metadata(&entry_path).ok()?;
        let own_size = unique_allocated_size(&entry_path, &metadata, seen_inodes)?;

        if metadata.is_dir() {
          Some(own_size + summarize_dir_children(&entry_path, seen_inodes))
        } else {
          Some(own_size)
        }
      })
      .sum(),
    Err(_) => 0,
  }
}

fn display_name(path: &Path, full_path: bool) -> String {
  if full_path {
    return path.to_string_lossy().to_string();
  }

  path
    .file_name()
    .map(|name| name.to_string_lossy().to_string())
    .unwrap_or_else(|| path.to_string_lossy().to_string())
}

fn is_hidden(path: &Path) -> bool {
  path
    .file_name()
    .and_then(|name| name.to_str())
    .is_some_and(|name| name.starts_with('.'))
}

fn unique_allocated_size(
  path: &Path,
  metadata: &std::fs::Metadata,
  seen_inodes: &SeenInodes,
) -> Option<u64> {
  if let Some(key) = inode_key(path, metadata) {
    let mut seen = seen_inodes.lock().ok()?;
    if !seen.insert(key) {
      return None;
    }
  }

  Some(allocated_size(metadata))
}

#[cfg(unix)]
fn inode_key(_path: &Path, metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
  use std::os::unix::fs::MetadataExt;

  Some((metadata.ino(), metadata.dev()))
}

#[cfg(windows)]
fn inode_key(path: &Path, metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
  use std::mem::MaybeUninit;
  use std::os::windows::io::AsRawHandle;
  use windows_sys::Win32::Foundation::HANDLE;
  use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
  };

  if metadata.is_dir() {
    return None;
  }

  let file = std::fs::File::open(path).ok()?;
  let mut info = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
  let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, info.as_mut_ptr()) };
  if ok == 0 {
    return None;
  }

  let info = unsafe { info.assume_init() };
  let file_index = ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64;
  Some((file_index, info.dwVolumeSerialNumber as u64))
}

#[cfg(not(any(unix, windows)))]
fn inode_key(_path: &Path, _metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
  None
}

#[cfg(unix)]
fn allocated_size(metadata: &std::fs::Metadata) -> u64 {
  use std::os::unix::fs::MetadataExt;

  metadata.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn allocated_size(metadata: &std::fs::Metadata) -> u64 {
  metadata.len()
}
