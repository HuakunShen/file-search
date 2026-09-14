use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use std::fmt;

pub const SNAPSHOT_FORMAT: &str = "space-lens.filesystem-snapshot";
pub const SNAPSHOT_SCHEMA_VERSION: &str = "1.0";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct DecimalBytes(pub u64);

impl Serialize for DecimalBytes {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    serializer.serialize_str(&self.0.to_string())
  }
}

impl<'de> Deserialize<'de> for DecimalBytes {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    struct DecimalBytesVisitor;

    impl<'de> Visitor<'de> for DecimalBytesVisitor {
      type Value = DecimalBytes;

      fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a decimal byte count encoded as a string")
      }

      fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
      where
        E: de::Error,
      {
        value
          .parse::<u64>()
          .map(DecimalBytes)
          .map_err(|_| E::custom("invalid decimal byte count"))
      }
    }

    deserializer.deserialize_str(DecimalBytesVisitor)
  }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotEnvelope {
  pub format: String,
  pub schema_version: String,
  pub producer: Producer,
  pub scan: ScanMetadata,
  pub roots: Vec<SnapshotRoot>,
  pub nodes: Vec<SnapshotNode>,
  pub errors: Vec<SnapshotError>,
  pub summary: SnapshotSummary,
}

impl SnapshotEnvelope {
  pub fn new(producer_version: impl Into<String>) -> Self {
    Self {
      format: SNAPSHOT_FORMAT.to_owned(),
      schema_version: SNAPSHOT_SCHEMA_VERSION.to_owned(),
      producer: Producer {
        name: "space-lens".to_owned(),
        version: producer_version.into(),
      },
      scan: ScanMetadata::default(),
      roots: Vec::new(),
      nodes: Vec::new(),
      errors: Vec::new(),
      summary: SnapshotSummary::default(),
    }
  }

  pub fn from_scan_nodes(
    nodes: &[crate::scanner::ScanNode],
    producer_version: impl Into<String>,
  ) -> Self {
    let mut snapshot = Self::new(producer_version);
    snapshot.scan.status = SnapshotStatus::Complete;
    snapshot.scan.privacy_mode = PrivacyMode::Full;

    for (root_index, node) in nodes.iter().enumerate() {
      let root_id = format!("root-{root_index}");
      snapshot.roots.push(SnapshotRoot {
        id: root_id.clone(),
        label: node.name.clone(),
        relative_path: Some(".".to_owned()),
      });
      append_scan_node(&mut snapshot, node, &root_id, None, &node.path, true);
    }

    snapshot.summary.node_count = snapshot.nodes.len() as u64;
    snapshot.summary.logical_bytes = Some(DecimalBytes(nodes.iter().map(|node| node.size).sum()));
    snapshot.summary.allocated_bytes = snapshot.summary.logical_bytes;
    snapshot.summary.unique_allocated_bytes = snapshot.summary.allocated_bytes;
    snapshot
  }
}

