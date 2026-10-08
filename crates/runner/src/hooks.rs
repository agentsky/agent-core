//! [`TurnHooks`]: the runner's only way out, which agentd implements.

use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;

use core_types::{
    CredentialKind, CredentialRef, Hop, MessageId, Requester, Side, TurnId, TurnKind,
};
use secrecy::SecretString;
use store::Session;

/// An error a hook returns. The runner logs it and puts it in
/// [`RunnerError::Hook`](crate::RunnerError::Hook), so it must not hold a
/// secret.
pub type HookError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// What a new `claude` process gets from
/// [`TurnHooks::process_starting`]: the values the runner doesn't make.
///
/// `Debug` shows the environment's names, never its values.
pub struct ProcessEnv {
    /// The process's placeholder token, for
    /// [`LaunchSpec::placeholder`](crate::LaunchSpec::placeholder).
    pub placeholder: SecretString,
    /// The agentctl token and the egress proxy variables, for
    /// [`LaunchSpec::env`](crate::LaunchSpec::env), whose rules apply.
    pub env: BTreeMap<String, SecretString>,
}

impl fmt::Debug for ProcessEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessEnv")
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// One turn to run on a session, as the router and the turn pipeline
/// decided it.
///
/// `Debug` shows the message's length, never its text.
#[derive(Clone, PartialEq, Eq)]
pub struct TurnRequest {
    /// The turn.
    pub turn: TurnId,
    /// The turn's user message, fed to the CLI as one stream-json line.
    pub message: String,
    /// Whose credential the turn runs on. Its kind picks the process's
    /// credential variable: a turn on another kind than the warm process's
    /// restarts it.
    pub credential: CredentialRef,
    /// `--model`, if the router chose one. A turn on another model than
    /// the warm process's restarts it.
    pub model: Option<String>,
    /// Who caused the turn, and pays for it.
    pub requester: Requester,
    /// How many agent-to-agent hops led to it.
    pub hop: Hop,
    /// Which side of the agent it runs on. On the agent's `Private` volume
    /// it decides the mounts: the owner's side gets `shared/` read-write
    /// and `memory/`, the public side (a private task a non-owner asked
    /// for) `shared/` read-only and no `memory/`. Every other volume gets
    /// `shared/` read-write and no `memory/`.
    pub side: Side,
    /// A normal turn, or a private task. It must match the session: a
    /// private task's session runs only `TurnKind::PrivateTask` with its
    /// own consent, and a normal session only `TurnKind::Normal`.
    pub kind: TurnKind,
    /// The message that started the turn, if a message on the surface did.
    pub trigger: Option<MessageId>,
}

impl fmt::Debug for TurnRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnRequest")
            .field("turn", &self.turn)
            .field("message_len", &self.message.len())
            .field("credential", &self.credential)
            .field("model", &self.model)
            .field("requester", &self.requester)
            .field("hop", &self.hop)
            .field("side", &self.side)
            .field("kind", &self.kind)
            .field("trigger", &self.trigger)
            .finish()
    }
}

