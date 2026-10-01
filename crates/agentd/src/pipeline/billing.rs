//! What a turn that failed on the credential it ran on says: in the
//! thread, and privately to its requester.
//!
//! A turn runs on its requester's own Claude account, or on the community
//! API key when the requester has none linked, never on anyone else's
//! (see [`router`]). So when the account or key hits its usage limit or is
//! refused, the thread is told whose it was, the requester's or the
//! community's, and the requester, who is the one who can do something
//! about it, is told in a direct message from the manager bot, at most
//! once per [`FAILURE_DM_INTERVAL`] for each kind of failure. The agent's
//! owner is never told, unless they are the requester.
//!
//! The exception is a private task, which always runs on the owner's own
//! account: its thread is told it was the owner's
//! ([`PRIVATE_USAGE_LIMIT_TEXT`], [`PRIVATE_LOGIN_TEXT`]), and nobody is
//! told privately.

use std::time::Duration;

use core_types::CredentialRef;
use runner::{ErrorKind, TurnOutcome};

/// How long after the manager bot told a requester about one kind of
/// failure, such as their own account's usage limit, it may tell them
/// about that kind again. Every failed turn still tells its thread.
pub const FAILURE_DM_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// What the thread is told when the requester's own Claude account has
/// reached its usage limit.
pub const USAGE_LIMIT_TEXT: &str = "Sorry, I can't answer that: the Claude account of the \
     person who asked has reached its usage limit. Try again when it resets.";
/// What the thread is told when the requester's own Claude login has
/// expired or was refused.
pub const LOGIN_EXPIRED_TEXT: &str = "Sorry, I can't answer that: the Claude login of the \
     person who asked has expired. They can run `/agent login` (`!agent login` on Rocket.Chat) \
     and ask again.";
/// What the thread is told when the community API key, which runs the
/// turns of members without a linked account, has reached its usage limit.
pub const COMMUNITY_USAGE_LIMIT_TEXT: &str = "Sorry, I can't answer that: the community API \
     key, which answers members without a linked Claude account, has reached its usage limit. \
     Link your own account with `/agent login` (`!agent login` on Rocket.Chat) to keep going.";
/// What the thread is told when the community API key was refused, or was
/// cleared while the turn ran.
pub const COMMUNITY_KEY_REFUSED_TEXT: &str = "Sorry, I can't answer that: the community API \
     key, which answers members without a linked Claude account, was refused. A community \
     admin can set a working one; meanwhile, link your own account with `/agent login` \
     (`!agent login` on Rocket.Chat).";

/// What the thread is told when a private task couldn't run because the
/// agent owner's Claude account, which it runs on, reached its usage limit.
pub const PRIVATE_USAGE_LIMIT_TEXT: &str = "Sorry, the private task couldn't run: the Claude \
     account of the agent's owner, which private tasks run on, has reached its usage limit.";
/// What the thread is told when a private task couldn't run because the
/// agent owner's Claude login expired, was refused, or isn't linked.
pub const PRIVATE_LOGIN_TEXT: &str = "Sorry, the private task couldn't run: the Claude login \
     of the agent's owner, which private tasks run on, has expired or isn't linked.";

/// A turn failure the credential it ran on caused, as the runner
/// classified it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CredentialFailure {
    /// The account or key reached its usage limit.
    UsageLimit,
    /// The account's login or the key was refused.
    Refused,
}

impl CredentialFailure {
    /// The credential failure `outcome` reports, if it reports one.
    pub(super) fn of(outcome: &TurnOutcome) -> Option<Self> {
        match outcome {
            TurnOutcome::Finished(result) if result.is_error => match result.error_kind {
                Some(ErrorKind::UsageLimit) => Some(Self::UsageLimit),
                Some(ErrorKind::Auth) => Some(Self::Refused),
                _ => None,
            },
            _ => None,
        }
    }

