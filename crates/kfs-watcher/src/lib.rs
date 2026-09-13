//! Cross-platform watcher event model and bounded polling implementation.
//!
//! This crate intentionally exposes platform-neutral watch events. The current
//! implementation uses std-only polling so tests and CLI smoke runs are
//! bounded and portable; native macOS/Linux/Windows watcher backends can later
//! implement the same event model behind conditional compilation.

use std::collections::HashMap;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use kfs_core::{BackendError, EntryKind, SearchConfig};
use kfs_crawler::{crawl, CrawlOptions};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchEventKind {
  Created,
  Modified,
  Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchEvent {
  pub path: PathBuf,
  pub kind: WatchEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryFingerprint {
  pub kind: EntryKind,
  pub size: Option<u64>,
  pub mtime: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchSnapshot {
  pub entries: HashMap<PathBuf, EntryFingerprint>,
  pub skipped: usize,
  pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WatchOptions {
  pub config: SearchConfig,
  pub duration: Duration,
  pub poll_interval: Duration,
  pub max_events: usize,
}

impl WatchOptions {
  pub fn new(config: SearchConfig) -> Self {
    Self {
      config,
      duration: Duration::from_secs(1),
      poll_interval: Duration::from_millis(250),
      max_events: 1024,
    }
  }

  pub fn with_duration(mut self, duration: Duration) -> Self {
    self.duration = duration;
    self
  }

  pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
    self.poll_interval = poll_interval;
    self
  }

  pub fn with_max_events(mut self, max_events: usize) -> Self {
    self.max_events = max_events;
    self
  }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchRunStats {
  pub events: usize,
  pub created: usize,
  pub modified: usize,
  pub deleted: usize,
  pub dirty: bool,
  pub errors: Vec<String>,
  pub elapsed_ms: u128,
}

pub fn snapshot(config: &SearchConfig) -> WatchSnapshot {
  let (entries, crawl_stats) = crawl(&CrawlOptions::new(config.clone()));
  WatchSnapshot {
    entries: entries
      .into_iter()
      .map(|entry| {
        (
          entry.path,
          EntryFingerprint {
            kind: entry.kind,
            size: entry.size,
            mtime: entry.mtime,
          },
        )
      })
      .collect(),
    skipped: crawl_stats.skipped,
    errors: crawl_stats.errors,
  }
}

pub fn diff_snapshots(before: &WatchSnapshot, after: &WatchSnapshot) -> Vec<WatchEvent> {
  let mut events = Vec::new();
  for (path, before_entry) in &before.entries {
    match after.entries.get(path) {
      Some(after_entry) if before_entry != after_entry => events.push(WatchEvent {
        path: path.clone(),
        kind: WatchEventKind::Modified,
      }),
      Some(_) => {}
      None => events.push(WatchEvent {
        path: path.clone(),
        kind: WatchEventKind::Deleted,
      }),
    }
  }
  for path in after.entries.keys() {
    if !before.entries.contains_key(path) {
      events.push(WatchEvent {
        path: path.clone(),
        kind: WatchEventKind::Created,
      });
    }
  }
  events.sort_by(|left, right| {
    left
      .path
      .cmp(&right.path)
      .then_with(|| event_kind_order(left.kind).cmp(&event_kind_order(right.kind)))
  });
  events
}

pub fn run_polling_watch<F>(
  options: &WatchOptions,
  mut on_event: F,
) -> Result<WatchRunStats, BackendError>
where
  F: FnMut(&WatchEvent) -> Result<(), BackendError>,
{
  let started = Instant::now();
  let mut stats = WatchRunStats::default();
  let mut previous = snapshot(&options.config);
  record_snapshot_health(&mut stats, &previous);

  while started.elapsed() < options.duration {
    let remaining = options.duration.saturating_sub(started.elapsed());
    thread::sleep(remaining.min(options.poll_interval));

    let next = snapshot(&options.config);
    record_snapshot_health(&mut stats, &next);
    for event in diff_snapshots(&previous, &next) {
      if stats.events >= options.max_events {
        stats.dirty = true;
        stats.elapsed_ms = started.elapsed().as_millis();
        return Ok(stats);
      }
      on_event(&event)?;
      stats.events += 1;
      match event.kind {
        WatchEventKind::Created => stats.created += 1,
        WatchEventKind::Modified => stats.modified += 1,
        WatchEventKind::Deleted => stats.deleted += 1,
      }
    }
    previous = next;
  }

  stats.elapsed_ms = started.elapsed().as_millis();
  Ok(stats)
}

fn record_snapshot_health(stats: &mut WatchRunStats, snapshot: &WatchSnapshot) {
  if !snapshot.errors.is_empty() {
    stats.dirty = true;
    stats.errors.extend(snapshot.errors.iter().cloned());
  }
}

fn event_kind_order(kind: WatchEventKind) -> u8 {
  match kind {
    WatchEventKind::Created => 0,
    WatchEventKind::Modified => 1,
    WatchEventKind::Deleted => 2,
  }
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::time::{SystemTime, UNIX_EPOCH};

  use kfs_core::SearchRoot;
  use kfs_crawler::remove_dir_all_if_exists;

  use super::*;

  fn fingerprint(size: u64) -> EntryFingerprint {
    EntryFingerprint {
      kind: EntryKind::File,
      size: Some(size),
      mtime: Some(i64::try_from(size).unwrap()),
    }
  }

  #[test]
  fn diff_snapshots_reports_created_modified_and_deleted_events() {
    let mut before = WatchSnapshot::default();
    before
      .entries
      .insert(PathBuf::from("/root/deleted.md"), fingerprint(1));
    before
      .entries
      .insert(PathBuf::from("/root/modified.md"), fingerprint(2));
    let mut after = WatchSnapshot::default();
    after
      .entries
      .insert(PathBuf::from("/root/created.md"), fingerprint(3));
    after
      .entries
      .insert(PathBuf::from("/root/modified.md"), fingerprint(4));

    let events = diff_snapshots(&before, &after);

    assert_eq!(events.len(), 3);
    assert!(events.iter().any(|event| {
      event.path.ends_with("created.md") && event.kind == WatchEventKind::Created
    }));
    assert!(events.iter().any(|event| {
      event.path.ends_with("modified.md") && event.kind == WatchEventKind::Modified
    }));
    assert!(events.iter().any(|event| {
      event.path.ends_with("deleted.md") && event.kind == WatchEventKind::Deleted
    }));
  }

  #[test]
  fn polling_watch_exits_after_duration_without_events() {
    let options = WatchOptions::new(SearchConfig::default())
      .with_duration(Duration::from_millis(1))
      .with_poll_interval(Duration::from_millis(1));

    let stats = run_polling_watch(&options, |_event| Ok(())).unwrap();

    assert_eq!(stats.events, 0);
    assert!(stats.elapsed_ms <= 250);
  }

  #[test]
  fn polling_watch_reports_file_created_during_run() {
    let root = temp_dir("created");
    fs::create_dir_all(&root).unwrap();
    let config = SearchConfig {
      roots: vec![SearchRoot::new(&root)],
    };
    let created_path = root.join("created-during-watch.md");
    let writer = std::thread::spawn({
      let created_path = created_path.clone();
      move || {
        std::thread::sleep(Duration::from_millis(50));
        fs::write(created_path, "created\n").unwrap();
      }
    });
    let options = WatchOptions::new(config)
      .with_duration(Duration::from_millis(300))
      .with_poll_interval(Duration::from_millis(25));

    let mut created = 0;
    let stats = run_polling_watch(&options, |event| {
      if event.path.ends_with("created-during-watch.md") && event.kind == WatchEventKind::Created {
        created += 1;
      }
      Ok(())
    })
    .unwrap();
    writer.join().unwrap();
    remove_dir_all_if_exists(&root).unwrap();

    assert_eq!(created, 1);
    assert_eq!(stats.created, 1);
  }

  fn temp_dir(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap()
      .as_nanos();
    std::env::temp_dir().join(format!("kfs-watcher-{name}-{nonce}"))
  }
}
