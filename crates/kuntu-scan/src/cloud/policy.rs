use super::model::{DownloadState, ItemInfo, ItemKind};
use serde::Serialize;
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
  SymbolicLink,
  NotRegularFile,
  Package,
  UnknownPackageState,
  HardLinked,
  NotICloud,
  AlreadyCloudOnly,
  StaleLocalCopy,
  UnknownDownloadState,
  NotUploaded,
  UnknownUploadState,
  Uploading,
  UnknownDownloadingState,
  Downloading,
  UnknownConflictState,
  HasConflicts,
  TransferError,
  UnknownLocalAllocation,
  ZeroLocalAllocation,
}

impl fmt::Display for SkipReason {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str(match self {
      Self::SymbolicLink => "symbolic link",
      Self::NotRegularFile => "not a regular file",
      Self::Package => "package",
      Self::UnknownPackageState => "package status unknown",
      Self::HardLinked => "multiple hard links",
      Self::NotICloud => "not an iCloud item",
      Self::AlreadyCloudOnly => "already cloud-only",
      Self::StaleLocalCopy => "local download status is not current",
      Self::UnknownDownloadState => "download status unknown",
      Self::NotUploaded => "not uploaded",
      Self::UnknownUploadState => "upload status unknown",
      Self::Uploading => "currently uploading",
      Self::UnknownDownloadingState => "download activity status unknown",
      Self::Downloading => "currently downloading",
      Self::UnknownConflictState => "conflict status unknown",
      Self::HasConflicts => "has unresolved conflicts",
      Self::TransferError => "has a transfer error",
      Self::UnknownLocalAllocation => "local allocation unknown",
      Self::ZeroLocalAllocation => "local allocation is zero",
    })
  }
}

pub fn eviction_skip_reason(info: &ItemInfo) -> Option<SkipReason> {
  match info.kind() {
    ItemKind::SymbolicLink => return Some(SkipReason::SymbolicLink),
    ItemKind::RegularFile => {}
    ItemKind::Directory | ItemKind::Other => return Some(SkipReason::NotRegularFile),
  }

  match info.is_package {
    Some(true) => return Some(SkipReason::Package),
    Some(false) => {}
    None => return Some(SkipReason::UnknownPackageState),
  }

  if info.fingerprint.links > 1 {
    return Some(SkipReason::HardLinked);
  }

  if !info.is_icloud {
    return Some(SkipReason::NotICloud);
  }

  match info.download_state {
    DownloadState::CloudOnly => return Some(SkipReason::AlreadyCloudOnly),
    DownloadState::LocalCurrent => {}
    DownloadState::LocalStale => return Some(SkipReason::StaleLocalCopy),
    DownloadState::Unknown => return Some(SkipReason::UnknownDownloadState),
  }

  match info.is_uploaded {
    Some(true) => {}
    Some(false) => return Some(SkipReason::NotUploaded),
    None => return Some(SkipReason::UnknownUploadState),
  }

  match info.is_uploading {
    Some(false) => {}
    Some(true) => return Some(SkipReason::Uploading),
    None => return Some(SkipReason::UnknownUploadState),
  }

  match info.is_downloading {
    Some(false) => {}
    Some(true) => return Some(SkipReason::Downloading),
    None => return Some(SkipReason::UnknownDownloadingState),
  }

  match info.has_conflicts {
    Some(false) => {}
    Some(true) => return Some(SkipReason::HasConflicts),
    None => return Some(SkipReason::UnknownConflictState),
  }

  if info.upload_error.is_some() || info.download_error.is_some() {
    return Some(SkipReason::TransferError);
  }

  match info.allocated_bytes {
    Some(bytes) if bytes.0 > 0 => None,
    Some(_) => Some(SkipReason::ZeroLocalAllocation),
    None => Some(SkipReason::UnknownLocalAllocation),
  }
}
