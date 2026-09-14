//! Root scoping, default ignore rules, and sensitive-path denial.
//!
//! Providers are allowed to be broad candidate generators. This policy is the
//! core gate that prevents hidden, generated, ignored, or sensitive paths from
//! leaking into ranked search results by default.

use std::path::{Component, Path};

use crate::model::{expand_tilde, lowercase_extension, PolicyDecision, SearchConfig, SearchQuery};

#[derive(Debug, Clone)]
pub struct PathPolicy {
  config: SearchConfig,
}

impl PathPolicy {
  pub fn new(config: SearchConfig) -> Self {
    Self { config }
  }

  pub fn evaluate(&self, path: &Path, query: Option<&SearchQuery>) -> PolicyDecision {
    let path = expand_tilde(path.to_path_buf());
    let root = self.matching_root(&path);
    let full_components = normal_components(&path);
    let relative_components = root
      .and_then(|root| path.strip_prefix(&root.path).ok())
      .map(normal_components)
      .unwrap_or_else(|| full_components.clone());
    let hidden = is_hidden(&relative_components);
    let ignored = is_ignored(&path, &relative_components);
    let sensitive = is_sensitive(&path, &full_components);
    let mut reasons = Vec::new();

    let mut allowed = true;
    let Some(root) = root else {
      allowed = false;
      reasons.push("outside-configured-roots".to_string());
      return PolicyDecision {
        path,
        allowed,
        root: None,
        root_priority: 0,
        hidden,
        ignored,
        sensitive,
        reasons,
      };
    };

    reasons.push("inside-root".to_string());
    if sensitive {
      allowed = false;
      reasons.push("sensitive-deny".to_string());
    }

    let include_hidden = query.is_some_and(|query| query.include_hidden) || root.include_hidden;
    if hidden && !include_hidden {
      allowed = false;
      reasons.push("hidden-deny".to_string());
    }

    let include_ignored = query.is_some_and(|query| query.include_ignored) || root.include_ignored;
    if ignored && !include_ignored {
      allowed = false;
      reasons.push("ignored-deny".to_string());
    }

    PolicyDecision {
      path,
      allowed,
      root: Some(root.path.clone()),
      root_priority: root.priority,
      hidden,
      ignored,
      sensitive,
      reasons,
    }
  }

  fn matching_root(&self, path: &Path) -> Option<&crate::model::SearchRoot> {
    self
      .config
      .roots
      .iter()
      .filter(|root| root.enabled && path.starts_with(&root.path))
      .max_by_key(|root| root.path.components().count())
  }
}

fn normal_components(path: &Path) -> Vec<String> {
  path
    .components()
    .filter_map(|component| match component {
      Component::Normal(value) => value.to_str().map(ToOwned::to_owned),
      _ => None,
    })
    .collect()
}

fn is_hidden(components: &[String]) -> bool {
  components
    .iter()
    .any(|component| component.starts_with('.') && component != "." && component != "..")
}

fn is_ignored(path: &Path, components: &[String]) -> bool {
  let ignored_components = [
    ".git",
    ".svn",
    ".hg",
    "node_modules",
    "target",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".next",
    ".nuxt",
    "dist",
    "build",
    ".turbo",
    ".cache",
    "vendor",
    "Pods",
    "DerivedData",
    ".gradle",
    "coverage",
  ];
  if components
    .iter()
    .any(|component| ignored_components.contains(&component.as_str()))
  {
    return true;
  }
  path
    .file_name()
    .and_then(|value| value.to_str())
    .is_some_and(|name| name == ".DS_Store")
}

fn is_sensitive(path: &Path, components: &[String]) -> bool {
  let sensitive_components = [
    ".ssh", ".gnupg", ".aws", ".azure", ".gcloud", ".kube", ".docker",
  ];
  if components.iter().any(|component| {
    let lower = component.to_ascii_lowercase();
    sensitive_components.contains(&component.as_str())
      || lower.contains("credentials")
      || lower.contains("secret")
  }) {
    return true;
  }

  if let Some(name) = path.file_name().and_then(|value| value.to_str()) {
    let lower = name.to_ascii_lowercase();
    if lower == ".env" || lower.starts_with(".env.") {
      return true;
    }
  }

  lowercase_extension(path)
    .is_some_and(|extension| matches!(extension.as_str(), "pem" | "key" | "p12" | "pfx"))
}

