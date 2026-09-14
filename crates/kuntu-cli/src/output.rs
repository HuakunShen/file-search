//! Output formatting for the local `kfs` CLI.
//!
//! Formatting is kept separate from command execution so tests and future
//! adapters can reuse the same stable text and JSON-like result rendering.

use kuntu_core::{
  ExplainResult, IndexRebuildStats, IndexRefreshStats, IndexRepairStats, IndexRootStatus,
  SearchResult,
};
use kuntu_daemon::DaemonRunStats;
use kuntu_watcher::WatchRunStats;

pub fn format_results_text(results: &[SearchResult]) -> String {
  if results.is_empty() {
    return "no results".to_string();
  }
  results
    .iter()
    .map(|result| {
      format!(
        "{}\t{}\t{}",
        result.score,
        result.provider,
        result.path.to_string_lossy()
      )
    })
    .collect::<Vec<_>>()
    .join("\n")
}

pub fn format_results_json(results: &[SearchResult]) -> String {
  let items = results
    .iter()
    .map(|result| {
      format!(
        "{{\"path\":\"{}\",\"score\":{},\"provider\":\"{}\",\"matches\":[{}]}}",
        escape_json(&result.path.to_string_lossy()),
        result.score,
        escape_json(&result.provider),
        result
          .matches
          .iter()
          .map(|kind| format!("\"{kind:?}\""))
          .collect::<Vec<_>>()
          .join(",")
      )
    })
    .collect::<Vec<_>>()
    .join(",");
  format!("[{items}]")
}

pub fn format_explain_text(explain: &ExplainResult) -> String {
  let root = explain
    .root
    .as_ref()
    .map(|path| path.to_string_lossy().into_owned())
    .unwrap_or_else(|| "-".to_string());
  format!(
    "path={}\nallowed={}\nroot={}\nhidden={}\nignored={}\nsensitive={}\nreasons={}",
    explain.path.to_string_lossy(),
    explain.allowed,
    root,
    explain.hidden,
    explain.ignored,
    explain.sensitive,
    explain.reasons.join(",")
  )
}

pub fn format_rebuild_stats(stats: &IndexRebuildStats) -> String {
  format!(
    "roots={} entries={} skipped={} errors={}",
    stats.roots,
    stats.entries,
    stats.skipped,
    stats.errors.len()
  )
}

pub fn format_refresh_stats(stats: &IndexRefreshStats) -> String {
  format!(
    "roots={} inserted={} updated={} deleted={} unchanged={} skipped={} errors={}",
    stats.roots,
    stats.inserted,
    stats.updated,
    stats.deleted,
    stats.unchanged,
    stats.skipped,
    stats.errors.len()
  )
}

pub fn format_repair_stats(stats: &IndexRepairStats) -> String {
  format!(
    "roots={} dirty_roots={} repaired_roots={} errors={}",
    stats.roots,
    stats.dirty_roots,
    stats.repaired_roots,
    stats.errors.len()
  )
}

pub fn format_status(status: &[IndexRootStatus]) -> String {
  if status.is_empty() {
    return "no indexed roots".to_string();
  }
  status
    .iter()
    .map(|root| {
      format!(
        "{}\tentries={}\tgeneration={}\tdirty={}",
        root.path.to_string_lossy(),
        root.entry_count,
        root.generation,
        root.dirty
      )
    })
    .collect::<Vec<_>>()
    .join("\n")
}

pub fn format_watch_stats(stats: &WatchRunStats) -> String {
  format!(
    "events={} created={} modified={} deleted={} dirty={} errors={} elapsed_ms={}",
    stats.events,
    stats.created,
    stats.modified,
    stats.deleted,
    stats.dirty,
    stats.errors.len(),
    stats.elapsed_ms
  )
}

pub fn format_bench_stats(candidate_count: usize, result_count: usize, elapsed_ms: u128) -> String {
  format!("candidates={candidate_count} results={result_count} elapsed_ms={elapsed_ms}")
}

pub fn format_daemon_stats(stats: &DaemonRunStats) -> String {
  format!(
    "addr={} requests={} elapsed_ms={}",
    stats.addr, stats.requests, stats.elapsed_ms
  )
}

