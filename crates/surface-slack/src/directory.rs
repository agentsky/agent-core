//! What agentd caches about a Slack workspace: its members, for mentions
//! and for who is home, which bot user each bot id belongs to, and what
//! kind of conversation each channel is and how it is shared.
//!
//! Every binding in a workspace shares one [`TeamDirectory`], so a team's
//! members are listed once however many agents are installed there. The
//! caches are rebuilt from Slack after a restart; nothing here needs to be
//! durable: each answer is Slack's, read again when it is missing.
//!
//! # Who is home
//!
//! [`TeamDirectory::home_user`] is the independent source the design's
//! home rule asks for: a Slack Connect message's own team fields can make
//! its sender outside, never home, so a sender is home only when this
//! agrees. It answers from the member list `users.list` gave, while that
//! list is younger than [`HOME_ANSWER_TTL`], and otherwise asks
//! `users.info`. Either way a user is home only as [`is_home`] reads
//! Slack's answer: an active member, not a stranger, of the workspace, or
//! of its Enterprise Grid organization with the workspace among their
//! workspaces, and every team the answer names is one of those. The
//! answers of `users.info` are kept for [`HOME_ANSWER_TTL`], at most
//! [`MAX_HOME_ANSWERS`] of them, the oldest dropped first; a lookup that
//! fails is an error, not an answer, and isn't kept. A failure that isn't
//! Slack being briefly unable to answer (a revoked token, a missing
//! scope, …) is also logged as a warning at most once per
//! [`LOOKUP_WARNING_INTERVAL`], since it shuts out every sender the member
//! list doesn't vouch for.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use std::collections::VecDeque;

use core_types::{ConvKind, ConversationId, Sharing, SurfaceError, TeamId, Throttle, UserId};
use render::MentionDirectory;
use tokio::time::Instant;

use crate::normalize::{is_bot_id, is_enterprise_id, is_user_id};
use crate::web::{Result, User, WebApi, is_unreadable};

/// How long a member list is used before `users.list` is read again.
pub const DEFAULT_MEMBER_TTL: Duration = Duration::from_secs(15 * 60);

/// The longest TTL [`TeamDirectory::with_ttl`] accepts.
pub const MAX_MEMBER_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How long after a failed refresh the next attempt waits.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(60);

/// The most bot ids remembered with a user, and apart from them the most
/// remembered without one, before that cache starts over. Each is shaped
/// like a bot id, so the cache holds at most a few hundred kilobytes.
const MAX_BOTS: usize = 10_000;

/// How long a conversation's kind and sharing are remembered. A group DM
/// can be converted to a private channel, and a channel shared or unshared.
pub const CONV_KIND_TTL: Duration = Duration::from_secs(60 * 60);

/// The most conversations remembered before the cache starts over.
const MAX_CONV_KINDS: usize = 10_000;

/// How long an answer of `users.info` to whether a user is home is kept.
pub const HOME_ANSWER_TTL: Duration = Duration::from_secs(60 * 60);

/// The most answers of `users.info` to whether a user is home kept at
/// once; past it, the oldest is dropped.
pub const MAX_HOME_ANSWERS: usize = 4096;

/// The `users.info` error codes that answer for no user the bot may see,
/// which the home check takes, and keeps, for "not home".
const NOT_HOME_CODES: &[&str] = &["user_not_found", "user_not_visible"];

/// How often a home check's failure that won't pass on its own is logged
/// as a warning at most.
pub const LOOKUP_WARNING_INTERVAL: Duration = Duration::from_secs(60);

/// What `conversations.info` says about a conversation, as
/// [`TeamDirectory::conv_info`] keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvInfo {
    /// What kind of conversation it is: `is_im`, then `is_mpim`, else a
    /// channel.
    pub kind: ConvKind,
    /// Whether, and with whom, it is shared ([`Conversation::sharing`](crate::web::Conversation::sharing)).
    pub sharing: Sharing,
}

