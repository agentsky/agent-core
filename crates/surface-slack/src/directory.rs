//! What agentd caches about a Slack workspace: its members, for mentions,
//! and which bot user each bot id belongs to.
//!
//! Every binding in a workspace shares one [`TeamDirectory`], so a team's
//! members are listed once however many agents are installed there. The
//! caches are rebuilt from Slack after a restart; nothing here needs to be
//! durable.

use std::collections::{HashMap, HashSet};
use std::fmt;
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

/// How long after a failed refresh the next attempt waits.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(60);

/// The most bot ids remembered before the cache starts over.
const MAX_BOTS: usize = 10_000;

/// A workspace's caches, shared by every binding in it.
///
/// `Debug` shows the team and how much is cached, never names.
pub struct TeamDirectory {
    team: TeamId,
    ttl: Duration,
    members: RwLock<Members>,
    refreshing: tokio::sync::Mutex<()>,
    bots: Mutex<HashMap<String, Option<UserId>>>,
}

/// The member list, and when `users.list` may be read again.
#[derive(Default)]
struct Members {
    directory: Arc<MemberDirectory>,
    loaded: bool,
    next_attempt: Option<Instant>,
    failure: Option<SurfaceError>,
}

impl Members {
    fn waiting(&self, now: Instant) -> bool {
        self.next_attempt.is_some_and(|next| next > now)
    }
}

impl TeamDirectory {
    /// Empty caches for `team`, refreshing members every
    /// [`DEFAULT_MEMBER_TTL`].
    pub fn new(team: TeamId) -> Self {
        Self {
            team,
            ttl: DEFAULT_MEMBER_TTL,
            members: RwLock::new(Members::default()),
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

    /// Sets the bot users of the agents agentd manages in this workspace,
    /// replacing any set before. A name one of them shares with other
    /// members resolves to that agent (see [`MemberDirectory`]). It applies
    /// to the current member list and every later one.
    pub fn set_managed_bots(&self, bots: impl IntoIterator<Item = UserId>) {
        let managed = Arc::new(bots.into_iter().collect::<HashSet<_>>());
        let mut members = self.write_members();
        members.directory = Arc::new(members.directory.with_managed(managed));
    }

    /// The member list as last read: empty until the first
    /// [`refresh_members`](Self::refresh_members).
    pub fn members(&self) -> Arc<MemberDirectory> {
        Arc::clone(&self.read_members().directory)
    }

    /// The member list, read again with `users.list` through `api` when it
    /// is older than the TTL. Concurrent callers share one refresh.
    ///
    /// After a failed refresh, the next attempt waits a minute (or the TTL,
    /// if shorter). Meanwhile the older list is returned, or, when there is
    /// none, the same error.
    ///
    /// # Errors
    ///
    /// The `users.list` error, when there is no older list to fall back on.
    pub async fn refresh_members(&self, api: &WebApi) -> Result<Arc<MemberDirectory>> {
        if let Some(fresh) = self.fresh() {
            return fresh;
        }
        let _refreshing = self.refreshing.lock().await;
        if let Some(fresh) = self.fresh() {
            return fresh;
        }
        let names = api
            .all_users()
            .await
            .map(|users| Arc::new(names_from_users(&users)));
        let now = Instant::now();
        let mut members = self.write_members();
        match names {
            Ok(names) => {
                let directory = Arc::new(MemberDirectory {
                    names,
                    managed: Arc::clone(&members.directory.managed),
                });
                *members = Members {
                    directory: Arc::clone(&directory),
                    loaded: true,
                    next_attempt: Some(now + self.ttl),
                    failure: None,
                };
                drop(members);
                tracing::debug!(team = %self.team, names = directory.len(), "refreshed the Slack member cache");
                Ok(directory)
            }
            Err(err) => {
                members.next_attempt = Some(now + RETRY_AFTER_FAILURE.min(self.ttl));
                if members.loaded {
                    let stale = Arc::clone(&members.directory);
                    drop(members);
                    tracing::warn!(team = %self.team, error = %err, "refreshing the Slack member cache failed; keeping the old list");
                    Ok(stale)
                } else {
                    members.failure = Some(err.clone());
                    Err(err)
                }
            }
        }
    }

    /// Whether the member list is missing or older than its TTL, no refresh
    /// is running, and no failed one asks to wait.
    pub(crate) fn needs_refresh(&self) -> bool {
        let waiting = self.read_members().waiting(Instant::now());
        !waiting && self.refreshing.try_lock().is_ok()
    }

    /// The answer to give without reading `users.list`, while the list is
    /// fresh or a failed refresh asks to wait.
    fn fresh(&self) -> Option<Result<Arc<MemberDirectory>>> {
        let members = self.read_members();
        if !members.waiting(Instant::now()) {
            return None;
        }
        Some(match &members.failure {
            Some(err) => Err(err.clone()),
            None => Ok(Arc::clone(&members.directory)),
        })
    }

    fn read_members(&self) -> std::sync::RwLockReadGuard<'_, Members> {
        self.members.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_members(&self) -> std::sync::RwLockWriteGuard<'_, Members> {
        self.members.write().unwrap_or_else(PoisonError::into_inner)
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

impl fmt::Debug for TeamDirectory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let directory = self.members();
        f.debug_struct("TeamDirectory")
            .field("team", &self.team)
            .field("ttl", &self.ttl)
            .field("names", &directory.len())
            .field("managed_bots", &directory.managed.len())
            .field("bots", &self.lock_bots().len())
            .finish_non_exhaustive()
    }
}

/// A snapshot of a workspace's members by name, for rendering `@Name`
/// mentions.
///
/// Each active member (bot users included, so agents resolve too) is known
/// by their display name and their full name; bot users also by their
/// username. A human's username is left out: it is often the local part of
/// their email address, which could shadow an agent's name. Names match
/// ignoring case and runs of white space.
///
/// A name two members share resolves to no one, since a missed mention is
/// better than pinging the wrong person, unless exactly one of them is a
/// managed agent's bot user ([`TeamDirectory::set_managed_bots`]): then it
/// resolves to that agent, so a human can't take an agent's name.
///
/// `Debug` shows counts, never names.
#[derive(Default)]
pub struct MemberDirectory {
    names: Arc<HashMap<String, Vec<UserId>>>,
    managed: Arc<HashSet<UserId>>,
}

impl MemberDirectory {
    /// Builds a snapshot from `users.list` members, with no managed agents.
    /// Deactivated members are left out.
    pub fn from_users(users: &[User]) -> Self {
        Self {
            names: Arc::new(names_from_users(users)),
            managed: Arc::default(),
        }
    }

