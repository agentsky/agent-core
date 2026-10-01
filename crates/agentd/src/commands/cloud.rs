//! `cloud add`, `cloud run`, `cloud list` and `cloud rm`: a member's
//! routines, and handing work to a Claude Code cloud session on the
//! member's own account by firing one of them ([`crate::cloud`]).
//!
//! # Who may run them, and where
//!
//! Every `cloud` command runs only where no one but the member and the
//! manager bot reads it ([`Origin::is_private`]): a Slack slash command, a
//! DM with the Slack manager app, or a DM with the Rocket.Chat manager bot.
//! `cloud add` sent anywhere else gets the secret-bearing refusal, which
//! says to revoke the token. `add` and `run` need a linked member and
//! `[cloud]` ([`Commands::with_cloud`]); `list` and `rm` work for any
//! member, and `rm` is the one `cloud` command a ban leaves. No `agentctl`
//! command or consent card starts a hand-off: only a member typing `cloud
//! run` does, and messages from bots are never commands.
//!
//! # A run
//!
//! `cloud run <routine> <task>` takes the task as the member saw it. On
//! Slack, whose text arrives with entities and with mentions, channels and
//! links as `<…>` tokens, each token becomes what Slack showed: `@name`, `#name`, a link's URL, or `label (url)` for a
//! link labelled otherwise, and any other token is refused. Then the
//! characters that only choose how their neighbours are drawn are dropped,
//! and a task with what wouldn't show as written (the checks a private
//! task's consent card makes), an empty one or one over
//! [`MAX_TASK_BYTES`] is refused. A routine
//! registered for another origin than `[cloud] base_url`'s is refused
//! before anything is written, and so its token goes nowhere else. The
//! hand-off is then written `sending`, unless the member asked for
//! `[cloud] handoffs_per_hour` within the last hour or the routine was
//! removed or replaced meanwhile, the routine fired once and the outcome
//! recorded, and the reply says what came of it: the session's
//! link, or the failure table's line. A link the store failed to record is
//! still in the reply.
//!
//! [`CloudNotifier`] tells a member about a hand-off whose answer was never
//! recorded.

mod notifier;

#[cfg(test)]
mod tests;

use commands::{CloudCommand, Command, RoutineLabel, RoutineUrl};
use core_types::{MemberId, MemberKey, RoutineToken};
use secrecy::ExposeSecret as _;
use store::{
    CloudBegun, CloudFinished, CloudHandoff, CloudHandoffState, CloudOrigin, CloudOutcome,
    CloudRoutinePut, NewCloudHandoff, NewCloudRoutine, StoreError,
};
use surface_slack::normalize::unescape;

use super::sessions::when;
use super::{Commands, FAILED, Failure, Origin, now};
use crate::cloud::{MAX_TASK_BYTES, SESSION_URL_PREFIX, check_task};
use crate::consents::unshowable;
use crate::ctl::without_joiners;

pub use notifier::{CLOUD_SWEEP_INTERVAL, CloudNotifier, CloudPass, stale_notice};

/// How many of a member's hand-offs `cloud list` shows, newest first.
pub const LISTED_HANDOFFS: u32 = 10;

/// How many characters of a task's first line `cloud list` shows.
pub const LISTED_TASK_CHARS: usize = 60;

/// Where a routine's API trigger, its token and the routines themselves
/// are managed.
const ROUTINES_PAGE: &str = "claude.ai/code/routines";

/// Why a Slack task was refused: Slack sent a part of it as a token that
/// can't be turned back into what the member saw.
const SLACK_TOKEN_REFUSED: &str = "Slack sent part of the task as something other than a \
                                   mention, a channel or a link, such as a broadcast, a user \
                                   group or a date, or as a mention without its name, which I \
                                   can't turn back into what you saw. Write it as plain text.";