    /// What the thread is told about a turn that ran on `credential`.
    pub(super) fn thread_text(self, credential: CredentialRef) -> &'static str {
        match (self, credential) {
            (Self::UsageLimit, CredentialRef::Member(_)) => USAGE_LIMIT_TEXT,
            (Self::Refused, CredentialRef::Member(_)) => LOGIN_EXPIRED_TEXT,
            (Self::UsageLimit, CredentialRef::Community) => COMMUNITY_USAGE_LIMIT_TEXT,
            (Self::Refused, CredentialRef::Community) => COMMUNITY_KEY_REFUSED_TEXT,
        }
    }

    /// What the thread is told about a private task that failed on the
    /// agent owner's account, which private tasks always run on.
    pub(super) fn private_task_text(self) -> &'static str {
        match self {
            Self::UsageLimit => PRIVATE_USAGE_LIMIT_TEXT,
            Self::Refused => PRIVATE_LOGIN_TEXT,
        }
    }

    /// The kind of failure, and whose credential it was, that
    /// [`FAILURE_DM_INTERVAL`] counts separately.
    pub(super) fn notice_kind(self, credential: CredentialRef) -> &'static str {
        match (self, credential) {
            (Self::UsageLimit, CredentialRef::Member(_)) => "usage_limit/member",
            (Self::Refused, CredentialRef::Member(_)) => "refused/member",
            (Self::UsageLimit, CredentialRef::Community) => "usage_limit/community",
            (Self::Refused, CredentialRef::Community) => "refused/community",
        }
    }

    /// What the requester is told privately about `agent`'s turn that ran
    /// on `credential`. It is sent by the manager bot, in their DM with it.
    pub(super) fn requester_text(self, credential: CredentialRef, agent: &str) -> String {
        let why = match (self, credential) {
            (Self::UsageLimit, CredentialRef::Member(_)) => {
                "your Claude account has reached its usage limit. Ask again when it resets."
            }
            (Self::Refused, CredentialRef::Member(_)) => {
                "your Claude login has expired or was refused. Send `login` to me here to link \
                 your account again."
            }
            (Self::UsageLimit, CredentialRef::Community) => {
                "it ran on the community API key, which has reached its usage limit. Send \
                 `login` to me here to link your own account."
            }
            (Self::Refused, CredentialRef::Community) => {
                "it ran on the community API key, which was refused. Send `login` to me here to \
                 link your own account."
            }
        };
        format!("{agent} couldn't answer your request: {why}")
    }
}

#[cfg(test)]
mod tests {
    use core_types::MemberId;
    use runner::{TurnResult, TurnStats};

    use super::*;

    fn result(is_error: bool, error_kind: Option<ErrorKind>) -> TurnOutcome {
        TurnOutcome::Finished(TurnResult {
            is_error,
            error_kind,
            subtype: None,
            result: None,
            terminal_reason: None,
            api_error_status: None,
            usage: None,
            cost_usd: None,
            process_total_cost_usd: None,
            session_id: None,
            stats: TurnStats::default(),
        })
    }

    fn failed(kind: Option<ErrorKind>) -> TurnOutcome {
        result(true, kind)
    }

    #[test]
    fn only_usage_limits_and_refusals_are_the_credentials_fault() {
        assert_eq!(
            CredentialFailure::of(&failed(Some(ErrorKind::UsageLimit))),
            Some(CredentialFailure::UsageLimit)
        );
        assert_eq!(
            CredentialFailure::of(&failed(Some(ErrorKind::Auth))),
            Some(CredentialFailure::Refused)
        );
        assert_eq!(CredentialFailure::of(&failed(Some(ErrorKind::Other))), None);
        assert_eq!(CredentialFailure::of(&failed(None)), None);
        let succeeded = result(false, Some(ErrorKind::UsageLimit));
        assert_eq!(CredentialFailure::of(&succeeded), None);
    }

    #[test]
    fn the_texts_name_whose_account_it_was_and_never_the_owner() {
        let member = CredentialRef::Member(MemberId::new_v4());
        for failure in [CredentialFailure::UsageLimit, CredentialFailure::Refused] {
            let own = failure.thread_text(member);
            let community = failure.thread_text(CredentialRef::Community);
            assert!(own.contains("of the person who asked"), "{own}");
            assert!(community.contains("community API key"), "{community}");
            let own = failure.requester_text(member, "helper");
            let community = failure.requester_text(CredentialRef::Community, "helper");
            assert!(own.starts_with("helper couldn't answer your request: your Claude"));
            assert!(community.contains("community API key"), "{community}");
            for text in [
                own.as_str(),
                community.as_str(),
                failure.thread_text(member),
                failure.thread_text(CredentialRef::Community),
            ] {
                assert!(!text.contains("owner"), "{text}");
                assert!(!text.contains("no Claude account linked"), "{text}");
            }
        }
    }

    #[test]
    fn a_private_tasks_texts_name_the_owners_account() {
        assert_eq!(
            CredentialFailure::UsageLimit.private_task_text(),
            PRIVATE_USAGE_LIMIT_TEXT
        );
        assert_eq!(
            CredentialFailure::Refused.private_task_text(),
            PRIVATE_LOGIN_TEXT
        );
        for text in [PRIVATE_USAGE_LIMIT_TEXT, PRIVATE_LOGIN_TEXT] {
            assert!(text.contains("of the agent's owner"), "{text}");
        }
    }

    #[test]
    fn each_failure_and_credential_kind_is_rate_limited_on_its_own() {
        let member = CredentialRef::Member(MemberId::new_v4());
        let kinds: std::collections::HashSet<_> =
            [CredentialFailure::UsageLimit, CredentialFailure::Refused]
                .into_iter()
                .flat_map(|failure| {
                    [member, CredentialRef::Community]
                        .map(|credential| failure.notice_kind(credential))
                })
                .collect();
        assert_eq!(kinds.len(), 4);
    }
}
