//! [`ModelPolicy`]: which model a turn uses, from the requester's plan.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Maps a Claude plan to a model, with a default for every other plan.
///
/// The plan is the string the profile lookup stores for a linked member
/// (`claude_links.plan`). It is matched exactly. A turn on the community key,
/// or on a member whose plan is unknown, gets the default.
///
/// Configuration deserializes into it:
///
/// ```
/// let policy: router::ModelPolicy = toml::from_str(
///     r#"
///     default = "claude-sonnet"
///     plans = { max = "claude-opus" }
///     "#,
/// )
/// .unwrap();
/// assert_eq!(policy.model_for(Some("max")), "claude-opus");
/// assert_eq!(policy.model_for(Some("pro")), "claude-sonnet");
/// assert_eq!(policy.model_for(None), "claude-sonnet");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPolicy {
    default: String,
    #[serde(default)]
    plans: BTreeMap<String, String>,
}

impl ModelPolicy {
    /// A policy that gives every plan `default`.
    pub fn new(default: impl Into<String>) -> Self {
        Self {
            default: default.into(),
            plans: BTreeMap::new(),
        }
    }

    /// The same policy, with `plan` mapped to `model`.
    pub fn with_plan(mut self, plan: impl Into<String>, model: impl Into<String>) -> Self {
        self.plans.insert(plan.into(), model.into());
        self
    }

    /// The model for a requester on `plan`, or the default when `plan` is
    /// `None` or not mapped.
    pub fn model_for(&self, plan: Option<&str>) -> &str {
        plan.and_then(|plan| self.plans.get(plan))
            .unwrap_or(&self.default)
    }

    /// The default model.
    pub fn default_model(&self) -> &str {
        &self.default
    }
}
