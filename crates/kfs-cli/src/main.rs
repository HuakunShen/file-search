//! Command-line adapter for the independent Rust file search prototype.
//!
//! The CLI exists for local testing and future integration experiments. It
//! requires explicit roots and delegates filtering/ranking to `kfs-core`.

mod args;
mod output;

use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use args::{parse_args, CommandSpec, IndexCommand, SearchProvider};
use kfs_core::{
  CandidateProvider, MetadataIndex, SearchConfig, SearchEngineCore, SearchQuery, SearchResult,
};
use kfs_daemon::{serve, DaemonConfig};
use kfs_index::KfsIndex;
use kfs_provider_spotlight::SpotlightProvider;
use kfs_watcher::{run_polling_watch, WatchOptions};
use output::{
  format_bench_stats, format_daemon_stats, format_explain_text, format_rebuild_stats,
  format_refresh_stats, format_repair_stats, format_results_json, format_results_text,
  format_status, format_watch_stats,
};

#[derive(Debug, Clone)]
struct SearchRun {
  results: Vec<SearchResult>,
  candidate_count: usize,
}

fn main() -> ExitCode {
  match run(std::env::args().skip(1).collect()) {
    Ok(text) => {
      println!("{text}");
      ExitCode::SUCCESS
    }
    Err(err) => {
      eprintln!("kfs: {err}");
      ExitCode::from(2)
    }
  }
}

fn run(raw_args: Vec<String>) -> Result<String, String> {
  let spec = parse_args(&raw_args)?;
  let config = SearchConfig { roots: spec.roots };
  let core = SearchEngineCore::new(config.clone());

  match spec.command {
    CommandSpec::Search { query, json } => {
      let search = search_with_provider(
        &config,
        &core,
        &query,
        spec.provider,
        spec.db_path.as_deref(),
      )?;
      if json {
        Ok(format_results_json(&search.results))
      } else {
        Ok(format_results_text(&search.results))
      }
    }
    CommandSpec::Explain { path, query } => {
      let explain = core.explain_path(&path, query.as_ref());
      Ok(format_explain_text(&explain))
    }
    CommandSpec::Bench { query } => {
      let started = Instant::now();
      let search = search_with_provider(
        &config,
        &core,
        &query,
        spec.provider,
        spec.db_path.as_deref(),
      )?;
      Ok(format_bench_stats(
        search.candidate_count,
        search.results.len(),
        started.elapsed().as_millis(),
      ))
    }
    CommandSpec::Index(command) => run_index_command(&config, spec.db_path.as_deref(), command),
    CommandSpec::Watch { duration_ms } => {
      run_watch_command(&config, spec.db_path.as_deref(), duration_ms)
    }
    CommandSpec::Daemon {
      addr,
      duration_ms,
      allow_non_loopback,
    } => run_daemon_command(
      &config,
      spec.db_path.as_deref(),
      addr,
      duration_ms,
      allow_non_loopback,
    ),
  }
}

fn search_with_provider(
  config: &SearchConfig,
  core: &SearchEngineCore,
  query: &SearchQuery,
  provider: SearchProvider,
  db_path: Option<&Path>,
) -> Result<SearchRun, String> {
  match provider {
    SearchProvider::Spotlight => search_spotlight(config, core, query),
    SearchProvider::Sqlite => {
      let path = db_path.ok_or_else(|| "--db is required for sqlite provider".to_string())?;
      search_sqlite(config, query, path)
    }
    SearchProvider::Auto => {
      if let Some(path) = db_path {
        let search = search_sqlite(config, query, path)?;
        if !search.results.is_empty() {
          return Ok(search);
        }
      }
      search_spotlight(config, core, query)
    }
  }
}

fn search_spotlight(
  config: &SearchConfig,
  core: &SearchEngineCore,
  query: &SearchQuery,
) -> Result<SearchRun, String> {
  let provider = SpotlightProvider::default();
  let candidates = provider
    .search_candidates(config, query)
    .map_err(|err| err.to_string())?;
  let candidate_count = candidates.len();
  Ok(SearchRun {
    results: core.search_candidates(query, candidates),
    candidate_count,
  })
}

