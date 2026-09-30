//! [`Supervisor`]: one realtime connection per active binding.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use core_types::{Binding, BindingId, InboundEvent, Sender, SurfaceKind};
use store::ActiveBot;
use surface_rocketchat::rest::Credentials;
use surface_rocketchat::{BotRoles, Dedup, RocketChatConfig, RocketChatSurface};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;

use super::RocketChatAgents;
use crate::commands::rocketchat::{CommandFeed, listen};

/// How often the supervisor runs a pass without being poked.
pub const SUPERVISE_INTERVAL: Duration = Duration::from_secs(60);

/// Keeps agentd listening as every agent's bot on one Rocket.Chat server.
///
/// Each pass, at startup, on every [poke](RocketChatAgents::poke) and every
/// [`SUPERVISE_INTERVAL`]:
///
/// 1. abandons creations that never finished
///    ([`RocketChatAgents::abandon_stale`]),
/// 2. retires the bot users that owe it
///    ([`RocketChatAgents::retire_pending`]),
/// 3. starts a connection for each active binding of an active or paused
///    agent that has none, and stops the connections of every other.
///
/// A paused agent's bot keeps listening: whichever connection records a
/// message first delivers it for every bot in the room, so a paused bot
/// that dropped messages would lose them for the others, and commands in
/// rooms it alone shares with agentd would go unheard.
///
/// Every connection, like the manager bot's, delivers through a
/// [`CommandFeed`]: commands go to the one intake and everything else to
/// `onward`. A connection that ends on its own, such as for a revoked token,
/// is logged and started again by the next pass.
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

/// A running connection: which start of it this is, and its stop signal.
struct Running {
    generation: u64,
    stop: watch::Sender<bool>,
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
        let mut running: HashMap<BindingId, Running> = HashMap::new();
        let mut tasks: JoinSet<(BindingId, u64)> = JoinSet::new();
        let mut generation = 0;
        let mut ticks = tokio::time::interval(self.every);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = stopping.wait_for(|stop| *stop) => break,
                Some(joined) = tasks.join_next() => {
                    if let Ok((binding, ended)) = joined
                        && running.get(&binding).is_some_and(|r| r.generation == ended)
                    {
                        running.remove(&binding);
                    }
                    continue;
                }
                () = self.agents.poked() => {}
                _ = ticks.tick() => {}
            }
            self.pass(&mut running, &mut tasks, &mut generation).await;
        }
        for connection in running.values() {
            connection.stop.send_replace(true);
        }
        while tasks.join_next().await.is_some() {}
    }

    async fn pass(
        &self,
        running: &mut HashMap<BindingId, Running>,
        tasks: &mut JoinSet<(BindingId, u64)>,
        generation: &mut u64,
    ) {
        if let Err(err) = self.agents.abandon_stale().await {
            tracing::warn!(error = %err, "looking for abandoned agent creations failed");
        }
        if let Err(err) = self.agents.retire_pending().await {
            tracing::warn!(error = %err, "retiring deleted agents' bot users failed");
        }
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
        running.retain(|binding, connection| {
            let keep = wanted.iter().any(|bot| bot.binding == *binding);
            if !keep {
                connection.stop.send_replace(true);
                tracing::info!(%binding, "stopped an agent's Rocket.Chat connection");
            }
            keep
        });
        for bot in wanted {
            if running.contains_key(&bot.binding) {
                continue;
            }
            *generation += 1;
            if let Some(stop) = self.start(bot, *generation, tasks) {
                running.insert(
                    stop.0,
                    Running {
                        generation: *generation,
                        stop: stop.1,
                    },
                );
            }
        }
    }

    /// Starts `bot`'s connection as start number `generation`, and returns
    /// its binding and stop signal, or `None` if its surface can't be
    /// built.
    fn start(
        &self,
        bot: ActiveBot,
        generation: u64,
        tasks: &mut JoinSet<(BindingId, u64)>,
    ) -> Option<(BindingId, watch::Sender<bool>)> {
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
        tasks.spawn(async move {
            if let Err(err) = listen(surface, binding, events, stopping).await {
                tracing::error!(binding = %id, error = %err, "an agent's Rocket.Chat connection ended");
            }
            (id, generation)
        });
        tracing::info!(binding = %id, agent = %bot.agent, "started an agent's Rocket.Chat connection");
        Some((id, stop))
    }
}