impl Commands {
    /// The reply to `command` from `key`, sent from `origin`; `delivered`
    /// is the command's text as the surface delivered it.
    pub(super) async fn cloud(
        &self,
        key: &MemberKey,
        command: CloudCommand,
        origin: &Origin,
        delivered: &str,
    ) -> Result<String, Failure> {
        let Some(place) = cloud_origin(origin) else {
            let name = Command::Cloud(command).name();
            tracing::info!(member = %key, command = name, "refused a cloud command where others read it");
            return Ok(format!(
                "`{name}` runs only where no one else reads it, so I didn't run it. Send it {}.",
                origin.private_place()
            ));
        };
        match command {
            CloudCommand::Add {
                label,
                routine,
                token,
            } => self.cloud_add(key, &label, &routine, &token, origin).await,
            CloudCommand::Run { label, task } => {
                self.cloud_run(key, &label, task, origin, place, delivered)
                    .await
            }
            CloudCommand::List => self.cloud_list(key, origin).await,
            CloudCommand::Rm { label } => self.cloud_rm(key, &label).await,
        }
    }

    /// `key`'s member, if their Claude account is linked, or else the reply
    /// that asks them to link one.
    async fn cloud_member(
        &self,
        key: &MemberKey,
        origin: &Origin,
    ) -> Result<Result<MemberId, String>, Failure> {
        Ok(self.linked_owner(key, origin).await?.map_err(|_| {
            format!(
                "Link your Claude account first: send {}. Only a linked member can register \
                 or run routines.",
                origin.command("login")
            )
        }))
    }

    async fn cloud_add(
        &self,
        key: &MemberKey,
        label: &RoutineLabel,
        routine: &RoutineUrl,
        token: &RoutineToken,
        origin: &Origin,
    ) -> Result<String, Failure> {
        let Some(fire) = &self.cloud else {
            return Ok(cloud_off("I didn't store the routine"));
        };
        let member = match self.cloud_member(key, origin).await? {
            Ok(member) => member,
            Err(reply) => return Ok(reply),
        };
        if !fire.fires_for(&routine.origin().ascii_serialization()) {
            tracing::info!(%member, routine = routine.routine_id().as_str(), "refused a routine URL on another origin");
            return Ok(format!(
                "That URL isn't on the routine endpoint this agentd fires, `{}`, so I didn't \
                 store the routine. Copy the URL from the routine's API trigger at \
                 {ROUTINES_PAGE}.",
                fire.origin()
            ));
        }
        let label = label.as_str();
        let routine_id = routine.routine_id();
        let put = self
            .inner
            .store
            .put_cloud_routine(
                &NewCloudRoutine {
                    member,
                    label,
                    routine_id,
                    url_origin: fire.origin(),
                    token,
                    added_by: key,
                },
                now(),
            )
            .await?;
        tracing::info!(%member, routine = routine_id.as_str(), put = ?put, "registered a cloud routine");
        let run = origin.command(&format!("cloud run {label} <task>"));
        Ok(match put {
            CloudRoutinePut::Added(_) => format!(
                "Registered routine `{label}` (`{routine_id}`). Hand it work with {run}; it runs \
                 on the account the routine belongs to."
            ),
            CloudRoutinePut::Replaced(_) => format!(
                "Replaced routine `{label}`: it now fires `{routine_id}` with the token you just \
                 sent. Hand it work with {run}."
            ),
            CloudRoutinePut::RoutineTaken { label: taken } => format!(
                "You registered that routine as `{taken}` already, so I didn't store it again. \
                 To give it a new token, send {}.",
                add_command(origin, &format!("{taken} <url> <token>"))
            ),
            CloudRoutinePut::Full => format!(
                "You have {}, the most one member may hold, so I didn't store this one. Remove \
                 one with {} first.",
                routines_counted(u64::from(store::MAX_CLOUD_ROUTINES)),
                origin.command("cloud rm <routine>")
            ),
        })
    }