fn search_sqlite(
  config: &SearchConfig,
  query: &SearchQuery,
  db_path: &Path,
) -> Result<SearchRun, String> {
  let index = KfsIndex::open(db_path).map_err(|err| err.to_string())?;
  let outcome = index
    .search_with_metrics(config, query)
    .map_err(|err| err.to_string())?;
  Ok(SearchRun {
    results: outcome.results,
    candidate_count: outcome.candidate_count,
  })
}

fn run_index_command(
  config: &SearchConfig,
  db_path: Option<&Path>,
  command: IndexCommand,
) -> Result<String, String> {
  let path = db_path.ok_or_else(|| "--db is required for index commands".to_string())?;
  let mut index = KfsIndex::open(path).map_err(|err| err.to_string())?;
  match command {
    IndexCommand::Rebuild => {
      let stats = index.rebuild_index(config).map_err(|err| err.to_string())?;
      Ok(format_rebuild_stats(&stats))
    }
    IndexCommand::Refresh => {
      let stats = index.refresh_index(config).map_err(|err| err.to_string())?;
      Ok(format_refresh_stats(&stats))
    }
    IndexCommand::Repair => {
      let stats = index.repair_index(config).map_err(|err| err.to_string())?;
      Ok(format_repair_stats(&stats))
    }
    IndexCommand::Status => {
      let status = index.status().map_err(|err| err.to_string())?;
      Ok(format_status(&status))
    }
  }
}

fn run_watch_command(
  config: &SearchConfig,
  db_path: Option<&Path>,
  duration_ms: u64,
) -> Result<String, String> {
  let path = db_path.ok_or_else(|| "--db is required for watch".to_string())?;
  let mut index = KfsIndex::open(path).map_err(|err| err.to_string())?;
  let options = WatchOptions::new(config.clone())
    .with_duration(Duration::from_millis(duration_ms))
    .with_poll_interval(Duration::from_millis(100));
  let stats = run_polling_watch(&options, |_event| index.refresh_index(config).map(|_| ()))
    .map_err(|err| err.to_string())?;
  if stats.dirty {
    for root in config.roots.iter().filter(|root| root.enabled) {
      index.mark_root_dirty(root).map_err(|err| err.to_string())?;
    }
  }
  Ok(format_watch_stats(&stats))
}

fn run_daemon_command(
  config: &SearchConfig,
  db_path: Option<&Path>,
  addr: String,
  duration_ms: Option<u64>,
  allow_non_loopback: bool,
) -> Result<String, String> {
  let path = db_path.ok_or_else(|| "--db is required for daemon".to_string())?;
  let stats = serve(DaemonConfig {
    db_path: path.to_path_buf(),
    addr,
    duration: duration_ms.map(Duration::from_millis),
    max_requests: None,
    allowed_roots: config.roots.clone(),
    allow_non_loopback,
  })
  .map_err(|err| err.to_string())?;
  Ok(format_daemon_stats(&stats))
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::path::PathBuf;
  use std::time::{SystemTime, UNIX_EPOCH};

  use super::*;

  #[test]
  fn bench_reports_candidate_and_result_counts_for_sqlite() {
    let root = temp_dir("bench-root");
    let db_path = temp_dir("bench-db").with_extension("sqlite");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("needlebench-target.md"), "needle\n").unwrap();
    fs::write(root.join("other-target.md"), "other\n").unwrap();

    run(vec![
      "index".to_string(),
      "rebuild".to_string(),
      "--root".to_string(),
      root.to_string_lossy().into_owned(),
      "--db".to_string(),
      db_path.to_string_lossy().into_owned(),
    ])
    .unwrap();
    let output = run(vec![
      "bench".to_string(),
      "needlebench".to_string(),
      "--root".to_string(),
      root.to_string_lossy().into_owned(),
      "--provider".to_string(),
      "sqlite".to_string(),
      "--db".to_string(),
      db_path.to_string_lossy().into_owned(),
    ])
    .unwrap();

    cleanup(&root, &db_path);

    assert!(output.contains("candidates=1"));
    assert!(output.contains("results=1"));
    assert!(output.contains("elapsed_ms="));
  }

  fn temp_dir(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap()
      .as_nanos();
    std::env::temp_dir().join(format!("kfs-cli-{name}-{nonce}"))
  }

  fn cleanup(root: &Path, db_path: &Path) {
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(db_path.with_extension("sqlite-shm"));
    let _ = fs::remove_file(db_path.with_extension("sqlite-wal"));
  }
}