fn append_scan_node(
  snapshot: &mut SnapshotEnvelope,
  node: &crate::scanner::ScanNode,
  root_id: &str,
  parent_id: Option<String>,
  root_path: &std::path::Path,
  is_root: bool,
) {
  let node_id = format!("node-{}", snapshot.nodes.len());
  let relative_path = node.path.strip_prefix(root_path).ok().map(|path| {
    if path.as_os_str().is_empty() {
      ".".to_owned()
    } else {
      path.to_string_lossy().to_string()
    }
  });
  let kind = if is_root {
    NodeKind::Root
  } else if std::fs::symlink_metadata(&node.path)
    .map(|metadata| metadata.is_dir())
    .unwrap_or(!node.children.is_empty())
  {
    NodeKind::Directory
  } else {
    NodeKind::File
  };
  let mut flags = Vec::new();
  if node.ignored {
    flags.push(NodeFlag::Ignored);
  }
  if node.collapsed {
    flags.push(NodeFlag::Collapsed);
  }

  snapshot.nodes.push(SnapshotNode {
    id: node_id.clone(),
    parent_id,
    root_id: root_id.to_owned(),
    name: Some(node.name.clone()),
    relative_path,
    kind,
    logical_bytes: Some(DecimalBytes(node.size)),
    allocated_bytes: Some(DecimalBytes(node.size)),
    unique_allocated_bytes: Some(DecimalBytes(node.size)),
    child_count: node.children.len() as u64,
    mtime: None,
    file_identity: None,
    hardlink_group: None,
    flags,
    scan_state: if node.collapsed {
      NodeScanState::NotTraversed
    } else {
      NodeScanState::Complete
    },
    error_refs: Vec::new(),
  });

  let mut children = node.children.iter().collect::<Vec<_>>();
  children.sort_by(|left, right| {
    right
      .size
      .cmp(&left.size)
      .then_with(|| left.name.cmp(&right.name))
  });
  for child in children {
    append_scan_node(
      snapshot,
      child,
      root_id,
      Some(node_id.clone()),
      root_path,
      false,
    );
  }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Producer {
  pub name: String,
  pub version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanMetadata {
  pub id: String,
  pub started_at: Option<String>,
  pub completed_at: Option<String>,
  pub status: SnapshotStatus,
  pub size_policy: SizePolicy,
  pub hardlink_policy: HardlinkPolicy,
  pub symlink_policy: SymlinkPolicy,
  pub privacy_mode: PrivacyMode,
}

impl Default for ScanMetadata {
  fn default() -> Self {
    Self {
      id: "phase0-fixture".to_owned(),
      started_at: None,
      completed_at: None,
      status: SnapshotStatus::Partial,
      size_policy: SizePolicy::AllocatedPreferred,
      hardlink_policy: HardlinkPolicy::CountOncePerScan,
      symlink_policy: SymlinkPolicy::DoNotFollow,
      privacy_mode: PrivacyMode::NamesRedacted,
    }
  }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SnapshotStatus {
  Complete,
  Partial,
  Cancelled,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SizePolicy {
  AllocatedPreferred,
  LogicalOnly,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HardlinkPolicy {
  CountOncePerScan,
  CountPerPath,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SymlinkPolicy {
  DoNotFollow,
  FollowWithinRoot,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrivacyMode {
  Full,
  NamesRedacted,
  AggregateOnly,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotRoot {
  pub id: String,
  pub label: String,
  pub relative_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotNode {
  pub id: String,
  pub parent_id: Option<String>,
  pub root_id: String,
  pub name: Option<String>,
  pub relative_path: Option<String>,
  pub kind: NodeKind,
  pub logical_bytes: Option<DecimalBytes>,
  pub allocated_bytes: Option<DecimalBytes>,
  pub unique_allocated_bytes: Option<DecimalBytes>,
  pub child_count: u64,
  pub mtime: Option<String>,
  pub file_identity: Option<String>,
  pub hardlink_group: Option<String>,
  pub flags: Vec<NodeFlag>,
  pub scan_state: NodeScanState,
  pub error_refs: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeKind {
  Root,
  Directory,
  File,
  Symlink,
  Other,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeFlag {
  Ignored,
  ExcludedByPolicy,
  Collapsed,
  MountBoundary,
  Package,
  CloudOnly,
  HardlinkAlreadyAccounted,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeScanState {
  Complete,
  NotTraversed,
  PermissionDenied,
  Incomplete,
  ChangedDuringScan,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotError {
  pub id: String,
  pub domain: String,
  pub code: String,
  pub path: Option<String>,
  pub recoverability: ErrorRecoverability,
  pub message: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorRecoverability {
  Retry,
  UserAction,
  Skip,
  Fatal,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotSummary {
  pub node_count: u64,
  pub logical_bytes: Option<DecimalBytes>,
  pub allocated_bytes: Option<DecimalBytes>,
  pub unique_allocated_bytes: Option<DecimalBytes>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityState {
  Available,
  Unavailable,
  PermissionRequired,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformCapabilities {
  pub filesystem_scan: CapabilityState,
  pub trash_cleanup: CapabilityState,
  pub i_cloud_local_copy_eviction: CapabilityState,
  pub finder_integration: CapabilityState,
}

impl PlatformCapabilities {
  pub fn for_current_platform() -> Self {
    Self {
      filesystem_scan: CapabilityState::Available,
      trash_cleanup: CapabilityState::Available,
      i_cloud_local_copy_eviction: if cfg!(target_os = "macos") {
        CapabilityState::Available
      } else {
        CapabilityState::Unavailable
      },
      finder_integration: if cfg!(target_os = "macos") {
        CapabilityState::Available
      } else {
        CapabilityState::Unavailable
      },
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn round_trips_schema_fixture_with_decimal_bytes() {
    let mut snapshot = SnapshotEnvelope::new("0.3.0-phase0");
    snapshot.roots.push(SnapshotRoot {
      id: "root-1".into(),
      label: "Synthetic Volume".into(),
      relative_path: Some(".".into()),
    });
    snapshot.nodes.push(SnapshotNode {
      id: "node-1".into(),
      parent_id: None,
      root_id: "root-1".into(),
      name: Some("Synthetic Volume".into()),
      relative_path: Some(".".into()),
      kind: NodeKind::Root,
      logical_bytes: Some(DecimalBytes(9_007_199_254_740_993)),
      allocated_bytes: Some(DecimalBytes(8_000)),
      unique_allocated_bytes: Some(DecimalBytes(8_000)),
      child_count: 0,
      mtime: None,
      file_identity: None,
      hardlink_group: None,
      flags: vec![],
      scan_state: NodeScanState::Complete,
      error_refs: vec![],
    });

    let encoded = serde_json::to_string_pretty(&snapshot).expect("encode");
    assert!(encoded.contains("\"logicalBytes\": \"9007199254740993\""));

    let decoded: SnapshotEnvelope = serde_json::from_str(&encoded).expect("decode");
    assert_eq!(decoded, snapshot);
  }

  #[test]
  fn capabilities_hide_apple_only_features_off_macos() {
    let capabilities = PlatformCapabilities::for_current_platform();
    if cfg!(target_os = "macos") {
      assert_eq!(
        capabilities.i_cloud_local_copy_eviction,
        CapabilityState::Available
      );
    } else {
      assert_eq!(
        capabilities.i_cloud_local_copy_eviction,
        CapabilityState::Unavailable
      );
    }
  }
}