    async fn cloud_run(
        &self,
        key: &MemberKey,
        label: &RoutineLabel,
        task: String,
        origin: &Origin,
        place: CloudOrigin,
        delivered: &str,
    ) -> Result<String, Failure> {
        let Some(fire) = &self.cloud else {
            return Ok(cloud_off("I didn't start anything"));
        };
        let member = match self.cloud_member(key, origin).await? {
            Ok(member) => member,
            Err(reply) => return Ok(reply),
        };
        let task = if origin.is_slack() {
            let Some(delivered) = delivered_task(delivered) else {
                tracing::warn!(%member, "a Slack cloud run's text as delivered doesn't parse as one");
                return Ok(FAILED.to_owned());
            };
            match slack_task(&delivered) {
                Ok(task) => task,
                Err(why) => return Ok(why.to_owned()),
            }
        } else {
            task
        };
        let task = without_joiners(&task);
        if let Some(why) = unshowable(&task) {
            return Ok(format!(
                "I didn't start anything: {why}. Change that and run it again; pasted code may \
                 need reflowing."
            ));
        }
        if task.trim().is_empty() || check_task(&task).is_err() {
            return Ok(format!(
                "I didn't start anything: the task is empty or longer than {MAX_TASK_BYTES} \
                 bytes."
            ));
        }
        let routine = match self.inner.store.cloud_routine(member, label.as_str()).await {
            Ok(Some(routine)) => routine,
            Ok(None) => return Ok(no_routine(label, origin)),
            Err(
                err @ (StoreError::Corrupt {
                    table: "cloud_routines",
                    ..
                }
                | StoreError::Seal {
                    table: "cloud_routines",
                    ..
                }),
            ) => {
                tracing::warn!(%member, error = %err, "a stored cloud routine can't be used");
                return Ok(format!(
                    "Routine `{label}`'s stored token can't be used any more, so I didn't start \
                     anything. Register it again with {}.",
                    add_command(origin, &format!("{label} <url> <token>"))
                ));
            }
            Err(err) => return Err(err.into()),
        };
        if !fire.fires_for(&routine.url_origin) {
            tracing::info!(%member, routine = routine.routine_id.as_str(), "refused to fire a routine registered for another origin");
            return Ok(format!(
                "Routine `{label}` was registered for another routine endpoint than the one \
                 this agentd fires now (`[cloud] base_url` changed), so I sent its token nowhere \
                 and started nothing. Register it again with {}, with the URL and a token from \
                 its API trigger.",
                add_command(origin, &format!("{label} <url> <token>"))
            ));
        }
        let begun = self
            .inner
            .store
            .begin_cloud_handoff(
                &NewCloudHandoff {
                    member,
                    routine_label: label.as_str(),
                    routine_id: &routine.routine_id,
                    requested_by: key,
                    origin: place,
                    task: &task,
                },
                fire.handoffs_per_hour(),
                now(),
            )
            .await;
        let id = match begun {
            Ok(CloudBegun::Begun(id)) => id,
            Ok(CloudBegun::RoutineGone) => {
                tracing::info!(%member, routine = routine.routine_id.as_str(), "a cloud routine was removed or replaced before its hand-off; fired nothing");
                return Ok(format!(
                    "Routine `{label}` was removed or replaced while I was starting it, so \
                     nothing was started. {} shows your routines.",
                    origin.command("cloud list")
                ));
            }
            Ok(CloudBegun::TooMany) => {
                tracing::info!(%member, routine = routine.routine_id.as_str(), "refused a cloud hand-off past the hourly cap");
                return Ok(format!(
                    "Nothing was started: you've asked for {} in the last hour, the most I start \
                     for one member. Try again later.",
                    handoffs_counted(u64::from(fire.handoffs_per_hour()))
                ));
            }
            Err(err) => {
                tracing::warn!(%member, routine = routine.routine_id.as_str(), error = %err, "couldn't record a cloud hand-off; fired nothing");
                return Ok(
                    "Nothing was started: I couldn't record the hand-off. Try again in a minute."
                        .to_owned(),
                );
            }
        };
        let outcome = fire.fire(&routine, &task).await;
        let state = outcome.state().as_str();
        match self
            .inner
            .store
            .finish_cloud_handoff(id, &outcome, now())
            .await
        {
            Ok(CloudFinished::Recorded) => {
                tracing::info!(%member, handoff = %id, routine = routine.routine_id.as_str(), state, "recorded a cloud hand-off")
            }
            Ok(CloudFinished::Kept) => {
                tracing::warn!(%member, handoff = %id, routine = routine.routine_id.as_str(), state, "a cloud hand-off had its outcome already; kept that one")
            }
            Ok(CloudFinished::Gone) => {
                tracing::warn!(%member, handoff = %id, routine = routine.routine_id.as_str(), state, "a cloud hand-off was deleted with the member's routines while its request was out; recorded nothing")
            }
            Err(err) => {
                tracing::warn!(%member, handoff = %id, routine = routine.routine_id.as_str(), state, error = %err, "couldn't record a cloud hand-off's outcome")
            }
        }
        Ok(outcome_reply(label, &outcome, origin))
    }

