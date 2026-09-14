//! Minimal argument parser for the local `kfs` CLI.
//!
//! The parser intentionally stays dependency-free while the Rust prototype is
//! being shaped. Keeping it small also makes provider and index command
//! behavior easy to test.

use std::path::PathBuf;

use kuntu_core::{SearchQuery, SearchRoot};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedArgs {
  pub roots: Vec<SearchRoot>,
  pub db_path: Option<PathBuf>,
  pub provider: SearchProvider,
  pub command: CommandSpec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchProvider {
  Spotlight,
  Sqlite,
  Auto,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandSpec {
  Search {
    query: SearchQuery,
    json: bool,
  },
  Explain {
    path: PathBuf,
    query: Option<SearchQuery>,
  },
  Bench {
    query: SearchQuery,
  },
  Index(IndexCommand),
  Watch {
    duration_ms: u64,
  },
  Daemon {
    addr: String,
    duration_ms: Option<u64>,
    allow_non_loopback: bool,
  },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexCommand {
  Rebuild,
  Refresh,
  Repair,
  Status,
}

pub fn parse_args(args: &[String]) -> Result<ParsedArgs, String> {
  if args.is_empty() {
    return Err(usage());
  }

  match args[0].as_str() {
    "search" => parse_search(&args[1..]),
    "explain" => parse_explain(&args[1..]),
    "bench" => parse_bench(&args[1..]),
    "index" => parse_index(&args[1..]),
    "watch" => parse_watch(&args[1..]),
    "daemon" => parse_daemon(&args[1..]),
    _ => Err(usage()),
  }
}

fn parse_search(args: &[String]) -> Result<ParsedArgs, String> {
  let (query_text, rest) = first_value(args, "search requires a query")?;
  let mut parsed = parse_common(rest)?;
  let query = parsed.query(query_text);
  let json = parsed.json;
  Ok(ParsedArgs {
    roots: parsed.take_roots()?,
    db_path: parsed.db_path,
    provider: parsed.provider,
    command: CommandSpec::Search { query, json },
  })
}

fn parse_explain(args: &[String]) -> Result<ParsedArgs, String> {
  let (path, rest) = first_value(args, "explain requires a path")?;
  let mut parsed = parse_common(rest)?;
  let query = parsed.query_text.take().map(SearchQuery::new);
  Ok(ParsedArgs {
    roots: parsed.take_roots()?,
    db_path: parsed.db_path,
    provider: parsed.provider,
    command: CommandSpec::Explain {
      path: PathBuf::from(path),
      query,
    },
  })
}

fn parse_bench(args: &[String]) -> Result<ParsedArgs, String> {
  let (query_text, rest) = first_value(args, "bench requires a query")?;
  let mut parsed = parse_common(rest)?;
  let query = parsed.query(query_text);
  Ok(ParsedArgs {
    roots: parsed.take_roots()?,
    db_path: parsed.db_path,
    provider: parsed.provider,
    command: CommandSpec::Bench { query },
  })
}

fn parse_index(args: &[String]) -> Result<ParsedArgs, String> {
  let Some(subcommand) = args.first() else {
    return Err("index requires rebuild or status".to_string());
  };
  let mut parsed = parse_common(&args[1..])?;
  let (roots, command) = match subcommand.as_str() {
    "rebuild" => {
      parsed.require_db()?;
      (parsed.take_roots()?, IndexCommand::Rebuild)
    }
    "refresh" => {
      parsed.require_db()?;
      (parsed.take_roots()?, IndexCommand::Refresh)
    }
    "repair" => {
      parsed.require_db()?;
      (parsed.take_roots()?, IndexCommand::Repair)
    }
    "status" => {
      parsed.require_db()?;
      (Vec::new(), IndexCommand::Status)
    }
    unknown => return Err(format!("unknown index command: {unknown}")),
  };
  Ok(ParsedArgs {
    roots,
    db_path: parsed.db_path,
    provider: parsed.provider,
    command: CommandSpec::Index(command),
  })
}

fn parse_watch(args: &[String]) -> Result<ParsedArgs, String> {
  let mut parsed = parse_common(args)?;
  parsed.require_db()?;
  let duration_ms = parsed.require_duration_ms()?;
  Ok(ParsedArgs {
    roots: parsed.take_roots()?,
    db_path: parsed.db_path,
    provider: parsed.provider,
    command: CommandSpec::Watch { duration_ms },
  })
}

fn parse_daemon(args: &[String]) -> Result<ParsedArgs, String> {
  let mut parsed = parse_common(args)?;
  parsed.require_db()?;
  let addr = parsed
    .addr
    .clone()
    .ok_or_else(|| "--addr is required for daemon".to_string())?;
  Ok(ParsedArgs {
    roots: parsed.take_roots()?,
    db_path: parsed.db_path,
    provider: parsed.provider,
    command: CommandSpec::Daemon {
      addr,
      duration_ms: parsed.duration_ms,
      allow_non_loopback: parsed.allow_non_loopback,
    },
  })
}

fn first_value<'a>(
  args: &'a [String],
  error: &'static str,
) -> Result<(&'a str, &'a [String]), String> {
  let Some(value) = args.first() else {
    return Err(error.to_string());
  };
  if value.starts_with("--") {
    return Err(error.to_string());
  }
  Ok((value.as_str(), &args[1..]))
}

#[derive(Debug)]
struct CommonArgs {
  roots: Vec<SearchRoot>,
  limit: Option<usize>,
  json: bool,
  include_hidden: bool,
  include_ignored: bool,
  extensions: Vec<String>,
  query_text: Option<String>,
  duration_ms: Option<u64>,
  addr: Option<String>,
  allow_non_loopback: bool,
  db_path: Option<PathBuf>,
  provider: SearchProvider,
}

impl Default for CommonArgs {
  fn default() -> Self {
    Self {
      roots: Vec::new(),
      limit: None,
      json: false,
      include_hidden: false,
      include_ignored: false,
      extensions: Vec::new(),
      query_text: None,
      duration_ms: None,
      addr: None,
      allow_non_loopback: false,
      db_path: None,
      provider: SearchProvider::Spotlight,
    }
  }
}

impl CommonArgs {
  fn take_roots(&mut self) -> Result<Vec<SearchRoot>, String> {
    if self.roots.is_empty() {
      return Err("at least one --root is required".to_string());
    }
    Ok(std::mem::take(&mut self.roots))
  }

  fn require_db(&self) -> Result<(), String> {
    if self.db_path.is_none() {
      return Err("--db is required for this command".to_string());
    }
    Ok(())
  }

  fn require_duration_ms(&self) -> Result<u64, String> {
    self
      .duration_ms
      .ok_or_else(|| "--duration-ms is required for watch".to_string())
  }

  fn query(&mut self, query_text: &str) -> SearchQuery {
    let mut query = SearchQuery::new(query_text);
    if let Some(limit) = self.limit {
      query.limit = limit;
    }
    query.include_hidden = self.include_hidden;
    query.include_ignored = self.include_ignored;
    query.extensions = std::mem::take(&mut self.extensions);
    query
  }
}

fn parse_common(args: &[String]) -> Result<CommonArgs, String> {
  let mut parsed = CommonArgs::default();
  let mut index = 0;
  while index < args.len() {
    match args[index].as_str() {
      "--root" => {
        index += 1;
        let value = args
          .get(index)
          .ok_or_else(|| "--root requires a path".to_string())?;
        parsed.roots.push(SearchRoot::new(value));
      }
      "--db" => {
        index += 1;
        let value = args
          .get(index)
          .ok_or_else(|| "--db requires a path".to_string())?;
        parsed.db_path = Some(PathBuf::from(value));
      }
      "--provider" => {
        index += 1;
        let value = args
          .get(index)
          .ok_or_else(|| "--provider requires sqlite, spotlight, or auto".to_string())?;
        parsed.provider = parse_provider(value)?;
      }
      "--limit" => {
        index += 1;
        let value = args
          .get(index)
          .ok_or_else(|| "--limit requires a number".to_string())?;
        parsed.limit = Some(
          value
            .parse::<usize>()
            .map_err(|_| "--limit must be a positive integer".to_string())?,
        );
      }
      "--duration-ms" => {
        index += 1;
        let value = args
          .get(index)
          .ok_or_else(|| "--duration-ms requires a number".to_string())?;
        parsed.duration_ms = Some(
          value
            .parse::<u64>()
            .map_err(|_| "--duration-ms must be a positive integer".to_string())?,
        );
      }
      "--addr" => {
        index += 1;
        let value = args
          .get(index)
          .ok_or_else(|| "--addr requires host:port".to_string())?;
        parsed.addr = Some(value.clone());
      }
      "--allow-non-loopback" => parsed.allow_non_loopback = true,
      "--json" => parsed.json = true,
      "--include-hidden" => parsed.include_hidden = true,
      "--include-ignored" => parsed.include_ignored = true,
      "--ext" => {
        index += 1;
        let value = args
          .get(index)
          .ok_or_else(|| "--ext requires an extension".to_string())?;
        parsed
          .extensions
          .push(value.trim_start_matches('.').to_ascii_lowercase());
      }
      "--query" => {
        index += 1;
        let value = args
          .get(index)
          .ok_or_else(|| "--query requires text".to_string())?;
        parsed.query_text = Some(value.clone());
      }
      unknown => return Err(format!("unknown argument: {unknown}")),
    }
    index += 1;
  }
  Ok(parsed)
}

fn parse_provider(value: &str) -> Result<SearchProvider, String> {
  match value {
    "spotlight" => Ok(SearchProvider::Spotlight),
    "sqlite" => Ok(SearchProvider::Sqlite),
    "auto" => Ok(SearchProvider::Auto),
    _ => Err("--provider must be sqlite, spotlight, or auto".to_string()),
  }
}

fn usage() -> String {
  "usage: kfs search <query> --root <path> [--provider sqlite|spotlight|auto] [--db path] [--limit n] [--json] | kfs index rebuild|refresh|repair --root <path> --db <path> | kfs index status --db <path> | kfs watch --root <path> --db <path> --duration-ms n | kfs daemon --root <path> --db <path> --addr host:port [--duration-ms n] [--allow-non-loopback]".to_string()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
  }

  #[test]
  fn search_requires_an_explicit_root() {
    let err = parse_args(&strings(&["search", "package json"])).unwrap_err();

    assert_eq!(err, "at least one --root is required");
  }

  #[test]
  fn parses_search_roots_limit_json_provider_db_and_extensions() {
    let parsed = parse_args(&strings(&[
      "search",
      "package json",
      "--root",
      "/Users/alice/Dev",
      "--limit",
      "7",
      "--json",
      "--ext",
      "json",
      "--provider",
      "sqlite",
      "--db",
      "/tmp/kfs.sqlite",
    ]))
    .unwrap();

    assert_eq!(parsed.roots, vec![SearchRoot::new("/Users/alice/Dev")]);
    assert_eq!(parsed.db_path, Some(PathBuf::from("/tmp/kfs.sqlite")));
    assert_eq!(parsed.provider, SearchProvider::Sqlite);
    assert_eq!(
      parsed.command,
      CommandSpec::Search {
        query: SearchQuery {
          query: "package json".to_string(),
          limit: 7,
          include_hidden: false,
          include_ignored: false,
          extensions: vec!["json".to_string()],
        },
        json: true,
      }
    );
  }

  #[test]
  fn parses_explain_with_optional_query() {
    let parsed = parse_args(&strings(&[
      "explain",
      "/Users/alice/Dev/project/package.json",
      "--root",
      "/Users/alice/Dev",
      "--query",
      "package json",
    ]))
    .unwrap();

    assert_eq!(
      parsed.command,
      CommandSpec::Explain {
        path: PathBuf::from("/Users/alice/Dev/project/package.json"),
        query: Some(SearchQuery::new("package json")),
      }
    );
  }

  #[test]
  fn parses_bench() {
    let parsed = parse_args(&strings(&[
      "bench",
      "main rs",
      "--root",
      "/Users/alice/Dev",
      "--limit",
      "3",
      "--provider",
      "auto",
    ]))
    .unwrap();

    assert_eq!(parsed.provider, SearchProvider::Auto);
    assert_eq!(
      parsed.command,
      CommandSpec::Bench {
        query: SearchQuery {
          query: "main rs".to_string(),
          limit: 3,
          include_hidden: false,
          include_ignored: false,
          extensions: Vec::new(),
        },
      }
    );
  }

  #[test]
  fn parses_index_rebuild() {
    let parsed = parse_args(&strings(&[
      "index",
      "rebuild",
      "--root",
      "/Users/alice/Dev",
      "--db",
      "/tmp/kfs.sqlite",
    ]))
    .unwrap();

    assert_eq!(parsed.command, CommandSpec::Index(IndexCommand::Rebuild));
    assert_eq!(parsed.roots, vec![SearchRoot::new("/Users/alice/Dev")]);
    assert_eq!(parsed.db_path, Some(PathBuf::from("/tmp/kfs.sqlite")));
  }

  #[test]
  fn parses_index_status() {
    let parsed = parse_args(&strings(&["index", "status", "--db", "/tmp/kfs.sqlite"])).unwrap();

    assert_eq!(parsed.command, CommandSpec::Index(IndexCommand::Status));
    assert_eq!(parsed.db_path, Some(PathBuf::from("/tmp/kfs.sqlite")));
  }

  #[test]
  fn parses_index_refresh() {
    let parsed = parse_args(&strings(&[
      "index",
      "refresh",
      "--root",
      "/Users/alice/Dev",
      "--db",
      "/tmp/kfs.sqlite",
    ]))
    .unwrap();

    assert_eq!(parsed.command, CommandSpec::Index(IndexCommand::Refresh));
    assert_eq!(parsed.roots, vec![SearchRoot::new("/Users/alice/Dev")]);
    assert_eq!(parsed.db_path, Some(PathBuf::from("/tmp/kfs.sqlite")));
  }

  #[test]
  fn parses_index_repair() {
    let parsed = parse_args(&strings(&[
      "index",
      "repair",
      "--root",
      "/Users/alice/Dev",
      "--db",
      "/tmp/kfs.sqlite",
    ]))
    .unwrap();

    assert_eq!(parsed.command, CommandSpec::Index(IndexCommand::Repair));
    assert_eq!(parsed.roots, vec![SearchRoot::new("/Users/alice/Dev")]);
    assert_eq!(parsed.db_path, Some(PathBuf::from("/tmp/kfs.sqlite")));
  }

  #[test]
  fn parses_watch_with_required_duration() {
    let parsed = parse_args(&strings(&[
      "watch",
      "--root",
      "/Users/alice/Dev",
      "--db",
      "/tmp/kfs.sqlite",
      "--duration-ms",
      "250",
    ]))
    .unwrap();

    assert_eq!(parsed.command, CommandSpec::Watch { duration_ms: 250 });
    assert_eq!(parsed.roots, vec![SearchRoot::new("/Users/alice/Dev")]);
  }

  #[test]
  fn watch_requires_duration_to_stay_bounded() {
    let err = parse_args(&strings(&[
      "watch",
      "--root",
      "/Users/alice/Dev",
      "--db",
      "/tmp/kfs.sqlite",
    ]))
    .unwrap_err();

    assert_eq!(err, "--duration-ms is required for watch");
  }

  #[test]
  fn parses_daemon_with_optional_duration() {
    let parsed = parse_args(&strings(&[
      "daemon",
      "--root",
      "/Users/alice/Dev",
      "--db",
      "/tmp/kfs.sqlite",
      "--addr",
      "127.0.0.1:0",
      "--duration-ms",
      "1000",
    ]))
    .unwrap();

    assert_eq!(
      parsed.command,
      CommandSpec::Daemon {
        addr: "127.0.0.1:0".to_string(),
        duration_ms: Some(1000),
        allow_non_loopback: false
      }
    );
    assert_eq!(parsed.roots, vec![SearchRoot::new("/Users/alice/Dev")]);
    assert_eq!(parsed.db_path, Some(PathBuf::from("/tmp/kfs.sqlite")));
  }

  #[test]
  fn parses_daemon_non_loopback_opt_in() {
    let parsed = parse_args(&strings(&[
      "daemon",
      "--root",
      "/Users/alice/Dev",
      "--db",
      "/tmp/kfs.sqlite",
      "--addr",
      "0.0.0.0:47865",
      "--allow-non-loopback",
    ]))
    .unwrap();

    assert_eq!(
      parsed.command,
      CommandSpec::Daemon {
        addr: "0.0.0.0:47865".to_string(),
        duration_ms: None,
        allow_non_loopback: true
      }
    );
  }
}
