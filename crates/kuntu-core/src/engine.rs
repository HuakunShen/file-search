//! Provider-independent search orchestration.
//!
//! The engine consumes candidate paths from any provider, applies core policy,
//! filters and deduplicates results, then ranks them with the shared ranker.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::model::{
  lowercase_extension, EntryKind, ExplainResult, SearchCandidate, SearchConfig, SearchQuery,
  SearchResult,
};
use crate::policy::PathPolicy;
use crate::ranking::score_candidate;

#[derive(Debug, Clone)]
pub struct SearchEngineCore {
  config: SearchConfig,
  policy: PathPolicy,
}

impl SearchEngineCore {
  pub fn new(config: SearchConfig) -> Self {
    Self {
      policy: PathPolicy::new(config.clone()),
      config,
    }
  }

  pub fn search_candidates(
    &self,
    query: &SearchQuery,
    candidates: impl IntoIterator<Item = SearchCandidate>,
  ) -> Vec<SearchResult> {
    let mut by_path: HashMap<PathBuf, SearchResult> = HashMap::new();

    for candidate in candidates {
      let decision = self.policy.evaluate(&candidate.path, Some(query));
      if !decision.allowed || !extension_allowed(&candidate.path, query) {
        continue;
      }

      let (score, matches) = score_candidate(query, &candidate, decision.root_priority);
      if score <= 0 {
        continue;
      }

      let result = SearchResult {
        path: decision.path,
        score,
        provider: candidate.provider,
        matches,
      };

      by_path
        .entry(result.path.clone())
        .and_modify(|existing| {
          if result.score > existing.score {
            *existing = result.clone();
          }
        })
        .or_insert(result);
    }

    let mut results = by_path.into_values().collect::<Vec<_>>();
    results.sort_by(|left, right| {
      right
        .score
        .cmp(&left.score)
        .then_with(|| left.path.cmp(&right.path))
    });
    results.truncate(query.limit);
    results
  }

  pub fn explain_path(&self, path: &Path, query: Option<&SearchQuery>) -> ExplainResult {
    let decision = self.policy.evaluate(path, query);
    let (score, matches) = if decision.allowed {
      if let Some(query) = query {
        let candidate = SearchCandidate {
          path: decision.path.clone(),
          kind: EntryKind::Other,
          provider: "explain".to_string(),
        };
        let (score, matches) = score_candidate(query, &candidate, decision.root_priority);
        (Some(score), matches)
      } else {
        (None, Vec::new())
      }
    } else {
      (None, Vec::new())
    };

    ExplainResult {
      path: decision.path,
      allowed: decision.allowed,
      root: decision.root,
      hidden: decision.hidden,
      ignored: decision.ignored,
      sensitive: decision.sensitive,
      reasons: decision.reasons,
      score,
      matches,
    }
  }

  pub fn config(&self) -> &SearchConfig {
    &self.config
  }
}

fn extension_allowed(path: &Path, query: &SearchQuery) -> bool {
  query.extensions.is_empty()
    || lowercase_extension(path).is_some_and(|extension| {
      query.extensions.iter().any(|allowed| {
        allowed
          .trim_start_matches('.')
          .eq_ignore_ascii_case(&extension)
      })
    })
}

#[cfg(test)]
mod tests {
  use std::path::{Component, PathBuf};

  use crate::{EntryKind, MatchKind, SearchRoot};

  use super::*;

  /// Fixture roots start at the platform's volume root and use only
  /// controlled components: a real temporary directory can carry ambient
  /// names (`.ssh`, `credentials`, `target`, ...) that `is_sensitive` and
  /// `is_ignored` would legitimately classify, making the tests
  /// environment-dependent. The paths are never created; evaluation is
  /// pure logic.
  fn fixture_root(name: &str) -> PathBuf {
    let mut base = PathBuf::new();
    for component in std::env::temp_dir().components() {
      match component {
        Component::Prefix(_) | Component::RootDir => base.push(component.as_os_str()),
        _ => break,
      }
    }
    base.join(format!("kuntu-core-{name}-{}", std::process::id()))
  }

  fn engine() -> (SearchEngineCore, PathBuf) {
    let root = SearchRoot::new(fixture_root("engine")).with_priority(5);
    let configured = root.path.clone();
    (
      SearchEngineCore::new(SearchConfig { roots: vec![root] }),
      configured,
    )
  }

  fn candidate(path: PathBuf, provider: &str) -> SearchCandidate {
    SearchCandidate {
      path,
      kind: EntryKind::File,
      provider: provider.to_string(),
    }
  }

  #[test]
  fn filters_denied_candidates_and_sorts_by_score() {
    let query = SearchQuery::new("package json").with_limit(10);
    let (engine, root) = engine();
    let outside = fixture_root("engine-elsewhere");
    let results = engine.search_candidates(
      &query,
      vec![
        candidate(root.join("app").join("package.json"), "spotlight"),
        candidate(
          root
            .join("app")
            .join("node_modules")
            .join("pkg")
            .join("package.json"),
          "spotlight",
        ),
        candidate(outside.join("documents").join("package.json"), "spotlight"),
        candidate(
          root.join("app").join("src").join("package_json_notes.md"),
          "spotlight",
        ),
      ],
    );

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].path, root.join("app").join("package.json"));
    assert!(results[0].score > results[1].score);
  }

  #[test]
  fn dedupes_paths_and_keeps_highest_score() {
    let query = SearchQuery::new("readme");
    let (engine, root) = engine();
    let path = root.join("readme.md");
    let results = engine.search_candidates(
      &query,
      vec![candidate(path.clone(), "weak"), candidate(path, "strong")],
    );

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].provider, "weak");
  }

  #[test]
  fn extension_filter_limits_results() {
    let mut query = SearchQuery::new("readme");
    query.extensions = vec!["md".to_string()];
    let (engine, root) = engine();
    let results = engine.search_candidates(
      &query,
      vec![
        candidate(root.join("readme.md"), "spotlight"),
        candidate(root.join("readme.txt"), "spotlight"),
      ],
    );

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].path, root.join("readme.md"));
  }

  #[test]
  fn explain_reports_policy_and_score() {
    let query = SearchQuery::new("package json");
    let (engine, root) = engine();
    let explain = engine.explain_path(&root.join("app").join("package.json"), Some(&query));

    assert!(explain.allowed);
    assert_eq!(explain.root, Some(root.clone()));
    assert!(explain.score.is_some_and(|score| score > 0));
    assert!(explain.matches.contains(&MatchKind::BasenameToken));
  }

  #[test]
  fn explain_denies_sensitive_path_without_scoring() {
    let query = SearchQuery::new("env");
    let (engine, root) = engine();
    let explain = engine.explain_path(&root.join(".env"), Some(&query));

    assert!(!explain.allowed);
    assert!(explain.sensitive);
    assert_eq!(explain.score, None);
  }
}