    async fn cloud_list(&self, key: &MemberKey, origin: &Origin) -> Result<String, Failure> {
        let member = self.member(key).await?;
        let routines = match member {
            Some(member) => self.inner.store.cloud_routines(member).await?,
            None => Vec::new(),
        };
        let mut text = if routines.is_empty() {
            format!(
                "You have no routines. Register one with {}.",
                add_command(origin, "<routine> <url> <token>")
            )
        } else {
            let mut text = "Your routines:".to_owned();
            for routine in &routines {
                text.push_str(&format!(
                    "\n- `{}`: `{}`",
                    routine.label, routine.routine_id
                ));
                if self
                    .cloud
                    .as_ref()
                    .is_some_and(|fire| !fire.fires_for(&routine.url_origin))
                {
                    text.push_str(&format!(
                        ", registered for another routine endpoint than this agentd fires: \
                         register it again with {}",
                        add_command(origin, &format!("{} <url> <token>", routine.label))
                    ));
                }
            }
            text
        };
        if self.cloud.is_none() {
            text.push_str(&format!(
                "\n\nCloud hand-off is off on this agentd: {} starts nothing.",
                origin.command("cloud run")
            ));
        }
        let handoffs = match member {
            Some(member) => self
                .inner
                .store
                .recent_cloud_handoffs(member, LISTED_HANDOFFS)
                .await
                .inspect_err(|err| {
                    tracing::warn!(%member, error = %err, "couldn't read a member's cloud hand-offs");
                })
                .ok(),
            None => Some(Vec::new()),
        };
        match handoffs {
            Some(handoffs) if !handoffs.is_empty() => {
                text.push_str("\n\nYour last hand-offs, newest first:");
                for recent in &handoffs {
                    text.push_str("\n- ");
                    text.push_str(&handoff_line(
                        &recent.handoff,
                        &task_line(recent.task.expose_secret()),
                    ));
                }
            }
            Some(_) => {}
            None => text.push_str("\n\nI couldn't read your hand-offs just now."),
        }
        text.push_str(
            "\n\nI only record that I fired a routine and what its endpoint answered: I don't \
             follow sessions. Follow yours at claude.ai/code.",
        );
        Ok(text)
    }

    async fn cloud_rm(&self, key: &MemberKey, label: &RoutineLabel) -> Result<String, Failure> {
        let deleted = match self.member(key).await? {
            Some(member) => {
                let deleted = self
                    .inner
                    .store
                    .delete_cloud_routine(member, label.as_str())
                    .await?;
                if deleted {
                    tracing::info!(%member, "deleted a cloud routine");
                }
                deleted
            }
            None => false,
        };
        Ok(if deleted {
            format!(
                "Forgot routine `{label}` and its token. I can't revoke the token: revoke it \
                 with **Revoke** on the routine's API trigger at {ROUTINES_PAGE}."
            )
        } else {
            format!("You have no routine `{label}`.")
        })
    }
}

/// The hand-off's record of where a command came from, for the private
/// places a `cloud` command may run; `None` anywhere else.
fn cloud_origin(origin: &Origin) -> Option<CloudOrigin> {
    match origin {
        Origin::SlackSlash { .. } => Some(CloudOrigin::SlackSlash),
        Origin::SlackDm { .. } => Some(CloudOrigin::SlackDm),
        Origin::RocketChatDm { .. } => Some(CloudOrigin::RocketChatDm),
        Origin::RocketChatChannel { .. } => None,
    }
}

/// The reply to `cloud add` or `cloud run` without `[cloud]`.
fn cloud_off(what: &str) -> String {
    format!(
        "Cloud hand-off is off on this agentd, so {what}. `cloud list` and `cloud rm` still work."
    )
}

