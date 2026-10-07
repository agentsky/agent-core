//! [`Supervisor`]: one realtime connection per active binding.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::time::Duration;

use core_types::{Binding, BindingId, InboundEvent, Sender, SurfaceKind};
use store::ActiveBot;
use surface_rocketchat::rest::Credentials;
use surface_rocketchat::{BotRoles, Dedup, RocketChatConfig, RocketChatSurface};
use tokio::sync::watch;
use tokio::task::{self, JoinError, JoinSet};
use tokio::time::{Instant, MissedTickBehavior};

use super::RocketChatAgents;
use crate::commands::rocketchat::{CommandFeed, listen};

/// How often the supervisor runs a pass without being poked.
pub const SUPERVISE_INTERVAL: Duration = Duration::from_secs(60);

/// Keeps agentd listening as every agent's bot on one Rocket.Chat server.
///
/// Each pass, at startup, on every [poke](RocketChatAgents::poke) and every
/// [`SUPERVISE_INTERVAL`]:
///
/// 1. starts a connection for each active binding of an active or paused
///    agent that has none, and stops the connections of every other,
/// 2. abandons creations that never finished
///    ([`RocketChatAgents::abandon_stale`]),
/// 3. retires the bot users that owe it
///    ([`RocketChatAgents::retire_pending`]).
///
/// A paused agent's bot keeps listening: whichever connection records a
/// message first delivers it for every bot in the room, so a paused bot
/// that dropped messages would lose them for the others, and commands in
/// rooms it alone shares with agentd would go unheard.
///
/// Every connection, like the manager bot's, delivers through a
/// [`CommandFeed`]: commands go to the one intake and everything else to
/// `onward`. A connection that ends on its own, such as for a revoked token,
/// or panics, is logged and started again by the next pass. One that keeps
/// ending waits longer each time: one interval after its second end in a
/// row, doubling up to 32 intervals, so a broken token doesn't log an error
/// every pass. One that ran for 32 intervals starts over.
pub struct Supervisor {
    agents: RocketChatAgents,
    template: RocketChatConfig,
    dedup: Arc<dyn Dedup>,
    bots: BotRoles,
    feed: CommandFeed,
    onward: Option<Sender<InboundEvent>>,
    every: Duration,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor")
            .field("team", self.agents.team())
            .finish_non_exhaustive()
    }
}

/// A running connection: which start of it this is, when it started, and
/// its stop signal.
struct Running {
    generation: u64,
    started: Instant,
    stop: watch::Sender<bool>,
}

/// How often a binding's connection ended on its own in a row, and when it
/// may start again.
struct Failing {
    times: u32,
    retry_at: Instant,
}

/// The connections: the running ones by binding, their tasks, which task
/// is which start of which binding, and the bindings whose connections keep
/// ending.
#[derive(Default)]
struct Connections {
    running: HashMap<BindingId, Running>,
    tasks: JoinSet<()>,
    starts: HashMap<task::Id, (BindingId, u64)>,
    failing: HashMap<BindingId, Failing>,
    generation: u64,
}

/// The longest wait before restarting a connection that keeps ending, in
/// supervisor intervals.
const RESTART_BACKOFF_MAX_INTERVALS: u32 = 32;

/// How long after the `times`th end in a row a connection may start again,
/// when passes run every `every`: at the next pass after the first, then
/// after `every`, doubling up to [`RESTART_BACKOFF_MAX_INTERVALS`] of them.
pub(super) fn restart_delay(times: u32, every: Duration) -> Duration {
    match times {
        0 | 1 => Duration::ZERO,
        n => every
            .saturating_mul(1 << (n - 2).min(31))
            .min(every.saturating_mul(RESTART_BACKOFF_MAX_INTERVALS)),
    }
}

impl Connections {
    /// Handles the end of a connection task. One that ended on its own,
    /// rather than by being stopped, leaves the running set, so the next
    /// pass may start it again after its restart delay.
    fn ended(&mut self, joined: Result<task::Id, JoinError>, every: Duration) {
        let id = match &joined {
            Ok(id) => *id,
            Err(err) => err.id(),
        };
        let Some((binding, generation)) = self.starts.remove(&id) else {
            return;
        };
        if let Err(err) = &joined
            && err.is_panic()
        {
            tracing::error!(%binding, "an agent's Rocket.Chat connection panicked");
        }
        let running = match self.running.entry(binding) {
            Entry::Occupied(entry) if entry.get().generation == generation => entry.remove(),
            _ => return,
        };
        let ran = running.started.elapsed();
        let failing = self.failing.entry(binding).or_insert(Failing {
            times: 0,
            retry_at: Instant::now(),
        });
        if ran >= every.saturating_mul(RESTART_BACKOFF_MAX_INTERVALS) {
            failing.times = 0;
        }
        failing.times = failing.times.saturating_add(1);
        failing.retry_at = Instant::now() + restart_delay(failing.times, every);
    }

