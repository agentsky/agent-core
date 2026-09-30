//! What agentd caches about a Slack workspace: its members, for mentions,
//! and which bot user each bot id belongs to.
//!
//! Every binding in a workspace shares one [`TeamDirectory`], so a team's
//! members are listed once however many agents are installed there. The
//! caches are rebuilt from Slack after a restart; nothing here needs to be
//! durable.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use core_types::{SurfaceError, TeamId, UserId};
use render::MentionDirectory;
use tokio::time::Instant;

use crate::normalize::is_user_id;
use crate::web::{Result, User, WebApi};

/// How long a member list is used before `users.list` is read again.
pub const DEFAULT_MEMBER_TTL: Duration = Duration::from_secs(15 * 60);

/// The longest TTL [`TeamDirectory::with_ttl`] accepts.
pub const MAX_MEMBER_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How long a stale member list is kept after a refresh fails, before the
/// next attempt.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(60);

/// The most bot ids remembered before the cache starts over.
const MAX_BOTS: usize = 10_000;

/// A workspace's caches, shared by every binding in it.
#[derive(Debug)]
pub struct TeamDirectory {
    team: TeamId,
    ttl: Duration,
    members: RwLock<Option<Loaded>>,
    refreshing: tokio::sync::Mutex<()>,
    bots: Mutex<HashMap<String, Option<UserId>>>,
}

#[derive(Debug, Clone)]
struct Loaded {
    next_refresh: Instant,
    directory: Arc<MemberDirectory>,
}

impl TeamDirectory {
    /// Empty caches for `team`, refreshing members every
    /// [`DEFAULT_MEMBER_TTL`].
    pub fn new(team: TeamId) -> Self {
        Self {
            team,
            ttl: DEFAULT_MEMBER_TTL,
            members: RwLock::new(None),
            refreshing: tokio::sync::Mutex::new(()),
            bots: Mutex::new(HashMap::new()),
        }
    }

    /// Sets how long a member list is used before it is read again, at most
    /// [`MAX_MEMBER_TTL`].
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl.min(MAX_MEMBER_TTL);
        self
    }

    /// The workspace.
    pub fn team(&self) -> &TeamId {
        &self.team
    }

    /// The member list as last read: empty until the first
    /// [`refresh_members`](Self::refresh_members).
    pub fn members(&self) -> Arc<MemberDirectory> {
        self.members
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map_or_else(Arc::default, |loaded| Arc::clone(&loaded.directory))
    }

    /// The member list, read again with `users.list` through `api` when it
    /// is older than the TTL. Concurrent callers share one refresh.
    ///
    /// When a refresh fails and an older list exists, the older list is
    /// returned and the next attempt waits a minute.
    ///
    /// # Errors
    ///
    /// The `users.list` error, when there is no older list to fall back on.
    pub async fn refresh_members(&self, api: &WebApi) -> Result<Arc<MemberDirectory>> {
        if let Some(fresh) = self.fresh() {
            return Ok(fresh);
        }
        let _refreshing = self.refreshing.lock().await;
        if let Some(fresh) = self.fresh() {
            return Ok(fresh);
        }
        let result = api.all_users().await;
        let now = Instant::now();
        let mut members = self.members.write().unwrap_or_else(PoisonError::into_inner);
        match result {
            Ok(users) => {
                let directory = Arc::new(MemberDirectory::from_users(&users));
                tracing::debug!(team = %self.team, names = directory.len(), "refreshed the Slack member cache");
                *members = Some(Loaded {
                    next_refresh: now + self.ttl,
                    directory: Arc::clone(&directory),
                });
                Ok(directory)
            }
            Err(err) => match members.as_mut() {
                Some(stale) => {
                    tracing::warn!(team = %self.team, error = %err, "refreshing the Slack member cache failed; keeping the old list");
                    stale.next_refresh = now + RETRY_AFTER_FAILURE.min(self.ttl);
                    Ok(Arc::clone(&stale.directory))
                }
                None => Err(err),
            },
        }
    }

    /// Whether the member list is missing or older than its TTL, with no
    /// refresh running.
    pub(crate) fn needs_refresh(&self) -> bool {
        self.fresh().is_none() && self.refreshing.try_lock().is_ok()
    }

    fn fresh(&self) -> Option<Arc<MemberDirectory>> {
        let now = Instant::now();
        self.members
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|loaded| loaded.next_refresh > now)
            .map(|loaded| Arc::clone(&loaded.directory))
    }

    /// The bot user of the bot `bot_id`, from `bots.info` through `api`,
    /// cached per bot id. `None` for a bot with no user, such as a legacy
    /// integration, or one Slack doesn't know (`bot_not_found`); that answer
    /// is cached too.
    ///
    /// # Errors
    ///
    /// Any other `bots.info` error. Nothing is cached then.
    pub async fn bot_user(&self, api: &WebApi, bot_id: &str) -> Result<Option<UserId>> {
        if let Some(known) = self.lock_bots().get(bot_id) {
            return Ok(known.clone());
        }
        let user = match api.bot_info(bot_id).await {
            Ok(bot) => bot.user_id.filter(|user| is_user_id(user.as_str())),
            Err(SurfaceError::NotFound(_)) => None,
            Err(err) => return Err(err),
        };
        let mut bots = self.lock_bots();
        if bots.len() >= MAX_BOTS {
            bots.clear();
        }
        bots.insert(bot_id.to_owned(), user.clone());
        Ok(user)
    }

    fn lock_bots(&self) -> std::sync::MutexGuard<'_, HashMap<String, Option<UserId>>> {
        self.bots.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A snapshot of a workspace's members by name, for rendering `@Name`
