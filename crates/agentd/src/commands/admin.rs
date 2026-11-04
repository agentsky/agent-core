//! The community admin commands: `admin api-key set` and `admin api-key
//! clear`.
//!
//! Only the community admins `[community] admins` lists may run them; anyone
//! else is told so, and a key they sent is dropped unstored. A key sent
//! where others can read it never gets here: [`Commands::run`] refuses it
//! first, for admins and everyone else alike. The key is a `SecretString`
//! from the parser on, is stored sealed, and is never logged or repeated in
//! a reply.

use commands::ApiKeyCommand;
use core_types::MemberKey;
use secrecy::ExposeSecret as _;

use super::{Commands, Failure, now};
use crate::community::{MAX_API_KEY_BYTES, is_plausible_api_key};

/// The reply to an admin command from someone who isn't an admin.
pub(super) const NOT_AN_ADMIN: &str =
    "Only community admins can change the community API key. Ask one of them.";

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