/// A workspace's caches, shared by every binding in it.
///
/// `Debug` shows the team and how much is cached, never names.
pub struct TeamDirectory {
    team: TeamId,
    home_org: Option<TeamId>,
    ttl: Duration,
    members: RwLock<Members>,
    refreshing: tokio::sync::Mutex<()>,
    bots: Mutex<Bots>,
    conv_infos: Mutex<HashMap<ConversationId, (ConvInfo, Instant)>>,
    home_answers: Mutex<HomeAnswers>,
    lookup_warnings: Throttle,
    grid_noticed: AtomicBool,
}

/// The answers `users.info` gave to whether a user is home, each kept for
/// [`HOME_ANSWER_TTL`] from when it was given, at most `capacity` of them,
/// the oldest dropped first. Each answer is numbered as it is kept, so
/// `order` tells an answer from an older one for the same user.
struct HomeAnswers {
    capacity: usize,
    answers: HashMap<UserId, (bool, Instant, u64)>,
    order: VecDeque<(UserId, u64)>,
    next: u64,
}

impl HomeAnswers {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            answers: HashMap::new(),
            order: VecDeque::new(),
            next: 0,
        }
    }

    /// The answer for `user` still kept at `now`.
    fn get(&mut self, user: &UserId, now: Instant) -> Option<bool> {
        let (home, given, _) = *self.answers.get(user)?;
        if now.saturating_duration_since(given) < HOME_ANSWER_TTL {
            return Some(home);
        }
        self.answers.remove(user);
        None
    }

    /// Keeps `home` for `user`, given at `now`, dropping the oldest answers
    /// past the capacity.
    fn insert(&mut self, user: UserId, home: bool, now: Instant) {
        let number = self.next;
        self.next += 1;
        self.answers.insert(user.clone(), (home, now, number));
        self.order.push_back((user, number));
        while self.answers.len() > self.capacity {
            let Some((oldest, number)) = self.order.pop_front() else {
                break;
            };
            if self
                .answers
                .get(&oldest)
                .is_some_and(|(_, _, kept)| *kept == number)
            {
                self.answers.remove(&oldest);
            }
        }
        if self.order.len() > 2 * self.capacity.max(1) {
            let answers = &self.answers;
            self.order.retain(|(user, number)| {
                answers.get(user).is_some_and(|(_, _, kept)| kept == number)
            });
        }
    }

    fn len(&self) -> usize {
        self.answers.len()
    }
}

/// Bot ids by what `bots.info` answered: those with a bot user, and apart
/// from them those without one. Anyone who can sign an agent's events can
/// make bot ids up, and each is remembered as having no user, so they fill
/// only that set, never pushing out the bots that have one.
#[derive(Default)]
struct Bots {
    users: HashMap<String, UserId>,
    userless: HashSet<String>,
}

impl Bots {
    fn get(&self, bot_id: &str) -> Option<Option<UserId>> {
        if let Some(user) = self.users.get(bot_id) {
            return Some(Some(user.clone()));
        }
        self.userless.contains(bot_id).then_some(None)
    }

    fn insert(&mut self, bot_id: &str, user: Option<UserId>) {
        match user {
            Some(user) => {
                if self.users.len() >= MAX_BOTS {
                    self.users.clear();
                }
                self.users.insert(bot_id.to_owned(), user);
            }
            None => {
                if self.userless.len() >= MAX_BOTS {
                    self.userless.clear();
                }
                self.userless.insert(bot_id.to_owned());
            }
        }
    }
}

