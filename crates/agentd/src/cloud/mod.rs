//! Cloud hand-off: starting a Claude Code cloud session on a member's own
//! account by firing one of their routines.
//!
//! [`FireClient`] sends the one request a hand-off makes: `POST
//! {base_url}/v1/claude_code/routines/<routine id>/fire` with the routine's
//! own token. It never retries, since the endpoint has no idempotency key
//! and every success starts a session. [`classify`] turns what came back
//! into a [`FireOutcome`]: the session was started, the endpoint refused
//! the request, or nobody can tell. agentd doesn't follow the session
//! afterwards; the member does, at the link.

mod fire;

pub use fire::{
    ANTHROPIC_VERSION, Answer, BodyError, Exchange, FireClient, FireError, FireOutcome,
    MAX_BODY_BYTES, MAX_TASK_BYTES, SESSION_URL_PREFIX, UnknownReason, classify,
};