    /// The same names, with `managed` as the managed agents' bot users.
    fn with_managed(&self, managed: Arc<HashSet<UserId>>) -> Self {
        Self {
            names: Arc::clone(&self.names),
            managed,
        }
    }

    /// The member called `name`: the only one, or the only managed agent
    /// among several.
    pub fn lookup(&self, name: &str) -> Option<&UserId> {
        let ids = self.names.get(&fold(name))?;
        let mut agents = ids.iter().filter(|id| self.managed.contains(*id));
        match (agents.next(), agents.next()) {
            (Some(agent), None) => Some(agent),
            (None, _) if ids.len() == 1 => ids.first(),
            _ => None,
        }
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

impl fmt::Debug for MemberDirectory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemberDirectory")
            .field("names", &self.names.len())
            .field("managed_bots", &self.managed.len())
            .finish()
    }
}

impl MentionDirectory for MemberDirectory {
    fn resolve(&self, name: &str) -> Option<String> {
        self.lookup(name).map(ToString::to_string)
    }
}

/// Every active member's names, folded, each with the distinct ids of the
/// members it names.
fn names_from_users(users: &[User]) -> HashMap<String, Vec<UserId>> {
    let mut names: HashMap<String, Vec<UserId>> = HashMap::new();
    for user in users.iter().filter(|user| !user.deleted) {
        let candidates = [
            user.profile.display_name.as_deref(),
            user.profile.real_name.as_deref(),
            user.real_name.as_deref(),
            user.name.as_deref().filter(|_| user.is_bot),
        ];
        for name in candidates.into_iter().flatten().map(fold) {
            if !name.is_empty() {
                names.entry(name).or_default().push(user.id.clone());
            }
        }
    }
    for ids in names.values_mut() {
        ids.sort_unstable();
        ids.dedup();
    }
    names
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

    fn bot(id: &str, name: &str) -> User {
        User {
            is_bot: true,
            ..user(id, "", name, name)
        }
    }

    #[test]
    fn members_resolve_by_display_and_real_name_ignoring_case_and_spacing() {
        let directory = MemberDirectory::from_users(&[
            user("U1", "Ada", "Ada Lovelace", "ada.l"),
            user("U2", "", "Grace  Hopper", "grace"),
            bot("U3", "helper"),
        ]);
        for name in ["ada", "ADA", "Ada Lovelace", "ada   lovelace"] {
            assert_eq!(directory.resolve(name).as_deref(), Some("U1"), "{name}");
        }
        assert_eq!(directory.resolve("Grace Hopper").as_deref(), Some("U2"));
        assert_eq!(directory.resolve("helper").as_deref(), Some("U3"));
        assert_eq!(directory.resolve("nobody"), None);
        assert_eq!(directory.resolve(""), None);
    }

    #[test]
    fn a_humans_username_is_not_a_name_but_a_bots_is() {
        let mut named_bot = bot("U3", "Helper Bot");
        named_bot.name = Some("helper".into());
        let directory = MemberDirectory::from_users(&[
            user("U1", "Ada", "Ada Lovelace", "ada.l"),
            user("U2", "Grace", "Grace Hopper", "helper"),
            named_bot,
        ]);
        assert_eq!(directory.resolve("ada.l"), None);
        assert_eq!(directory.resolve("helper").as_deref(), Some("U3"));
        assert_eq!(directory.resolve("Helper Bot").as_deref(), Some("U3"));
    }

    #[test]
    fn a_managed_agent_wins_a_name_it_shares() {
        let users = [
            user("U1", "Helper", "Helper Person", "h1"),
            bot("U2", "helper"),
            bot("U3", "helper"),
            bot("U4", "scout"),
            user("U5", "Scout", "Scout Person", "s1"),
            user("U6", "Sam", "Sam One", "sam1"),
            user("U7", "Sam", "Sam Two", "sam2"),
        ];
        let plain = MemberDirectory::from_users(&users);
        assert_eq!(plain.resolve("helper"), None);
        assert_eq!(plain.resolve("scout"), None);

        let one = plain.with_managed(Arc::new(HashSet::from(["U2".into(), "U4".into()])));
        assert_eq!(one.resolve("helper").as_deref(), Some("U2"));
        assert_eq!(one.resolve("scout").as_deref(), Some("U4"));
        assert_eq!(one.resolve("Helper Person").as_deref(), Some("U1"));
        assert_eq!(one.resolve("sam"), None);

        let two = plain.with_managed(Arc::new(HashSet::from(["U2".into(), "U3".into()])));
        assert_eq!(two.resolve("helper"), None);

        let absent = plain.with_managed(Arc::new(HashSet::from(["U9".into()])));
        assert_eq!(absent.resolve("helper"), None);
        assert_eq!(absent.resolve("Helper Person").as_deref(), Some("U1"));
    }

    #[test]
    fn a_member_listed_twice_is_one_member() {
        let ada = user("U1", "Ada", "Ada Lovelace", "ada");
        let directory = MemberDirectory::from_users(&[
            ada.clone(),
            user("U2", "Grace", "Grace Hopper", "grace"),
            ada,
        ]);
        assert_eq!(directory.resolve("ada").as_deref(), Some("U1"));
    }

    #[test]
    fn debug_shows_counts_not_names() {
        let team = TeamDirectory::new("T1".into());
        team.set_managed_bots(["U2".into()]);
        let debug = format!("{team:?}");
        assert!(debug.contains("T1"), "{debug}");
        assert!(debug.contains("managed_bots: 1"), "{debug}");
        assert!(!debug.contains("U2"), "{debug}");

        let members = MemberDirectory::from_users(&[user("U1", "Ada", "Ada Lovelace", "ada")]);
        let debug = format!("{members:?}");
        assert!(!debug.contains("ada") && !debug.contains("U1"), "{debug}");
        assert!(debug.contains("names: 2"), "{debug}");
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
