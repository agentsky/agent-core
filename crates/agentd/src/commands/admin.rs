//! The community admin commands: `admin api-key set`, `admin api-key
//! clear`, `admin ban` and `admin unban`.
//!
//! Only the community admins `[community] admins` lists may run them; anyone
//! else is told so, and a key they sent is dropped unstored. A key sent
//! where others can read it never gets here: [`Commands::run`] refuses it
//! first, for admins and everyone else alike. The key is a `SecretString`
//! from the parser on, is stored sealed, and is never logged or repeated in
//! a reply.
//!
//! A ban is of a member, so it covers every identity they linked, but not
//! an identity of theirs they never linked, which is another member to
//! agentd: agents refuse their requests, their own and the hops they
//! started, and the only commands they may run are those that take
//! something away from them: `me`, `logout`, and `pause` and `delete` of
//! their own agents. Their agents still answer others, on each requester's
//! own credential, as their owner left them. An admin can't be banned, and
//! a ban left on an admin's member (one made before they became an admin)
//! doesn't hold them back, so a ban can't lock the community out of
//! `admin unban`. Deleting the member deletes their ban.

use commands::{ApiKeyCommand, UserRef};
use core_types::MemberKey;
use secrecy::ExposeSecret as _;

use super::{Commands, Failure, now};
use crate::community::{MAX_API_KEY_BYTES, is_plausible_api_key};

/// The reply to an admin command from someone who isn't an admin.
pub(super) const NOT_AN_ADMIN: &str =
    "Only community admins can run `admin` commands. Ask one of them.";

/// The longest reason a ban may give, in characters.
pub(super) const MAX_BAN_REASON_CHARS: usize = 500;

/// How a reply names the member `user` names.
fn user_label(user: &UserRef) -> String {
    let name = match user {
        UserRef::Name(name) | UserRef::Id(name) => name,
    };
    format!("`@{}`", name.replace('`', ""))
}

impl Commands {
    /// Runs `admin api-key set` or `admin api-key clear` for `member`.
    pub(super) async fn api_key(
        &self,
        member: &MemberKey,
        command: ApiKeyCommand,
    ) -> Result<String, Failure> {
        if !self.is_admin(member) {
            tracing::info!(%member, "refused an admin command from someone who isn't an admin");
            return Ok(NOT_AN_ADMIN.to_owned());
        }
        let store = &self.inner.store;
        match command {
            ApiKeyCommand::Set { key } => {
                if !is_plausible_api_key(key.expose_secret()) {
                    return Ok(format!(
                        "That isn't an API key I can use: a key is 1 to {MAX_API_KEY_BYTES} \
                         letters, digits and punctuation, with no spaces. I didn't store it."
                    ));
                }
                store.set_community_api_key(&key, member, now()).await?;
                tracing::info!(admin = %member, "set the community API key");
                Ok(
                    "The community API key is set. Members without a linked Claude account \
                    now run agents on it; everyone else still runs on their own account, and \
                    an agent's owner only ever on theirs."
                        .to_owned(),
                )
            }
            ApiKeyCommand::Clear => {
                if store.clear_community_api_key(member, now()).await? {
                    tracing::info!(admin = %member, "cleared the community API key");
                    Ok(
                        "The community API key is cleared. Members without a linked Claude \
                        account are asked to link one again. You may want to revoke the key \
                        in the Anthropic Console too."
                            .to_owned(),
                    )
                } else {
                    Ok("No community API key was set.".to_owned())
                }
            }
        }
    }

    /// The line `me` shows a community admin about the community API key.
    pub(super) async fn community_key_status(&self) -> Result<String, Failure> {
        let status = self.inner.store.community_api_key_status().await?;
        let state = if status.set { "set" } else { "not set" };
        let changed = match (status.changed_by, status.changed_at) {
            (Some(by), Some(at)) => format!(
                " (last changed by `{}` on {})",
                by.to_string().replace('`', ""),
                at.date()
            ),
            _ => String::new(),
        };
        Ok(format!(
            "Community API key: {state}{changed}. You are a community admin."
        ))
    }
}

impl Commands {
    /// Runs `admin ban <user> [reason]` for `admin`.
    pub(super) async fn ban(
        &self,
        admin: &MemberKey,
        user: &UserRef,
        reason: Option<String>,
    ) -> Result<String, Failure> {
        if !self.is_admin(admin) {
            tracing::info!(member = %admin, "refused an admin command from someone who isn't an admin");
            return Ok(NOT_AN_ADMIN.to_owned());
        }
        let label = user_label(user);
        let Some(id) = self.resolve_user(admin, user).await? else {
            return Ok(format!("I don't know {label}."));
        };
        let target = MemberKey {
            user: id,
            ..admin.clone()
        };
        if self.is_admin(&target) {
            return Ok(format!(
                "{label} is a community admin, and admins can't be banned. Take them off \
                 `[community] admins` first."
            ));
        }
        if reason
            .as_deref()
            .is_some_and(|reason| reason.chars().count() > MAX_BAN_REASON_CHARS)
        {
            return Ok(format!(
                "A reason is at most {MAX_BAN_REASON_CHARS} characters. I didn't ban {label}."
            ));
        }
        let store = &self.inner.store;
        let member = store
            .ensure_member(&target, target.user.as_str(), now())
            .await?;
        if store
            .ban_member(member, admin, reason.as_deref(), now())
            .await?
        {
            tracing::info!(%admin, %member, "banned a member");
            Ok(format!(
                "Banned {label}. Agents refuse their requests, and they can only run `me`, \
                 `logout`, and `pause` or `delete` their agents. Undo it with `admin unban`."
            ))
        } else {
            Ok(format!("{label} is banned already."))
        }
    }

    /// Runs `admin unban <user>` for `admin`.
    pub(super) async fn unban(&self, admin: &MemberKey, user: &UserRef) -> Result<String, Failure> {
        if !self.is_admin(admin) {
            tracing::info!(member = %admin, "refused an admin command from someone who isn't an admin");
            return Ok(NOT_AN_ADMIN.to_owned());
        }
        let label = user_label(user);
        let Some(id) = self.resolve_user(admin, user).await? else {
            return Ok(format!("I don't know {label}."));
        };
        let target = MemberKey {
            user: id,
            ..admin.clone()
        };
        let store = &self.inner.store;
        let unbanned = match self.member(&target).await? {
            Some(member) => {
                let unbanned = store.unban_member(member).await?;
                if unbanned {
                    tracing::info!(%admin, %member, "unbanned a member");
                }
                unbanned
            }
            None => false,
        };
        Ok(if unbanned {
            format!("Unbanned {label}.")
        } else {
            format!("{label} isn't banned.")
        })
    }
}
