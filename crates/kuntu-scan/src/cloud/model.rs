use serde::{Serialize, Serializer};
use std::fmt;
use std::path::Path;

pub type Result<T> = std::result::Result<T, CloudError>;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Bytes(pub u64);

impl Serialize for Bytes {
  fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&self.0.to_string())
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
  UnsupportedPlatform,
  InvalidPath,
  SymbolicLink,
  OutsideScope,
  CrossDevice,
  ChangedSincePlan,
  NotICloud,
  InvalidState,
  Io,
  Native,
  IncompletePlan,
  ExpiredPlan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NativeError {
  pub domain: String,
  pub code: i64,
  pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CloudError {
  pub kind: ErrorKind,
  pub message: String,
  pub native: Option<NativeError>,
}

impl CloudError {
  pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
    Self {
      kind,
      message: message.into(),
      native: None,
    }
  }

  pub fn native(error: NativeError) -> Self {
    Self {
      kind: ErrorKind::Native,
      message: error.description.clone(),
      native: Some(error),
    }
  }

  pub(crate) fn io(path: &Path, error: std::io::Error) -> Self {
    Self::new(ErrorKind::Io, format!("{}: {error}", path.display()))
  }
}

impl fmt::Display for CloudError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str(&self.message)
  }
}

impl std::error::Error for CloudError {}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
  RegularFile,
  Directory,
  SymbolicLink,
  Other,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Fingerprint {
  pub device: u64,
  pub inode: u64,
  pub kind: ItemKind,
  pub logical_bytes: u64,
  pub links: u64,
  pub modified_sec: i64,
  pub modified_nsec: i64,
  pub changed_sec: i64,
  pub changed_nsec: i64,
}

impl Fingerprint {
  pub fn same_object(&self, other: &Self) -> bool {
    self.device == other.device && self.inode == other.inode && self.kind == other.kind
  }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
  CloudOnly,
  LocalCurrent,
  LocalStale,
  Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ItemInfo {
  #[serde(skip)]
  pub fingerprint: Fingerprint,
  pub allocated_bytes: Option<Bytes>,
  pub foundation_allocated_bytes: Option<Bytes>,
  pub foundation_total_allocated_bytes: Option<Bytes>,
  pub is_icloud: bool,
  pub is_package: Option<bool>,
  pub download_state: DownloadState,
  pub is_uploaded: Option<bool>,
  pub is_uploading: Option<bool>,
  pub is_downloading: Option<bool>,
  pub has_conflicts: Option<bool>,
  pub upload_error: Option<NativeError>,
  pub download_error: Option<NativeError>,
}

impl ItemInfo {
  pub fn logical_bytes(&self) -> Bytes {
    Bytes(self.fingerprint.logical_bytes)
  }

  pub fn kind(&self) -> ItemKind {
    self.fingerprint.kind
  }
}