/// mentions.
///
/// Each active member (bot users included, so agents resolve too) is known
/// by their display name, their full name and their username. Names match
/// ignoring case and runs of white space. A name two members share resolves
/// to no one: a missed mention is better than pinging the wrong person.
#[derive(Debug, Default)]
pub struct MemberDirectory {
    names: HashMap<String, Option<UserId>>,
}

impl MemberDirectory {
    /// Builds a snapshot from `users.list` members. Deactivated members are
    /// left out.
    pub fn from_users(users: &[User]) -> Self {
        let mut names: HashMap<String, Option<UserId>> = HashMap::new();
        for user in users.iter().filter(|user| !user.deleted) {
            let candidates = [
                user.profile.display_name.as_deref(),
                user.profile.real_name.as_deref(),
                user.real_name.as_deref(),
                user.name.as_deref(),
            ];
            for name in candidates.into_iter().flatten().map(fold) {
                if name.is_empty() {
                    continue;
                }
                match names.entry(name) {
                    Entry::Vacant(slot) => {
                        slot.insert(Some(user.id.clone()));
                    }
                    Entry::Occupied(mut slot) => {
                        if slot.get().as_ref() != Some(&user.id) {
                            slot.insert(None);
                        }
                    }
                }
            }
        }
        Self { names }
    }

    /// The one member called `name`, if exactly one is.
    pub fn lookup(&self, name: &str) -> Option<&UserId> {
        self.names.get(&fold(name)).and_then(Option::as_ref)
    }

    /// How many distinct names the snapshot knows.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Whether the snapshot knows no names.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

impl MentionDirectory for MemberDirectory {
    fn resolve(&self, name: &str) -> Option<String> {
        self.lookup(name).map(ToString::to_string)
    }
}

/// Lowercases and collapses white space.
fn fold(name: &str) -> String {
    name.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::Profile;

    fn user(id: &str, display: &str, real: &str, name: &str) -> User {
        User {
            id: id.into(),
            team_id: Some("T1".into()),
            name: Some(name.into()),
            real_name: Some(real.into()),
            deleted: false,
            is_bot: false,
            profile: Profile {
                display_name: Some(display.into()),
                real_name: Some(real.into()),
            },
        }
    }

    #[test]
    fn members_resolve_by_display_real_and_user_name_ignoring_case_and_spacing() {
        let directory = MemberDirectory::from_users(&[
            user("U1", "Ada", "Ada Lovelace", "ada.l"),
            user("U2", "", "Grace  Hopper", "grace"),
        ]);
        for name in ["ada", "ADA", "Ada Lovelace", "ada   lovelace", "ada.l"] {
            assert_eq!(directory.resolve(name).as_deref(), Some("U1"), "{name}");
        }
        assert_eq!(directory.resolve("Grace Hopper").as_deref(), Some("U2"));
        assert_eq!(directory.resolve("grace").as_deref(), Some("U2"));
        assert_eq!(directory.resolve("nobody"), None);
        assert_eq!(directory.resolve(""), None);
    }

    #[test]
    fn the_ttl_is_capped() {
        let directory = TeamDirectory::new("T1".into()).with_ttl(Duration::MAX);
        assert_eq!(directory.ttl, MAX_MEMBER_TTL);
        assert_eq!(directory.team().as_str(), "T1");
    }

    #[test]
    fn a_shared_name_resolves_to_no_one() {
        let directory = MemberDirectory::from_users(&[
            user("U1", "Sam", "Sam One", "sam1"),
            user("U2", "sam", "Sam Two", "sam2"),
        ]);
        assert_eq!(directory.resolve("Sam"), None);
        assert_eq!(directory.resolve("Sam Two").as_deref(), Some("U2"));
    }

    #[test]
    fn deactivated_members_are_left_out_and_bots_kept() {
        let mut gone = user("U1", "Old", "Old Timer", "old");
        gone.deleted = true;
        let mut agent = user("U2", "", "helper", "helper");
        agent.is_bot = true;
        let directory = MemberDirectory::from_users(&[gone, agent]);
        assert_eq!(directory.resolve("old"), None);
        assert_eq!(directory.resolve("helper").as_deref(), Some("U2"));
        assert_eq!(directory.len(), 1);
        assert!(!directory.is_empty());
        assert!(MemberDirectory::default().is_empty());
    }
}
