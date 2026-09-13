//! macOS Spotlight provider for the Rust file search prototype.
//!
//! The provider only gathers candidate paths from `mdfind`; filtering,
//! sensitive-path denial, dedupe, and ranking remain in `kfs-core`.

use std::io;
use std::path::PathBuf;
use std::process::Command;

use kfs_core::{
  ranking::tokenize, BackendError, CandidateProvider, EntryKind, SearchCandidate, SearchConfig,
  SearchQuery,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
  pub stdout: String,
  pub stderr: String,
  pub status_success: bool,
}

pub trait CommandRunner {
  fn run(&self, program: &str, args: &[String]) -> io::Result<CommandOutput>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StdCommandRunner;

impl CommandRunner for StdCommandRunner {
  fn run(&self, program: &str, args: &[String]) -> io::Result<CommandOutput> {
    let output = Command::new(program).args(args).output()?;
    Ok(CommandOutput {
      stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
      stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
      status_success: output.status.success(),
    })
  }
}

#[derive(Debug, Clone)]
pub struct SpotlightProvider<R = StdCommandRunner> {
  runner: R,
}

impl Default for SpotlightProvider<StdCommandRunner> {
  fn default() -> Self {
    Self {
      runner: StdCommandRunner,
    }
  }
}

impl<R: CommandRunner> SpotlightProvider<R> {
  pub fn new(runner: R) -> Self {
    Self { runner }
  }

  pub fn search(
    &self,
    config: &SearchConfig,
    query: &SearchQuery,
  ) -> io::Result<Vec<SearchCandidate>> {
    #[cfg(not(target_os = "macos"))]
    {
      let _ = (config, query);
      return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Spotlight provider is only available on macOS",
      ));
    }

    #[cfg(target_os = "macos")]
    {
      let args = build_mdfind_args(config, query);
      let output = self.runner.run("mdfind", &args)?;
      if !output.status_success {
        return Err(io::Error::other(output.stderr));
      }
      Ok(
        parse_mdfind_output(&output.stdout)
          .into_iter()
          .map(|path| SearchCandidate {
            path,
            kind: EntryKind::Other,
            provider: "spotlight".to_string(),
          })
          .collect(),
      )
    }
  }
}

impl<R: CommandRunner> CandidateProvider for SpotlightProvider<R> {
  fn provider_name(&self) -> &'static str {
    "spotlight"
  }

  fn search_candidates(
    &self,
    config: &SearchConfig,
    query: &SearchQuery,
  ) -> Result<Vec<SearchCandidate>, BackendError> {
    self.search(config, query).map_err(BackendError::from)
  }
}

pub fn build_mdfind_args(config: &SearchConfig, query: &SearchQuery) -> Vec<String> {
  let mut args = Vec::new();
  for root in config.roots.iter().filter(|root| root.enabled) {
    args.push("-onlyin".to_string());
    args.push(root.path.to_string_lossy().into_owned());
  }
  args.push(build_spotlight_query(&query.query));
  args
}

pub fn build_spotlight_query(query: &str) -> String {
  let tokens = tokenize(query);
  if tokens.is_empty() {
    return "kMDItemFSName == \"*\"".to_string();
  }

  tokens
    .iter()
    .map(|token| {
      let escaped = escape_mdfind_value(token);
      format!("(kMDItemFSName == \"*{escaped}*\"cdw || kMDItemPath == \"*{escaped}*\"cdw)")
    })
    .collect::<Vec<_>>()
    .join(" && ")
}

fn escape_mdfind_value(value: &str) -> String {
  let mut escaped = String::new();
  for ch in value.chars() {
    match ch {
      '\\' => escaped.push_str("\\\\"),
      '"' => escaped.push_str("\\\""),
      _ => escaped.push(ch),
    }
  }
  escaped
}

pub fn parse_mdfind_output(output: &str) -> Vec<PathBuf> {
  output
    .lines()
    .map(str::trim)
    .filter(|line| !line.is_empty())
    .map(PathBuf::from)
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;
  use kfs_core::{SearchConfig, SearchQuery, SearchRoot};

  #[test]
  fn build_mdfind_args_scopes_each_enabled_root_and_uses_filename_query() {
    let config = SearchConfig {
      roots: vec![
        SearchRoot::new("/Users/alice/Dev").with_priority(10),
        SearchRoot::new("/Users/alice/Downloads").disabled(),
      ],
    };
    let query = SearchQuery::new("package json");

    let args = build_mdfind_args(&config, &query);

    assert_eq!(
            args,
            vec![
                "-onlyin",
                "/Users/alice/Dev",
                "(kMDItemFSName == \"*package*\"cdw || kMDItemPath == \"*package*\"cdw) && (kMDItemFSName == \"*json*\"cdw || kMDItemPath == \"*json*\"cdw)",
            ]
        );
  }

  #[test]
  fn build_spotlight_query_escapes_backslashes_and_quotes() {
    let query = build_spotlight_query(r#"my "file"\name"#);

    assert_eq!(
            query,
            "(kMDItemFSName == \"*my*\"cdw || kMDItemPath == \"*my*\"cdw) && (kMDItemFSName == \"*file*\"cdw || kMDItemPath == \"*file*\"cdw) && (kMDItemFSName == \"*name*\"cdw || kMDItemPath == \"*name*\"cdw)"
        );
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn trait_search_maps_runner_output_to_spotlight_candidates() {
    #[derive(Debug, Clone, Copy)]
    struct FakeRunner;

    impl CommandRunner for FakeRunner {
      fn run(&self, program: &str, args: &[String]) -> io::Result<CommandOutput> {
        assert_eq!(program, "mdfind");
        assert_eq!(args[0], "-onlyin");
        Ok(CommandOutput {
          stdout: "/Users/alice/Dev/package.json\n".to_string(),
          stderr: String::new(),
          status_success: true,
        })
      }
    }

    let provider = SpotlightProvider::new(FakeRunner);
    let config = SearchConfig {
      roots: vec![SearchRoot::new("/Users/alice/Dev")],
    };
    let candidates = provider
      .search_candidates(&config, &SearchQuery::new("package"))
      .unwrap();

    assert_eq!(candidates.len(), 1);
    assert_eq!(
      candidates[0].path,
      PathBuf::from("/Users/alice/Dev/package.json")
    );
    assert_eq!(candidates[0].provider, "spotlight");
  }

  #[test]
  fn parse_mdfind_output_ignores_blank_lines() {
    let paths = parse_mdfind_output("/Users/alice/Dev/a\n\n /Users/alice/Dev/b \n");

    assert_eq!(
      paths,
      vec![
        PathBuf::from("/Users/alice/Dev/a"),
        PathBuf::from("/Users/alice/Dev/b"),
      ]
    );
  }

  #[cfg(not(target_os = "macos"))]
  #[test]
  fn search_candidates_returns_unsupported_off_macos() {
    let provider = SpotlightProvider::default();
    let config = SearchConfig {
      roots: vec![SearchRoot::new("/tmp/anything")],
    };

    let error = provider
      .search_candidates(&config, &SearchQuery::new("package"))
      .unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
  }
}
