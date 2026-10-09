//! Cloud hand-off: starting a Claude Code cloud session on a member's own
//! account by firing one of their routines.
//!
//! [`FireClient`] sends the one request a hand-off makes: `POST
//! {base_url}/v1/claude_code/routines/<routine id>/fire` with the routine's
//! own token. It never retries, since the endpoint has no idempotency key
//! and every success starts a session. What came back becomes a
//! [`FireOutcome`], the store's own [`store::CloudOutcome`]: the session
//! was started, the endpoint refused the request, or nobody can tell, which
//! the hand-off records as it is. agentd doesn't follow the session
//! afterwards; the member does, at the link.

mod fire;

pub use fire::{
    ANTHROPIC_VERSION, FireClient, FireClientError, FireOutcome, MAX_BODY_BYTES,
    MAX_RETRY_AFTER_SECS, MAX_TASK_BYTES, SESSION_URL_PREFIX, TaskError, check_task,
};