fn no_routine(label: &RoutineLabel, origin: &Origin) -> String {
    format!(
        "You have no routine `{label}`. {} shows yours, and {} registers one.",
        origin.command("cloud list"),
        add_command(origin, "<routine> <url> <token>")
    )
}

/// `n` hand-offs, in words.
pub(super) fn handoffs_counted(n: u64) -> String {
    if n == 1 {
        "1 hand-off".to_owned()
    } else {
        format!("{n} hand-offs")
    }
}

/// How the member registers a routine, with `args` after `cloud add`: on
/// Slack always the slash command, whose text Slack keeps nowhere, since a
/// direct message would keep the token in Slack's history.
fn add_command(origin: &Origin, args: &str) -> String {
    if origin.is_slack() {
        format!("`/agent cloud add {args}`")
    } else {
        origin.command(&format!("cloud add {args}"))
    }
}

/// `n` routines, in words.
pub(super) fn routines_counted(n: u64) -> String {
    if n == 1 {
        "1 routine".to_owned()
    } else {
        format!("{n} routines")
    }
}

/// One hand-off in `cloud list`: when it was asked, its routine, where it
/// stands, and its task's first line `task`.
fn handoff_line(handoff: &CloudHandoff, task: &str) -> String {
    let state = match handoff.state {
        CloudHandoffState::Sending => "no answer yet".to_owned(),
        CloudHandoffState::Fired => match (&handoff.session_url, &handoff.session_id) {
            (Some(url), _) => format!("started {url}"),
            (None, Some(id)) => format!("started `{id}`, at https://claude.ai/code"),
            (None, None) => "started".to_owned(),
        },
        CloudHandoffState::Rejected => match handoff.http_status {
            Some(status) => format!("refused (HTTP {status}), nothing started"),
            None => "not sent, nothing started".to_owned(),
        },
        CloudHandoffState::Unknown => "no known answer, it may have started".to_owned(),
    };
    format!(
        "{}, `{}`: {state}. Task: {task}",
        when(handoff.created_at),
        handoff.routine_label
    )
}

/// The first line of `task` as `cloud list` shows it: cut to
/// [`LISTED_TASK_CHARS`] characters, as a code span, so nothing in it
/// formats, links, mentions or broadcasts on either surface, and Slack's
/// renderer escapes it. A code span can't show a backtick, so backticks
/// are left out.
fn task_line(task: &str) -> String {
    let line: String = task.lines().next().unwrap_or_default().replace('`', "");
    let line = line.trim();
    let mut shown: String = line.chars().take(LISTED_TASK_CHARS).collect();
    if shown.len() < line.len() {
        shown.push('…');
    }
    if shown.trim().is_empty() {
        "(nothing to show)".to_owned()
    } else {
        format!("`{shown}`")
    }
}

/// The task of a `cloud run` whose text, as the surface delivered it, is
/// `delivered`; `None` if it isn't one. The command words and the label
/// hold no entity or token, so they parse the same as the decoded text.
fn delivered_task(delivered: &str) -> Option<String> {
    match commands::parse(delivered) {
        Ok(Command::Cloud(CloudCommand::Run { task, .. })) => Some(task),
        _ => None,
    }
}

/// A Slack task as Slack showed it to the member: each `<…>` token
/// rewritten, `<@U…|name>` to `@name`, `<#C…|name>` to `#name`, `<url>` and
/// a `<url|label>` whose label is its URL to the URL, and any other
/// `<url|label>` to `label (url)`, so a label can't hide where a link
/// goes; the entities in the rest decoded. Slack writes every `<` and `>`
/// the member typed as an entity, so every bracket left in `delivered` is
/// Slack's own.
///
/// # Errors
///
/// [`SLACK_TOKEN_REFUSED`] for any other token, such as a broadcast, a
/// user group, a date or a mention without its name, and for a `<` that is
/// never closed.
fn slack_task(delivered: &str) -> Result<String, &'static str> {
    let mut shown = String::with_capacity(delivered.len());
    let mut rest = delivered;
    while let Some(open) = rest.find('<') {
        shown.push_str(&unescape(&rest[..open]));
        let after = &rest[open + 1..];
        let close = after.find('>').ok_or(SLACK_TOKEN_REFUSED)?;
        shown.push_str(&shown_token(&after[..close]).ok_or(SLACK_TOKEN_REFUSED)?);
        rest = &after[close + 1..];
    }
    shown.push_str(&unescape(rest));
    Ok(shown)
}

