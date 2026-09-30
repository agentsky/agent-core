//! [`Hooks`]: agentd's side of the runner's [`TurnHooks`].

use std::collections::BTreeMap;
use std::net::IpAddr;

use core_types::{CredentialKind, SessionId};
use cred_proxy::{EGRESS_ENV, PlaceholderId, Registry};
use runner::{HookError, ProcessEnv, Session, TurnHooks, TurnRequest};
use secrecy::{ExposeSecret as _, SecretString};

use crate::ctl::{Ctl, Outbox, ProcessInfo, ProcessToken, Turn};

/// The variable agentctl reads its token from.
pub const AGENTCTL_TOKEN_VAR: &str = "AGENTCTL_TOKEN";
/// The variable agentctl reads the API's address from.
pub const AGENTCTL_URL_VAR: &str = "AGENTCTL_URL";

/// Mints each `claude` process's placeholder and agentctl token, points
/// and clears them around each turn, and revokes them when the process
/// stops.
///
/// - [`process_starting`](TurnHooks::process_starting) mints a placeholder
///   of the process's credential kind with [`Registry::mint`], bound to the
///   session and the container's address, and issues the process's
///   agentctl token with [`Ctl::issue_process_token`]. The process's
///   environment gets the token, the agentctl API's address, and
///   [`EGRESS_ENV`], which sends its HTTPS through the egress proxy.
/// - [`turn_starting`](TurnHooks::turn_starting) points the placeholder at
///   the turn's credential with [`Registry::point`], then records the turn
///   on the token with [`Ctl::begin_turn`].
/// - [`turn_finished`](TurnHooks::turn_finished) clears the turn with
///   [`Ctl::end_turn`], which hands back the turn's outbox, and unpoints the
///   placeholder with [`Registry::unpoint`], whatever `end_turn` did. A
///   placeholder already revoked, as when the container died mid-turn, has
///   nothing left to clear.
/// - [`process_stopping`](TurnHooks::process_stopping) revokes the
///   placeholder, which closes the session's egress tunnels once it was
///   the session's last, and the token. Both name this process only, so a
///   late call for an old process leaves the session's new one alone, and
///   a second call does nothing.
#[derive(Debug, Clone)]
pub struct Hooks {
    registry: Registry,
    ctl: Ctl,
    agentctl_url: String,
    env: BTreeMap<String, String>,
}

impl Hooks {
    /// Hooks over `registry`, the one the credential proxy checks, and
    /// `ctl`, the agentctl API, whose address processes get as
    /// `agentctl_url`. `env` is added to every process's environment after
    /// [`EGRESS_ENV`], overriding it; it must hold no secret and none of
    /// the names the runner refuses.
    pub fn new(
        registry: Registry,
        ctl: Ctl,
        agentctl_url: impl Into<String>,
        env: BTreeMap<String, String>,
    ) -> Self {
        Self {
            registry,
            ctl,
            agentctl_url: agentctl_url.into(),
            env,
        }
    }
}

/// What a `claude` process was given: its placeholder's id and its
/// agentctl token.
#[derive(Debug)]
pub struct ProcessHandle {
    session: SessionId,
    placeholder: PlaceholderId,
    token: ProcessToken,
}

impl ProcessHandle {
    /// The process's session.
    pub fn session(&self) -> SessionId {
        self.session
    }

    /// The id of the process's placeholder.
    pub fn placeholder(&self) -> PlaceholderId {
        self.placeholder
    }
}

#[async_trait::async_trait]
impl TurnHooks for Hooks {
    type Process = ProcessHandle;
    type Finished = Option<Outbox>;

    async fn process_starting(
        &self,
        session: &Session,
        container_ip: IpAddr,
        kind: CredentialKind,
    ) -> Result<(ProcessEnv, ProcessHandle), HookError> {
        let placeholder = self.registry.mint(session.id, container_ip, kind)?;
        let issued = self
            .ctl
            .issue_process_token(ProcessInfo {
                session: session.id,
                agent: session.agent,
                volume: session.volume(),
                container_ip,
            })
            .await;
        let token = match issued {
            Ok(token) => token,
            Err(err) => {
                self.registry.revoke(placeholder.id());
                return Err(err.into());
            }
        };
        let mut env: BTreeMap<String, SecretString> = EGRESS_ENV
            .iter()
            .map(|(name, value)| ((*name).to_owned(), SecretString::from(*value)))
            .collect();
        env.insert(
            AGENTCTL_URL_VAR.to_owned(),
            SecretString::from(self.agentctl_url.as_str()),
        );
        for (name, value) in &self.env {
            env.insert(name.clone(), SecretString::from(value.as_str()));
        }
        env.insert(AGENTCTL_TOKEN_VAR.to_owned(), token.secret().clone());
        let process_env = ProcessEnv {
            placeholder: SecretString::from(placeholder.expose_secret()),
            env,
        };
        tracing::debug!(session = %session.id, %container_ip, ?kind, "minted a placeholder and an agentctl token");
        Ok((
            process_env,
            ProcessHandle {
                session: session.id,
                placeholder: placeholder.id(),
                token,
            },
        ))
    }

    async fn turn_starting(
        &self,
        session: &Session,
        process: &ProcessHandle,
        turn: &TurnRequest,
    ) -> Result<(), HookError> {
        self.registry.point(process.placeholder, turn.credential)?;
        self.ctl
            .begin_turn(
                &process.token,
                Turn {
                    id: turn.turn,
                    requester: turn.requester.clone(),
                    hop: turn.hop,
                    kind: turn.kind,
                    side: turn.side,
                    thread: session.thread.clone(),
                    trigger: turn.trigger.clone(),
                },
            )
            .await?;
        Ok(())
    }

    async fn turn_finished(
        &self,
        session: &Session,
        process: &ProcessHandle,
        turn: &TurnRequest,
    ) -> Result<Option<Outbox>, HookError> {
        let ended = self.ctl.end_turn(&process.token).await;
        if !self.registry.unpoint(process.placeholder) {
            tracing::debug!(session = %session.id, turn = %turn.turn, "the turn's placeholder was already revoked");
        }
        Ok(ended?)
    }

    async fn process_stopping(
        &self,
        session: &Session,
        process: &ProcessHandle,
    ) -> Result<(), HookError> {
        let revoked = self.registry.revoke(process.placeholder);
        self.ctl.revoke_process_token(&process.token).await?;
        tracing::debug!(session = %session.id, revoked, "revoked a process's placeholder and agentctl token");
        Ok(())
    }
}