/// What the runner calls around processes and turns. agentd implements it
/// (T23): it mints placeholders and points them at credentials, and issues
/// agentctl tokens and records their turns. The runner doesn't know how.
///
/// For each process the runner calls, in order:
///
/// 1. [`process_starting`](Self::process_starting) once, before the
///    process starts.
/// 2. For each turn on it, [`turn_starting`](Self::turn_starting), then
///    [`turn_finished`](Self::turn_finished), which is called on every exit
///    from the turn once `turn_starting` was called: success, error,
///    timeout, crash, a failed or panicking `turn_starting`, and a caller
///    that stopped waiting. It returns before the session's next turn can
///    start.
/// 3. [`process_stopping`](Self::process_stopping), before the process or
///    its container is stopped, or once it is found gone. It may be called
///    again for the same process, from the container's death, so it must be
///    idempotent.
///
/// Every call names the process by the [`Process`](Self::Process) value
/// `process_starting` returned, so a late call for an old process of a
/// session never touches the session's new one.
///
/// Every call gets the session's row. Its id, agent, thread, scope and kind
/// never change; its `started` and timestamps are as of when the runner
/// read it.
#[async_trait::async_trait]
pub trait TurnHooks: Send + Sync + 'static {
    /// What [`process_starting`](Self::process_starting) made for one
    /// process, such as its placeholder's id and its agentctl token, which
    /// later calls for that process get back.
    type Process: Send + Sync + 'static;
    /// What [`turn_finished`](Self::turn_finished) returns, such as the
    /// turn's agentctl outbox. The runner hands it to the caller in
    /// [`TurnReport::finished`](crate::TurnReport::finished).
    type Finished: Send + 'static;

    /// A process for `session` is about to start in the container at
    /// `container_ip`, on credential kind `kind`. Returns its placeholder
    /// and environment, and the value later calls for this process get.
    ///
    /// # Errors
    ///
    /// Any failure. The process isn't started, the turn fails, and the
    /// session's container is stopped. A panic is handled the same way.
    async fn process_starting(
        &self,
        session: &Session,
        container_ip: IpAddr,
        kind: CredentialKind,
    ) -> Result<(ProcessEnv, Self::Process), HookError>;

    /// A turn is about to go to the process: point its placeholder at the
    /// turn's credential and record the turn on its agentctl token.
    ///
    /// # Errors
    ///
    /// Any failure. The turn isn't sent, and
    /// [`turn_finished`](Self::turn_finished) is still called.
    async fn turn_starting(
        &self,
        session: &Session,
        process: &Self::Process,
        turn: &TurnRequest,
    ) -> Result<(), HookError>;

    /// The turn has ended, however it ended: clear it from the agentctl
    /// token and unpoint the placeholder.
    ///
    /// # Errors
    ///
    /// Any failure. The runner can't tell what is still pointed, so it
    /// stops the process (calling
    /// [`process_stopping`](Self::process_stopping)), and its container too
    /// if that call fails or the process may still run. It does the same
    /// when this hook, `turn_starting` or the turn panicked, and the panic
    /// then fails the turn with
    /// [`RunnerError::TurnTask`](crate::RunnerError::TurnTask).
    async fn turn_finished(
        &self,
        session: &Session,
        process: &Self::Process,
        turn: &TurnRequest,
    ) -> Result<Self::Finished, HookError>;

    /// The process is about to be stopped, its container is, or it was
    /// found gone: revoke its placeholder and its agentctl token. Called
    /// again, harmlessly, when the container is reported dead.
    ///
    /// # Errors
    ///
    /// Any failure. It is logged, and the stop goes ahead and takes the
    /// process's container with it, so that once the container is stopped
    /// nothing left running there keeps what the hook failed to revoke. A
    /// container the sandbox fails to stop may stay running; it stays the
    /// session's, marked dead, until a later stop succeeds. Once the
    /// container is stopped the runner calls the hook once more, unless a
    /// call for the process has succeeded since. A second failure is logged
    /// and the runner never calls the hook for that process again, so what it
    /// failed to revoke stays live unless the implementation revokes it some
    /// other way, although the container's address may already belong to
    /// another container. A panic is handled the same way.
    async fn process_stopping(
        &self,
        session: &Session,
        process: &Self::Process,
    ) -> Result<(), HookError>;
}

#[cfg(test)]
mod tests {
    use core_types::{MemberKey, SurfaceKind};

    use super::*;

    #[test]
    fn debug_shows_no_secret_or_message_text() {
        let env = ProcessEnv {
            placeholder: SecretString::from("placeholder-secret"),
            env: BTreeMap::from([("AGENTCTL_TOKEN".into(), SecretString::from("ctl-secret"))]),
        };
        let debug = format!("{env:?}");
        assert!(debug.contains("AGENTCTL_TOKEN"), "{debug}");
        assert!(!debug.contains("secret"), "{debug}");

        let request = TurnRequest {
            turn: TurnId::new_v4(),
            message: "the message text".into(),
            credential: CredentialRef::Community,
            model: None,
            requester: Requester {
                member: None,
                key: MemberKey {
                    surface: SurfaceKind::Slack,
                    team: "T1".into(),
                    user: "U1".into(),
                },
            },
            hop: Hop::ZERO,
            side: Side::Public,
            kind: TurnKind::Normal,
            trigger: None,
        };
        let debug = format!("{request:?}");
        assert!(debug.contains("message_len: 16"), "{debug}");
        assert!(!debug.contains("message text"), "{debug}");
    }
}
