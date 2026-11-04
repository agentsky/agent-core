//! [`Plan`] and [`PlanInfo`]: what the account profile says about the
//! member's subscription.

use serde::Deserialize;

/// A Claude subscription plan, from the profile's
/// `organization.organization_type`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Plan {
    /// `claude_pro`.
    Pro,
    /// `claude_max`.
    Max,
    /// `claude_team`.
    Team,
    /// `claude_enterprise`.
    Enterprise,
    /// Any other organization type, kept as the profile spelled it.
    Unknown(String),
}

impl Plan {
    /// The plan for an `organization.organization_type` value. Values this
    /// crate doesn't know become [`Plan::Unknown`] rather than an error.
    pub fn from_organization_type(value: &str) -> Self {
        match value {
            "claude_pro" => Self::Pro,
            "claude_max" => Self::Max,
            "claude_team" => Self::Team,
            "claude_enterprise" => Self::Enterprise,
            other => Self::Unknown(other.to_owned()),
        }
    }

    /// The `organization_type` spelling, which is also how the store keeps
    /// the plan, so `Plan::from_organization_type(plan.as_str()) == plan`.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Pro => "claude_pro",
            Self::Max => "claude_max",
            Self::Team => "claude_team",
            Self::Enterprise => "claude_enterprise",
            Self::Unknown(other) => other,
        }
    }
}

impl std::fmt::Display for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the profile says about the member's subscription.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanInfo {
    /// The plan, or `None` if the profile names no organization type (an
    /// account without a subscription).
    pub plan: Option<Plan>,
    /// `organization.rate_limit_tier`, if the profile has one.
    pub rate_limit_tier: Option<String>,
}

impl PlanInfo {
    /// Rebuilds the plan from the store's `plan` and `rate_limit_tier`
    /// columns.
    pub fn from_stored(plan: Option<&str>, rate_limit_tier: Option<&str>) -> Self {
        Self {
            plan: plan.map(Plan::from_organization_type),
            rate_limit_tier: rate_limit_tier.map(str::to_owned),
        }
    }

    pub(crate) fn stored_plan(&self) -> Option<String> {
        self.plan.as_ref().map(|plan| plan.as_str().to_owned())
    }
}

/// The parts of the profile response this crate reads. Every field is
/// optional and unknown fields are ignored, since the profile is Claude
/// Code's, not a published contract.
#[derive(Debug, Deserialize)]
pub(crate) struct ProfileResponse {
    #[serde(default)]
    organization: Option<Organization>,
}

#[derive(Debug, Deserialize)]
struct Organization {
    #[serde(default)]
    organization_type: Option<String>,
    #[serde(default)]
    rate_limit_tier: Option<String>,
}

impl From<ProfileResponse> for PlanInfo {
    fn from(profile: ProfileResponse) -> Self {
        let Some(organization) = profile.organization else {
            return Self::default();
        };
        Self {
            plan: organization
                .organization_type
                .filter(|value| !value.is_empty())
                .map(|value| Plan::from_organization_type(&value)),
            rate_limit_tier: organization.rate_limit_tier.filter(|tier| !tier.is_empty()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_organization_types_map_to_plans() {
        for (value, plan) in [
            ("claude_pro", Plan::Pro),
            ("claude_max", Plan::Max),
            ("claude_team", Plan::Team),
            ("claude_enterprise", Plan::Enterprise),
        ] {
            assert_eq!(Plan::from_organization_type(value), plan);
            assert_eq!(plan.as_str(), value);
            assert_eq!(plan.to_string(), value);
        }
    }

    #[test]
    fn unknown_organization_types_are_kept() {
        let plan = Plan::from_organization_type("claude_ultra");
        assert_eq!(plan, Plan::Unknown("claude_ultra".to_owned()));
        assert_eq!(plan.as_str(), "claude_ultra");
        assert_eq!(Plan::from_organization_type(plan.as_str()), plan);
    }

    fn parse(json: &str) -> PlanInfo {
        serde_json::from_str::<ProfileResponse>(json)
            .unwrap()
            .into()
    }

    #[test]
    fn the_profile_is_read_leniently() {
        assert_eq!(
            parse(
                r#"{"account":{"uuid":"a"},"organization":{"uuid":"o",
                "organization_type":"claude_max","rate_limit_tier":"default_claude_max_20x",
                "has_extra_usage_enabled":true}}"#
            ),
            PlanInfo {
                plan: Some(Plan::Max),
                rate_limit_tier: Some("default_claude_max_20x".to_owned()),
            }
        );
        assert_eq!(parse("{}"), PlanInfo::default());
        assert_eq!(parse(r#"{"organization":null}"#), PlanInfo::default());
        assert_eq!(
            parse(r#"{"organization":{"organization_type":null,"rate_limit_tier":""}}"#),
            PlanInfo::default()
        );
    }

    #[test]
    fn stored_values_round_trip() {
        let info = PlanInfo {
            plan: Some(Plan::Team),
            rate_limit_tier: Some("tier".to_owned()),
        };
        let stored = info.stored_plan();
        assert_eq!(stored.as_deref(), Some("claude_team"));
        assert_eq!(
            PlanInfo::from_stored(stored.as_deref(), info.rate_limit_tier.as_deref()),
            info
        );
        assert_eq!(PlanInfo::from_stored(None, None), PlanInfo::default());
    }
}