#[cfg(test)]
mod tests {
  use std::path::PathBuf;

  use crate::{SearchConfig, SearchQuery, SearchRoot};

  use super::*;

  /// Fixture roots live under the platform's own temporary directory so the
  /// drive/prefix shape matches what a real caller passes on every platform.
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
    base.join(format!("kfs-core-{name}-{}", std::process::id()))
  }

  fn policy() -> (PathPolicy, PathBuf) {
    let root = SearchRoot::new(fixture_root("policy")).with_priority(10);
    let configured = root.path.clone();
    (
      PathPolicy::new(SearchConfig { roots: vec![root] }),
      configured,
    )
  }

  #[test]
  fn allows_paths_inside_enabled_roots() {
    let (policy, root) = policy();
    let decision = policy.evaluate(&root.join("project").join("main.rs"), None);

    assert!(decision.allowed);
    assert_eq!(decision.root, Some(root.clone()));
    assert_eq!(decision.root_priority, 10);
    assert_eq!(decision.reasons, vec!["inside-root"]);
  }

  #[test]
  fn denies_paths_outside_configured_roots() {
    let (policy, _root) = policy();
    let outside = fixture_root("elsewhere");
    let decision = policy.evaluate(&outside.join("documents").join("report.pdf"), None);

    assert!(!decision.allowed);
    assert_eq!(decision.root, None);
    assert!(decision
      .reasons
      .contains(&"outside-configured-roots".to_string()));
  }

  #[test]
  fn denies_generated_directories_by_default() {
    let (policy, root) = policy();
    let decision = policy.evaluate(
      &root
        .join("app")
        .join("node_modules")
        .join("pkg")
        .join("index.js"),
      None,
    );

    assert!(!decision.allowed);
    assert!(decision.ignored);
    assert!(decision.reasons.contains(&"ignored-deny".to_string()));
  }

  #[test]
  fn denies_sensitive_paths_even_when_ignored_paths_are_allowed() {
    let (policy, root) = policy();
    let query = SearchQuery {
      include_ignored: true,
      include_hidden: true,
      ..SearchQuery::new("id_rsa")
    };
    let decision = policy.evaluate(&root.join(".ssh").join("id_rsa"), Some(&query));

    assert!(!decision.allowed);
    assert!(decision.sensitive);
    assert!(decision.reasons.contains(&"sensitive-deny".to_string()));
  }

  #[test]
  fn query_can_include_hidden_non_sensitive_paths() {
    let (policy, root) = policy();
    let query = SearchQuery {
      include_hidden: true,
      ..SearchQuery::new("config")
    };
    let decision = policy.evaluate(&root.join(".config").join("readme.md"), Some(&query));

    assert!(decision.allowed);
    assert!(decision.hidden);
  }

  #[test]
  fn root_can_include_ignored_paths() {
    let root = SearchRoot::new(fixture_root("policy")).include_ignored(true);
    let configured = root.path.clone();
    let policy = PathPolicy::new(SearchConfig { roots: vec![root] });
    let decision = policy.evaluate(
      &configured
        .join("project")
        .join("target")
        .join("debug")
        .join("app"),
      None,
    );

    assert!(decision.allowed);
    assert!(decision.ignored);
  }

  #[test]
  fn explicit_root_under_hidden_parent_does_not_hide_every_child() {
    let root = SearchRoot::new(
      fixture_root("hidden-parent")
        .join(".codex")
        .join("worktree")
        .join("project"),
    );
    let configured = root.path.clone();
    let policy = PathPolicy::new(SearchConfig { roots: vec![root] });

    let decision = policy.evaluate(&configured.join("src").join("main.rs"), None);

    assert!(decision.allowed);
    assert!(!decision.hidden);
  }

  #[test]
  fn hidden_directories_inside_explicit_root_are_still_hidden() {
    let root = SearchRoot::new(
      fixture_root("hidden-parent")
        .join(".codex")
        .join("worktree")
        .join("project"),
    );
    let configured = root.path.clone();
    let policy = PathPolicy::new(SearchConfig { roots: vec![root] });

    let decision = policy.evaluate(&configured.join(".cache").join("file"), None);

    assert!(!decision.allowed);
    assert!(decision.hidden);
  }
}