/// The member list, and when `users.list` may be read again.
///
/// `retrying` says the last read failed, so `next_attempt` is a retry wait
/// a new managed bot doesn't cut short. `outdated` says a managed bot the
/// list lacks was set since the last read began, so the read that ends
/// next leaves the list stale.
#[derive(Default)]
struct Members {
    directory: Arc<MemberDirectory>,
    home: Arc<HashSet<UserId>>,
    home_read_at: Option<Instant>,
    loaded: bool,
    next_attempt: Option<Instant>,
    failure: Option<SurfaceError>,
    retrying: bool,
    outdated: bool,
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
            home_org: None,
            ttl: DEFAULT_MEMBER_TTL,
            members: RwLock::new(Members::default()),
            refreshing: tokio::sync::Mutex::new(()),
            bots: Mutex::default(),
            conv_infos: Mutex::new(HashMap::new()),
            home_answers: Mutex::new(HomeAnswers::new(MAX_HOME_ANSWERS)),
            lookup_warnings: Throttle::new(LOOKUP_WARNING_INTERVAL),
            grid_noticed: AtomicBool::new(false),
        }
    }

    /// Sets the workspace's Enterprise Grid organization, the
    /// `enterprise_id` `auth.test` gave, which a message's sender team
    /// fields may name for a home member
    /// ([`Context::home_org`](crate::normalize::Context::home_org)).
    #[must_use]
    pub fn with_home_org(mut self, home_org: Option<TeamId>) -> Self {
        self.home_org = home_org;
        self
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

    /// The workspace's Enterprise Grid organization, if it has one.
    pub fn home_org(&self) -> Option<&TeamId> {
        self.home_org.as_ref()
    }

    /// Sets the bot users of the agents agentd manages in this workspace,
    /// replacing any set before. A name one of them shares with other
    /// members resolves to that agent (see [`MemberDirectory`]). It applies
    /// to the current member list and every later one.
    ///
    /// When the list lacks one of them, such as an agent installed since it
    /// was read, the list goes stale, so the next
    /// [`refresh_members`](Self::refresh_members) or render reads
    /// `users.list` again, unless a failed read asks to wait.
    pub fn set_managed_bots(&self, bots: impl IntoIterator<Item = UserId>) {
        let managed: HashSet<UserId> = bots.into_iter().collect();
        let current = self.members();
        let unknown = !current.knows_all(&managed);
        let mut members = self.write_members();
        members.directory = Arc::new(members.directory.with_managed(Arc::new(managed)));
        if unknown {
            members.outdated = true;
            if !members.retrying {
                members.next_attempt = None;
            }
        }
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
        self.write_members().outdated = false;
        let reading = Instant::now();
        let names = api.all_users().await.map(|users| {
            users.iter().for_each(|user| self.notice_grid(user));
            (
                Arc::new(names_from_users(&users)),
                Arc::new(home_users(&users, &self.team, self.home_org.as_ref())),
            )
        });
        let now = Instant::now();
        let mut members = self.write_members();
        match names {
            Ok((names, home)) => {
                let directory = Arc::new(MemberDirectory {
                    names,
                    managed: Arc::clone(&members.directory.managed),
                });
                *members = Members {
                    directory: Arc::clone(&directory),
                    home,
                    home_read_at: Some(reading),
                    loaded: true,
                    next_attempt: (!members.outdated).then(|| now + self.ttl),
                    failure: None,
                    retrying: false,
                    outdated: false,
                };
                drop(members);
                tracing::debug!(team = %self.team, names = directory.len(), "refreshed the Slack member cache");
                Ok(directory)
            }
            Err(err) => {
                members.next_attempt = Some(now + RETRY_AFTER_FAILURE.min(self.ttl));
                members.retrying = true;
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
    /// is cached too, apart from the bots that have a user, so bot ids made
    /// up in forged events never push those out. `None` at once, without a
    /// call and without caching it, for an id not shaped like a bot id
    /// ([`is_bot_id`]), so each cached id is at most a few dozen bytes.
    ///
    /// # Errors
    ///
    /// Any other `bots.info` error. Nothing is cached then.
    pub async fn bot_user(&self, api: &WebApi, bot_id: &str) -> Result<Option<UserId>> {
        if !is_bot_id(bot_id) {
            return Ok(None);
        }
        if let Some(known) = self.lock_bots().get(bot_id) {
            return Ok(known);
        }
        let user = match api.bot_info(bot_id).await {
            Ok(bot) => bot.user_id.filter(|user| is_user_id(user.as_str())),
            Err(SurfaceError::NotFound(_)) => None,
            Err(err) => return Err(err),
        };
        self.lock_bots().insert(bot_id, user.clone());
        Ok(user)
    }

    /// Whether `user` belongs to the workspace, for the home rule (see
    /// [Who is home](self#who-is-home)): `Ok(true)` when the member list,
    /// read less than [`HOME_ANSWER_TTL`] ago, has them as [`is_home`]
    /// reads it, or else when `users.info` through `api` answers so;
    /// `Ok(false)` when `users.info` answers otherwise, or says
    /// `user_not_found` or `user_not_visible`. `users.info`'s answers are
    /// kept for
    /// [`HOME_ANSWER_TTL`], at most [`MAX_HOME_ANSWERS`], the oldest
    /// dropped first, and one dropped is asked again, never taken as home.
    /// Pass a client made with [`WebApi::without_waiting`] for a caller
    /// that mustn't wait for a used-up quota.
    ///
    /// # Errors
    ///
    /// Any other `users.info` error, as it came, whatever its variant
    /// ([`SurfaceError::Transport`], [`SurfaceError::RateLimited`],
    /// [`SurfaceError::Api`], [`SurfaceError::Unauthorized`],
    /// [`SurfaceError::Forbidden`] for a `missing_scope`, …), and
    /// [`SurfaceError::Api`] for an answer about another user. It is no
    /// answer, and nothing is kept. One that won't pass on its own, which
    /// is any but Slack unreachable or busy and a rate limit, is logged as a
    /// warning at most once per [`LOOKUP_WARNING_INTERVAL`].
    pub async fn home_user(&self, api: &WebApi, user: &UserId) -> Result<bool> {
        let now = Instant::now();
        if self.listed_home(user, now) {
            return Ok(true);
        }
        if let Some(home) = self.lock_home_answers().get(user, now) {
            return Ok(home);
        }
        let home = match api.user_info(user).await {
            Ok(info) if info.id != *user => {
                let err = SurfaceError::Api("users.info answered for another user".into());
                self.lookup_failed(&err);
                return Err(err);
            }
            Ok(info) => {
                self.notice_grid(&info);
                is_home(&info, &self.team, self.home_org.as_ref())
            }
            Err(SurfaceError::NotFound(code) | SurfaceError::Api(code))
                if NOT_HOME_CODES.contains(&code.as_str()) =>
            {
                false
            }
            Err(err) => {
                self.lookup_failed(&err);
                return Err(err);
            }
        };
        self.lock_home_answers()
            .insert(user.clone(), home, Instant::now());
        Ok(home)
    }

    /// Whether the member list, read less than [`HOME_ANSWER_TTL`] before
    /// `now`, has `user` as home.
    fn listed_home(&self, user: &UserId, now: Instant) -> bool {
        let members = self.read_members();
        members
            .home_read_at
            .is_some_and(|read| now.saturating_duration_since(read) < HOME_ANSWER_TTL)
            && members.home.contains(user)
    }

    /// Logs a failed `users.info` that won't pass on its own (a revoked
    /// token, a missing scope, an answer that can't be read, …), at most
    /// once per [`LOOKUP_WARNING_INTERVAL`].
    fn lookup_failed(&self, err: &SurfaceError) {
        let passing = match err {
            SurfaceError::RateLimited { .. } => true,
            SurfaceError::Transport(_) => !is_unreadable(err),
            _ => false,
        };
        if passing {
            return;
        }
        if let Some(quiet) = self.lookup_warnings.record((), std::time::Instant::now()) {
            tracing::warn!(team = %self.team, error = %err, failed_since_last_warning = quiet, "couldn't ask Slack whether a user is home; refusing whoever was asked about");
        }
    }

    /// Warns, once, when Slack describes `user` as a member of an Enterprise
    /// Grid organization while `auth.test` named none for the workspace:
    /// then no member's answer names only home, and everyone is refused.
    fn notice_grid(&self, user: &User) {
        let grid = user
            .enterprise_user
            .as_ref()
            .and_then(|grid| grid.enterprise_id.as_deref())
            .is_some_and(is_enterprise_id);
        if self.home_org.is_none() && grid && !self.grid_noticed.swap(true, Ordering::Relaxed) {
            tracing::warn!(team = %self.team, "Slack names an Enterprise Grid organization for the workspace's members, but auth.test gave the workspace none; every member is refused until agentd restarts with one");
        }
    }

    /// What kind of conversation `channel` is, from `conversations.info`;
    /// a wrapper over [`conv_info`](Self::conv_info). A channel id's prefix
    /// can't tell a group DM or a private channel from a public one.
    ///
    /// # Errors
    ///
    /// As for [`conv_info`](Self::conv_info).
    pub async fn conv_kind(&self, api: &WebApi, channel: &ConversationId) -> Result<ConvKind> {
        Ok(self.conv_info(api, channel).await?.kind)
    }

    /// What `conversations.info` through `api` says about `channel`: its
    /// kind (`is_im`, then `is_mpim`, else a channel) and its sharing,
    /// remembered for [`CONV_KIND_TTL`].
    ///
    /// # Errors
    ///
    /// The `conversations.info` error, or [`SurfaceError::NotFound`] when
    /// the id Slack answers with isn't `channel` exactly, as for an id in
    /// another case. Nothing is cached then.
    pub async fn conv_info(&self, api: &WebApi, channel: &ConversationId) -> Result<ConvInfo> {
        if let Some((info, until)) = self.lock_conv_infos().get(channel)
            && *until > Instant::now()
        {
            return Ok(info.clone());
        }
        self.conv_info_fresh(api, channel).await
    }

    /// As [`conv_info`](Self::conv_info), but always asking Slack, for
    /// where the sharing guards the owner's work; the answer replaces the
    /// one remembered.
    ///
    /// # Errors
    ///
    /// As for [`conv_info`](Self::conv_info). A failed read leaves what was
    /// remembered as it was.
    pub async fn conv_info_fresh(
        &self,
        api: &WebApi,
        channel: &ConversationId,
    ) -> Result<ConvInfo> {
        let conversation = api.conversation_info(channel).await?;
        if conversation.id != *channel {
            return Err(SurfaceError::NotFound(
                "the channel id isn't Slack's own spelling".into(),
            ));
        }
        let kind = if conversation.is_im {
            ConvKind::Dm
        } else if conversation.is_mpim {
            ConvKind::GroupDm
        } else {
            ConvKind::Channel
        };
        let info = ConvInfo {
            kind,
            sharing: conversation.sharing(),
        };
        let mut infos = self.lock_conv_infos();
        if infos.len() >= MAX_CONV_KINDS {
            infos.clear();
        }
        infos.insert(
            channel.clone(),
            (info.clone(), Instant::now() + CONV_KIND_TTL),
        );
        Ok(info)
    }

    /// Forgets what [`conv_info`](Self::conv_info) remembers of `channel`,
    /// as for an id Slack replaced, so nothing reads it from the cache
    /// again.
    pub fn forget_conv(&self, channel: &ConversationId) {
        self.lock_conv_infos().remove(channel);
    }

    fn lock_conv_infos(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<ConversationId, (ConvInfo, Instant)>> {
        self.conv_infos
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_home_answers(&self) -> std::sync::MutexGuard<'_, HomeAnswers> {
        self.home_answers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_bots(&self) -> std::sync::MutexGuard<'_, Bots> {
        self.bots.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for TeamDirectory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let directory = self.members();
        let (users, userless) = {
            let bots = self.lock_bots();
            (bots.users.len(), bots.userless.len())
        };
        f.debug_struct("TeamDirectory")
            .field("team", &self.team)
            .field("ttl", &self.ttl)
            .field("names", &directory.len())
            .field("managed_bots", &directory.managed.len())
            .field("bots", &users)
            .field("userless_bots", &userless)
            .field("home_members", &self.read_members().home.len())
            .field("home_answers", &self.lock_home_answers().len())
            .field("conv_infos", &self.lock_conv_infos().len())
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
/// resolves to that agent, so a human can't take an agent's name. A managed
/// agent's bot user id resolves to it too, which names it unambiguously.
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

    /// Whether every member in `ids` has a name in the snapshot.
    fn knows_all(&self, ids: &HashSet<UserId>) -> bool {
        if ids.is_empty() {
            return true;
        }
        let known: HashSet<&UserId> = self.names.values().flatten().collect();
        ids.iter().all(|id| known.contains(id))
    }

    /// The member called `name`: the only one, or the only managed agent
    /// among several. A managed agent's bot user is also called by its
    /// user id, exactly as written, so agentd can name one agent among
    /// several that share a name.
    pub fn lookup(&self, name: &str) -> Option<&UserId> {
        if let Some(bot) = self.managed.iter().find(|bot| bot.as_str() == name) {
            return Some(bot);
        }
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

/// The ids of the `users.list` entries [`is_home`] counts as home.
fn home_users(users: &[User], team: &TeamId, home_org: Option<&TeamId>) -> HashSet<UserId> {
    users
        .iter()
        .filter(|user| is_home(user, team, home_org))
        .map(|user| user.id.clone())
        .collect()
}

/// The workspaces of `home_org` that Slack's `user` belongs to, as their
/// `enterprise_user` lists them, when it is of `home_org` and lists
/// `team`.
fn grid_teams<'a>(
    user: &'a User,
    team: &TeamId,
    home_org: Option<&TeamId>,
) -> Option<&'a [TeamId]> {
    let org = home_org?;
    user.enterprise_user
        .as_ref()
        .filter(|grid| grid.enterprise_id.as_deref() == Some(org.as_str()))
        .and_then(|grid| grid.teams.as_deref())
        .filter(|teams| teams.contains(team))
}

/// Whether Slack's `user` is a member of `team`, of the Enterprise Grid
/// organization `home_org` if any: their `team_id` is `team`, or their
/// `enterprise_user` is of `home_org` and lists `team` among its
/// workspaces. It says nothing of whether the account is active, or of the
/// other teams the answer names; [`is_home`] does.
pub fn is_member(user: &User, team: &TeamId, home_org: Option<&TeamId>) -> bool {
    user.team_id.as_ref() == Some(team) || grid_teams(user, team, home_org).is_some()
}

/// Whether Slack's `user` belongs to `team`, of the Enterprise Grid
/// organization `home_org` if any: an active account (not `deleted`), not
/// a stranger, that [`is_member`] of `team`; and every team the answer
/// names (`team_id`, `profile.team`, `enterprise_user`'s organization) is
/// `team`, `home_org`, or one of the workspaces `enterprise_user` lists.
/// Guests (`is_restricted`, `is_ultra_restricted`) count as home, as they
/// did before Slack Connect: they are the workspace's own accounts.
pub fn is_home(user: &User, team: &TeamId, home_org: Option<&TeamId>) -> bool {
    if user.deleted || user.is_stranger {
        return false;
    }
    let grid_teams = grid_teams(user, team, home_org);
    let names_home = |field: &str| {
        field == team.as_str()
            || home_org.is_some_and(|org| field == org.as_str())
            || grid_teams.is_some_and(|teams| teams.iter().any(|other| other.as_str() == field))
    };
    is_member(user, team, home_org)
        && user
            .team_id
            .as_ref()
            .is_none_or(|team_id| names_home(team_id.as_str()))
        && user.profile.team.as_deref().is_none_or(names_home)
        && user
            .enterprise_user
            .as_ref()
            .and_then(|grid| grid.enterprise_id.as_deref())
            .is_none_or(names_home)
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
    use crate::web::{EnterpriseUser, Profile};

    fn user(id: &str, display: &str, real: &str, name: &str) -> User {
        User {
            id: id.into(),
            team_id: Some("T1".into()),
            name: Some(name.into()),
            real_name: Some(real.into()),
            deleted: false,
            is_bot: false,
            is_stranger: false,
            profile: Profile {
                display_name: Some(display.into()),
                real_name: Some(real.into()),
                team: None,
            },
            enterprise_user: None,
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
        assert_eq!(two.resolve("U3").as_deref(), Some("U3"), "by its id");
        assert_eq!(two.resolve("u3"), None);
        assert_eq!(two.resolve("U1"), None, "a human isn't named by id");

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

    #[test]
    fn made_up_bot_ids_never_push_out_the_bots_with_a_user() {
        let mut bots = Bots::default();
        bots.insert("B0REAL", Some("U0REAL".into()));
        for n in 0..=MAX_BOTS {
            bots.insert(&format!("B{n}"), None);
        }
        assert_eq!(bots.get("B0REAL"), Some(Some(UserId::from("U0REAL"))));
        assert_eq!(bots.get(&format!("B{MAX_BOTS}")), Some(None));
        assert_eq!(bots.get("B0"), None, "the userless cache started over");
        assert_eq!(bots.get("B0UNSEEN"), None);
    }

    #[test]
    fn the_home_answer_cache_is_bounded_and_looks_up_an_evicted_no_again() {
        let start = Instant::now();
        let mut answers = HomeAnswers::new(2);
        answers.insert("U0NO".into(), false, start);
        answers.insert("U0YES".into(), true, start);
        assert_eq!(answers.get(&"U0NO".into(), start), Some(false));
        answers.insert("U0LATER".into(), true, start);
        assert_eq!(answers.len(), 2);
        assert_eq!(
            answers.get(&"U0NO".into(), start),
            None,
            "the oldest answer, a no, was dropped and is asked again"
        );
        assert_eq!(answers.get(&"U0YES".into(), start), Some(true));
        assert_eq!(answers.get(&"U0LATER".into(), start), Some(true));

        let later = start + HOME_ANSWER_TTL;
        assert_eq!(answers.get(&"U0YES".into(), later), None, "kept an hour");
        assert_eq!(answers.len(), 1);
        answers.insert("U0YES".into(), false, later);
        answers.insert("U0LAST".into(), false, later);
        assert_eq!(
            answers.get(&"U0LATER".into(), later),
            None,
            "an answer given again is as new as its last"
        );
        assert_eq!(answers.get(&"U0YES".into(), later), Some(false));
        assert_eq!(answers.get(&"U0LAST".into(), later), Some(false));

        let mut many = HomeAnswers::new(MAX_HOME_ANSWERS);
        for n in 0..=MAX_HOME_ANSWERS {
            many.insert(format!("U{n}").into(), n % 2 == 0, start);
        }
        assert_eq!(many.len(), MAX_HOME_ANSWERS);
        assert_eq!(many.get(&"U0".into(), start), None);
        assert_eq!(many.get(&"U1".into(), start), Some(false));
        for _ in 0..3 * MAX_HOME_ANSWERS {
            many.insert("U1".into(), false, start);
        }
        assert!(many.order.len() <= 2 * MAX_HOME_ANSWERS);
        assert_eq!(
            TeamDirectory::new("T1".into()).lock_home_answers().capacity,
            MAX_HOME_ANSWERS
        );
    }

    #[test]
    fn only_members_listed_with_the_workspaces_team_are_home() {
        let mut theirs = user("U2", "Zoe", "Zoe Outside", "zoe");
        theirs.team_id = Some("T2".into());
        let mut unknown = user("U3", "Kit", "Kit Nobody", "kit");
        unknown.team_id = None;
        let mut gone = user("U4", "Old", "Old Timer", "old");
        gone.deleted = true;
        let home = home_users(
            &[user("U1", "Ada", "Ada", "ada"), theirs, unknown, gone],
            &"T1".into(),
            None,
        );
        assert_eq!(home, HashSet::from([UserId::from("U1")]));
    }

    #[test]
    fn a_member_list_older_than_an_hour_vouches_for_no_one() {
        let directory = TeamDirectory::new("T1".into());
        let read = Instant::now();
        {
            let mut members = directory.write_members();
            members.home = Arc::new(HashSet::from([UserId::from("U1")]));
            members.home_read_at = Some(read);
        }
        let ada = UserId::from("U1");
        assert!(directory.listed_home(&ada, read));
        assert!(directory.listed_home(&ada, read + HOME_ANSWER_TTL - Duration::from_secs(1)));
        assert!(
            !directory.listed_home(&ada, read + HOME_ANSWER_TTL),
            "past an hour, users.info is asked again"
        );
        assert!(!directory.listed_home(&"U2".into(), read));
        assert!(!TeamDirectory::new("T1".into()).listed_home(&ada, read));
    }

    fn grid(org: &str, teams: &[&str]) -> Option<EnterpriseUser> {
        Some(EnterpriseUser {
            enterprise_id: Some(org.into()),
            teams: Some(teams.iter().map(|team| TeamId::from(*team)).collect()),
        })
    }

    #[test]
    fn home_is_an_active_member_whose_every_team_is_home() {
        let team = TeamId::from("T1");
        let org = TeamId::from("E1");
        let home = |user: &User| is_home(user, &team, Some(&org));
        let ada = user("U1", "Ada", "Ada", "ada");
        assert!(home(&ada));
        assert!(is_home(&ada, &team, None));

        let mut gone = ada.clone();
        gone.deleted = true;
        assert!(!home(&gone), "a deactivated account isn't home");
        let mut stranger = ada.clone();
        stranger.is_stranger = true;
        assert!(!home(&stranger));

        let mut profile_elsewhere = ada.clone();
        profile_elsewhere.profile.team = Some("T2".into());
        assert!(!home(&profile_elsewhere), "every team named must be home");
        let mut profile_unreadable = ada.clone();
        profile_unreadable.profile.team = Some(String::new());
        assert!(!home(&profile_unreadable));
        let mut profile_org = ada.clone();
        profile_org.profile.team = Some("E1".into());
        assert!(home(&profile_org));

        let mut other_org = ada.clone();
        other_org.enterprise_user = grid("E2", &["T1"]);
        assert!(!home(&other_org), "another organization's member");
        let mut same_org = ada.clone();
        same_org.enterprise_user = grid("E1", &["T1", "T3"]);
        assert!(home(&same_org));

        let mut sibling = ada.clone();
        sibling.team_id = Some("T3".into());
        sibling.profile.team = Some("T3".into());
        assert!(!home(&sibling), "another workspace of the organization");
        sibling.enterprise_user = grid("E1", &["T3"]);
        assert!(
            !home(&sibling),
            "of the organization, but not of this workspace"
        );
        sibling.enterprise_user = grid("E1", &["T3", "T1"]);
        assert!(
            home(&sibling),
            "a Grid member of this workspace among others"
        );
        assert!(
            !is_home(&sibling, &team, None),
            "only with the organization auth.test named"
        );
        sibling.enterprise_user = grid("E2", &["T3", "T1"]);
        assert!(!home(&sibling));
        sibling.enterprise_user = Some(EnterpriseUser {
            enterprise_id: Some("E1".into()),
            teams: None,
        });
        assert!(
            !home(&sibling),
            "an unreadable list of workspaces lists none"
        );

        let mut theirs = ada;
        theirs.team_id = Some("T2".into());
        theirs.enterprise_user = grid("E1", &["T2", "T1"]);
        theirs.profile.team = Some("T9".into());
        assert!(
            !home(&theirs),
            "a team outside the member's workspaces fails the rule"
        );
    }
}
