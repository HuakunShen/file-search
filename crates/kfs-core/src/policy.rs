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

        let include_ignored =
            query.is_some_and(|query| query.include_ignored) || root.include_ignored;
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
        self.config
            .roots
            .iter()
            .filter(|root| root.enabled && path.starts_with(&root.path))
            .max_by_key(|root| root.path.components().count())
    }
}

fn normal_components(path: &Path) -> Vec<String> {
    path.components()
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
    path.file_name()
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
    use std::path::{Path, PathBuf};

    use crate::{SearchConfig, SearchQuery, SearchRoot};

    use super::*;

    fn policy() -> PathPolicy {
        PathPolicy::new(SearchConfig {
            roots: vec![SearchRoot::new("/Users/alice/Dev").with_priority(10)],
        })
    }

    #[test]
    fn allows_paths_inside_enabled_roots() {
        let decision = policy().evaluate(Path::new("/Users/alice/Dev/project/main.rs"), None);

        assert!(decision.allowed);
        assert_eq!(decision.root, Some(PathBuf::from("/Users/alice/Dev")));
        assert_eq!(decision.root_priority, 10);
        assert_eq!(decision.reasons, vec!["inside-root"]);
    }

    #[test]
    fn denies_paths_outside_configured_roots() {
        let decision = policy().evaluate(Path::new("/Users/alice/Documents/report.pdf"), None);

        assert!(!decision.allowed);
        assert_eq!(decision.root, None);
        assert!(decision
            .reasons
            .contains(&"outside-configured-roots".to_string()));
    }

    #[test]
    fn denies_generated_directories_by_default() {
        let decision = policy().evaluate(
            Path::new("/Users/alice/Dev/app/node_modules/pkg/index.js"),
            None,
        );

        assert!(!decision.allowed);
        assert!(decision.ignored);
        assert!(decision.reasons.contains(&"ignored-deny".to_string()));
    }

    #[test]
    fn denies_sensitive_paths_even_when_ignored_paths_are_allowed() {
        let query = SearchQuery {
            include_ignored: true,
            include_hidden: true,
            ..SearchQuery::new("id_rsa")
        };
        let decision = policy().evaluate(Path::new("/Users/alice/Dev/.ssh/id_rsa"), Some(&query));

        assert!(!decision.allowed);
        assert!(decision.sensitive);
        assert!(decision.reasons.contains(&"sensitive-deny".to_string()));
    }

    #[test]
    fn query_can_include_hidden_non_sensitive_paths() {
        let query = SearchQuery {
            include_hidden: true,
            ..SearchQuery::new("config")
        };
        let decision = policy().evaluate(
            Path::new("/Users/alice/Dev/.config/readme.md"),
            Some(&query),
        );

        assert!(decision.allowed);
        assert!(decision.hidden);
    }

    #[test]
    fn root_can_include_ignored_paths() {
        let policy = PathPolicy::new(SearchConfig {
            roots: vec![SearchRoot::new("/Users/alice/Dev").include_ignored(true)],
        });
        let decision =
            policy.evaluate(Path::new("/Users/alice/Dev/project/target/debug/app"), None);

        assert!(decision.allowed);
        assert!(decision.ignored);
    }

    #[test]
    fn explicit_root_under_hidden_parent_does_not_hide_every_child() {
        let policy = PathPolicy::new(SearchConfig {
            roots: vec![SearchRoot::new("/Users/alice/.codex/worktree/project")],
        });

        let decision = policy.evaluate(
            Path::new("/Users/alice/.codex/worktree/project/src/main.rs"),
            None,
        );

        assert!(decision.allowed);
        assert!(!decision.hidden);
    }

    #[test]
    fn hidden_directories_inside_explicit_root_are_still_hidden() {
        let policy = PathPolicy::new(SearchConfig {
            roots: vec![SearchRoot::new("/Users/alice/.codex/worktree/project")],
        });

        let decision = policy.evaluate(
            Path::new("/Users/alice/.codex/worktree/project/.cache/file"),
            None,
        );

        assert!(!decision.allowed);
        assert!(decision.hidden);
    }
}