/// What Slack showed for the token `<token>`, if it is a named mention, a
/// named channel or a link.
fn shown_token(token: &str) -> Option<String> {
    let named = |sigil: char, rest: &str| {
        let (_, name) = rest.split_once('|')?;
        (!name.is_empty()).then(|| format!("{sigil}{}", unescape(name)))
    };
    if let Some(user) = token.strip_prefix('@') {
        return named('@', user);
    }
    if let Some(channel) = token.strip_prefix('#') {
        return named('#', channel);
    }
    let (url, label) = token.split_once('|').unwrap_or((token, ""));
    if !is_link(url) {
        return None;
    }
    let (url, label) = (unescape(url), unescape(label));
    Some(if label.is_empty() || label == url {
        url
    } else {
        format!("{label} ({url})")
    })
}

/// Whether `url` is what Slack puts in a link token: a scheme, such as
/// `https` or `mailto`, then `:` and something without blanks.
fn is_link(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        && !rest.is_empty()
        && !rest.chars().any(char::is_whitespace)
}

/// The reply to a `cloud run` of routine `label` that led to `outcome`.
fn outcome_reply(label: &RoutineLabel, outcome: &CloudOutcome, origin: &Origin) -> String {
    let again = add_command(origin, &format!("{label} <url> <token>"));
    match outcome {
        CloudOutcome::Fired {
            session_id,
            session_url,
        } => {
            let started = match session_url {
                Some(url) => format!("Started a cloud session from routine `{label}`: {url}"),
                None => format!(
                    "Started cloud session `{session_id}` from routine `{label}`. Find it at \
                     {}",
                    SESSION_URL_PREFIX.trim_end_matches('/')
                ),
            };
            format!(
                "{started}\n\nI won't follow it from here. Follow and steer it at that link, in \
                 the Claude app, or with `claude --teleport {session_id}` in a checkout of the \
                 repository."
            )
        }
        CloudOutcome::Rejected {
            status,
            retry_after_secs,
            ..
        } => match status {
            None => "Nothing was started: the request couldn't be sent to the routine endpoint. \
                     Try again in a while."
                .to_owned(),
            Some(400) => format!(
                "Nothing was started: routine `{label}` refused the task. It may be paused; \
                 check it at {ROUTINES_PAGE}."
            ),
            Some(401) => format!(
                "Nothing was started: the routine endpoint refused routine `{label}`'s token. \
                 Generate a new token on its API trigger at {ROUTINES_PAGE}, then send {again}."
            ),
            Some(403) => "Nothing was started: the account can't fire routines, or something \
                          in between refused the request."
                .to_owned(),
            Some(404) => format!(
                "Nothing was started: the routine endpoint doesn't know routine `{label}`. It \
                 may have been deleted; if so, remove it with {}.",
                origin.command(&format!("cloud rm {label}"))
            ),
            Some(429) => {
                let wait = retry_after_secs.map_or_else(
                    || "Try again later".to_owned(),
                    |secs| format!("It resets in about {}", wait_in_words(secs)),
                );
                format!(
                    "Nothing was started: the routine or the account fired as often as an hour \
                     allows. {wait}; I don't retry on my own."
                )
            }
            Some(status) => format!(
                "Nothing was started: the routine endpoint refused the request (HTTP {status})."
            ),
        },
        CloudOutcome::Unknown { .. } => {
            "I can't tell whether a cloud session started: the routine endpoint's answer didn't \
             say, or never came. Check claude.ai/code before running it again; I don't retry \
             on my own."
                .to_owned()
        }
    }
}

/// `secs` in whole minutes, at least one, or in whole hours past an hour.
fn wait_in_words(secs: u32) -> String {
    match secs.div_ceil(60) {
        0 | 1 => "a minute".to_owned(),
        n @ 2..=60 => format!("{n} minutes"),
        n => format!("{} hours", n.div_ceil(60)),
    }
}