fn escape_json(value: &str) -> String {
  let mut escaped = String::new();
  for ch in value.chars() {
    match ch {
      '"' => escaped.push_str("\\\""),
      '\\' => escaped.push_str("\\\\"),
      '\n' => escaped.push_str("\\n"),
      '\r' => escaped.push_str("\\r"),
      '\t' => escaped.push_str("\\t"),
      _ => escaped.push(ch),
    }
  }
  escaped
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use kuntu_core::{
    ExplainResult, IndexRebuildStats, IndexRefreshStats, IndexRepairStats, IndexRootStatus,
    MatchKind, SearchResult,
  };
  use kuntu_daemon::DaemonRunStats;
  use kuntu_watcher::WatchRunStats;

  use super::*;

  #[test]
  fn formats_json_results_with_escaped_paths() {
    let text = format_results_json(&[SearchResult {
      path: PathBuf::from("/Users/alice/Dev/a\"b"),
      score: 42,
      provider: "test".to_string(),
      matches: vec![MatchKind::BasenamePrefix],
      kind: kuntu_core::EntryKind::File,
      byte_size: None,
      modified_unix_seconds: None,
    }]);

    assert_eq!(
            text,
            "[{\"path\":\"/Users/alice/Dev/a\\\"b\",\"score\":42,\"provider\":\"test\",\"matches\":[\"BasenamePrefix\"]}]"
        );
  }

  #[test]
  fn formats_explain_text() {
    let text = format_explain_text(&ExplainResult {
      path: PathBuf::from("/Users/alice/Dev/project"),
      allowed: true,
      root: Some(PathBuf::from("/Users/alice/Dev")),
      hidden: false,
      ignored: false,
      sensitive: false,
      reasons: vec!["inside-root".to_string()],
      score: None,
      matches: Vec::new(),
    });

    assert!(text.contains("allowed=true"));
    assert!(text.contains("root=/Users/alice/Dev"));
  }

  #[test]
  fn formats_rebuild_stats() {
    let text = format_rebuild_stats(&IndexRebuildStats {
      roots: 1,
      entries: 3,
      skipped: 2,
      errors: Vec::new(),
    });

    assert_eq!(text, "roots=1 entries=3 skipped=2 errors=0");
  }

  #[test]
  fn formats_status_rows() {
    let text = format_status(&[IndexRootStatus {
      path: PathBuf::from("/Users/alice/Dev"),
      entry_count: 10,
      generation: 2,
      dirty: false,
      last_full_scan_at: Some(1),
      last_incremental_at: None,
    }]);

    assert!(text.contains("/Users/alice/Dev"));
    assert!(text.contains("entries=10"));
  }

  #[test]
  fn formats_refresh_stats() {
    let text = format_refresh_stats(&IndexRefreshStats {
      roots: 1,
      inserted: 2,
      updated: 3,
      deleted: 4,
      unchanged: 5,
      skipped: 6,
      errors: Vec::new(),
    });

    assert_eq!(
      text,
      "roots=1 inserted=2 updated=3 deleted=4 unchanged=5 skipped=6 errors=0"
    );
  }

  #[test]
  fn formats_repair_stats() {
    let text = format_repair_stats(&IndexRepairStats {
      roots: 2,
      dirty_roots: 1,
      repaired_roots: 1,
      errors: Vec::new(),
    });

    assert_eq!(text, "roots=2 dirty_roots=1 repaired_roots=1 errors=0");
  }

  #[test]
  fn formats_watch_stats() {
    let text = format_watch_stats(&WatchRunStats {
      events: 3,
      created: 1,
      modified: 1,
      deleted: 1,
      dirty: false,
      errors: Vec::new(),
      elapsed_ms: 42,
    });

    assert_eq!(
      text,
      "events=3 created=1 modified=1 deleted=1 dirty=false errors=0 elapsed_ms=42"
    );
  }

  #[test]
  fn formats_bench_stats() {
    assert_eq!(
      format_bench_stats(5, 2, 17),
      "candidates=5 results=2 elapsed_ms=17"
    );
  }

  #[test]
  fn formats_daemon_stats() {
    assert_eq!(
      format_daemon_stats(&DaemonRunStats {
        addr: "127.0.0.1:1234".to_string(),
        requests: 2,
        elapsed_ms: 100,
      }),
      "addr=127.0.0.1:1234 requests=2 elapsed_ms=100"
    );
  }
}
