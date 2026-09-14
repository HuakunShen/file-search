use super::model::{CloudError, ErrorKind, Fingerprint, ItemKind, Result};
use std::fs::{self, Metadata};
use std::path::{Component, Path, PathBuf};

pub fn absolute_path(path: &Path) -> Result<PathBuf> {
  if !path.is_absolute() {
    return Err(CloudError::new(
      ErrorKind::InvalidPath,
      format!("path must be absolute: {}", path.display()),
    ));
  }

  let mut normalized = PathBuf::new();
  for component in path.components() {
    match component {
      Component::RootDir | Component::Prefix(_) => normalized.push(component.as_os_str()),
      Component::CurDir => {}
      Component::ParentDir => {
        return Err(CloudError::new(
          ErrorKind::InvalidPath,
          format!("path must not contain '..': {}", path.display()),
        ));
      }
      Component::Normal(value) => normalized.push(value),
    }
  }

  Ok(normalized)
}

pub fn ensure_within_scope(root: &Path, path: &Path) -> Result<()> {
  let root = absolute_path(root)?;
  let path = absolute_path(path)?;

  if path == root || path.starts_with(&root) {
    Ok(())
  } else {
    Err(CloudError::new(
      ErrorKind::OutsideScope,
      format!("{} is outside {}", path.display(), root.display()),
    ))
  }
}

/// Production caller is the macOS iCloud backend (platform.rs); kept
/// cross-platform for the test suite and future non-Apple backends.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn fingerprint_for_path(path: &Path) -> Result<Fingerprint> {
  let metadata = fs::symlink_metadata(path).map_err(|error| CloudError::io(path, error))?;
  Ok(fingerprint_from_metadata(&metadata))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn allocated_bytes_for_path(path: &Path) -> Result<super::model::Bytes> {
  let metadata = fs::symlink_metadata(path).map_err(|error| CloudError::io(path, error))?;
  Ok(super::model::Bytes(allocated_size(&metadata)))
}

pub(crate) fn fingerprint_from_metadata(metadata: &Metadata) -> Fingerprint {
  Fingerprint {
    device: device(metadata),
    inode: inode(metadata),
    kind: item_kind(metadata),
    logical_bytes: metadata.len(),
    links: links(metadata),
    modified_sec: modified_sec(metadata),
    modified_nsec: modified_nsec(metadata),
    changed_sec: changed_sec(metadata),
    changed_nsec: changed_nsec(metadata),
  }
}

fn item_kind(metadata: &Metadata) -> ItemKind {
  let file_type = metadata.file_type();
  if file_type.is_symlink() {
    ItemKind::SymbolicLink
  } else if file_type.is_file() {
    ItemKind::RegularFile
  } else if file_type.is_dir() {
    ItemKind::Directory
  } else {
    ItemKind::Other
  }
}

#[cfg(unix)]
fn device(metadata: &Metadata) -> u64 {
  use std::os::unix::fs::MetadataExt;

  metadata.dev()
}

#[cfg(not(unix))]
fn device(_metadata: &Metadata) -> u64 {
  0
}

#[cfg(unix)]
fn inode(metadata: &Metadata) -> u64 {
  use std::os::unix::fs::MetadataExt;

  metadata.ino()
}

#[cfg(not(unix))]
fn inode(_metadata: &Metadata) -> u64 {
  0
}

#[cfg(unix)]
fn links(metadata: &Metadata) -> u64 {
  use std::os::unix::fs::MetadataExt;

  metadata.nlink()
}

#[cfg(not(unix))]
fn links(_metadata: &Metadata) -> u64 {
  1
}

#[cfg(unix)]
fn modified_sec(metadata: &Metadata) -> i64 {
  use std::os::unix::fs::MetadataExt;

  metadata.mtime()
}

#[cfg(not(unix))]
fn modified_sec(_metadata: &Metadata) -> i64 {
  0
}

#[cfg(unix)]
fn modified_nsec(metadata: &Metadata) -> i64 {
  use std::os::unix::fs::MetadataExt;

  metadata.mtime_nsec()
}

#[cfg(not(unix))]
fn modified_nsec(_metadata: &Metadata) -> i64 {
  0
}

#[cfg(unix)]
fn changed_sec(metadata: &Metadata) -> i64 {
  use std::os::unix::fs::MetadataExt;

  metadata.ctime()
}

#[cfg(not(unix))]
fn changed_sec(_metadata: &Metadata) -> i64 {
  0
}

#[cfg(unix)]
fn changed_nsec(metadata: &Metadata) -> i64 {
  use std::os::unix::fs::MetadataExt;

  metadata.ctime_nsec()
}

#[cfg(not(unix))]
fn changed_nsec(_metadata: &Metadata) -> i64 {
  0
}

#[cfg(unix)]
fn allocated_size(metadata: &Metadata) -> u64 {
  use std::os::unix::fs::MetadataExt;

  metadata.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn allocated_size(metadata: &Metadata) -> u64 {
  metadata.len()
}