    /// Stops every connection and waits for them.
    async fn stop_all(mut self) {
        for connection in self.running.values() {
            connection.stop.send_replace(true);
        }
        while self.tasks.join_next().await.is_some() {}
    }
}

impl Supervisor {
    /// A supervisor for `agents`' server. Each bot's surface is `template`
    /// with the bot's credentials, deduplicating with `dedup` and telling
    /// bots apart with the shared `bots`. Connections deliver through
    /// `feed`, passing what isn't a command to `onward`.
    pub fn new(
        agents: RocketChatAgents,
        template: RocketChatConfig,
        dedup: Arc<dyn Dedup>,
        bots: BotRoles,
        feed: CommandFeed,
        onward: Option<Sender<InboundEvent>>,
    ) -> Self {
        Self {
            agents,
            template,
            dedup,
            bots,
            feed,
            onward,
            every: SUPERVISE_INTERVAL,
        }
    }

    /// Runs a pass every `every` instead of every [`SUPERVISE_INTERVAL`].
    pub fn every(mut self, every: Duration) -> Self {
        self.every = every;
        self
    }

    /// Runs passes until `stopping` becomes true or its sender is dropped,
    /// then stops every connection, waits for them, and drops the command
    /// feed. A pass in progress finishes first.
    pub async fn run(self, mut stopping: watch::Receiver<bool>) {
        let mut connections = Connections::default();
        let mut ticks = tokio::time::interval(self.every);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = stopping.wait_for(|stop| *stop) => break,
                Some(joined) = connections.tasks.join_next_with_id() => {
                    connections.ended(joined.map(|(id, ())| id), self.every);
                    continue;
                }
                () = self.agents.poked() => {}
                _ = ticks.tick() => {}
            }
            self.pass(&mut connections).await;
        }
        connections.stop_all().await;
    }

    async fn pass(&self, connections: &mut Connections) {
        self.reconcile(connections).await;
        if let Err(err) = self.agents.abandon_stale().await {
            tracing::warn!(error = %err, "looking for abandoned agent creations failed");
        }
        if let Err(err) = self.agents.retire_pending().await {
            tracing::warn!(error = %err, "retiring deleted agents' bot users failed");
        }
    }

    /// Starts a connection for every bot that should have one, unless it
    /// keeps ending and its restart delay runs, and stops the others.
    async fn reconcile(&self, connections: &mut Connections) {
        let wanted = match self
            .agents
            .store()
            .active_bots(SurfaceKind::RocketChat, self.agents.team())
            .await
        {
            Ok(bots) => bots,
            Err(err) => {
                tracing::warn!(error = %err, "listing the agents' bots failed");
                return;
            }
        };
        let is_wanted = |binding: &BindingId| wanted.iter().any(|bot| bot.binding == *binding);
        connections.running.retain(|binding, connection| {
            let keep = is_wanted(binding);
            if !keep {
                connection.stop.send_replace(true);
                tracing::info!(%binding, "stopped an agent's Rocket.Chat connection");
            }
            keep
        });
        connections.failing.retain(|binding, _| is_wanted(binding));
        let now = Instant::now();
        for bot in wanted {
            if connections.running.contains_key(&bot.binding)
                || connections
                    .failing
                    .get(&bot.binding)
                    .is_some_and(|f| f.retry_at > now)
            {
                continue;
            }
            connections.generation += 1;
            let generation = connections.generation;
            let binding = bot.binding;
            let Some((id, stop)) = self.start(bot, &mut connections.tasks) else {
                continue;
            };
            connections.starts.insert(id, (binding, generation));
            connections.running.insert(
                binding,
                Running {
                    generation,
                    started: now,
                    stop,
                },
            );
        }
    }

    /// Starts `bot`'s connection, and returns its task's id and its stop
    /// signal, or `None` if its surface can't be built.
    fn start(
        &self,
        bot: ActiveBot,
        tasks: &mut JoinSet<()>,
    ) -> Option<(task::Id, watch::Sender<bool>)> {
        let mut config = self.template.clone();
        config.credentials = Credentials {
            user_id: bot.bot.user.clone(),
            token: bot.token,
        };
        let surface = match RocketChatSurface::new(config, self.dedup.clone(), self.bots.clone()) {
            Ok(surface) => Arc::new(surface),
            Err(err) => {
                tracing::error!(binding = %bot.binding, error = %err, "couldn't set up an agent's Rocket.Chat surface");
                return None;
            }
        };
        let binding = Binding {
            id: bot.binding,
            agent: Some(bot.agent),
            bot: bot.bot,
        };
        let (stop, stopping) = watch::channel(false);
        let events = self.feed.clone().into_sender(self.onward.clone());
        let id = binding.id;
        let task = tasks.spawn(async move {
            if let Err(err) = listen(surface, binding, events, stopping).await {
                tracing::error!(binding = %id, error = %err, "an agent's Rocket.Chat connection ended");
            }
        });
        tracing::info!(binding = %id, agent = %bot.agent, "started an agent's Rocket.Chat connection");
        Some((task.id(), stop))
    }
}

#[cfg(test)]
mod tests;
