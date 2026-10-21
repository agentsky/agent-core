//! Commands on Rocket.Chat: which messages are commands, and the intake
//! that runs the commands every connection hears.
//!
//! Custom slash commands need an Apps-Engine app, so a Rocket.Chat command
//! is either a direct message to the manager bot, parsed as a whole, or a
//! message in any other room that starts with `!agent`, parsed after the
//! prefix (see [`commands::strip_prefix`]). A DM with an agent's bot counts
//! as another room: only the manager bot's DM is private enough for a
//! secret.
//!
//! Every bot connection receives every message in its rooms, and the
//! surface delivers each message only on the connection that records it
//! first in the store, whichever bot that is. So every connection, the
//! manager bot's and each agent's, has to look for commands in what it
//! delivers: through [`CommandFeed::into_sender`], which sends commands to
//! the one [`CommandIntake`] and passes only other messages onward, so a
//! command is never also taken as a turn. T13 has only the manager bot's
//! connection; T14 feeds every agent's into the same intake. A room without the manager bot is
//! heard by the agents' connections, and a command there is answered all the
//! same.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use core_types::{
    Binding, ConvKind, ConversationId, InboundEvent, MemberKey, SendError, Sender, Sink, Surface,
    SurfaceError,
};
use store::Store;
use surface_rocketchat::Dedup;
use surface_rocketchat::rest::RestClient;
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;

use super::{Commands, OpenDm, Origin};

/// How many commands may wait between the connections and the intake.
const COMMAND_BUFFER: usize = 64;

/// The command in `event`, with the manager bot's `binding`, and where it
/// came from; `None` if the message isn't a command. `event` may come from
/// any bot's connection.
///
/// Messages from bots, the manager bot's own replies included, are never
/// commands.
pub fn command_in<'e>(event: &'e InboundEvent, manager: &Binding) -> Option<(Origin, &'e str)> {
    if event.sender_is_bot || event.sender_bot_user.is_some() || event.sender == manager.bot {
        return None;
    }
    let room = event.conv.conversation.clone();
    if event.binding == manager.id && event.conv_kind == ConvKind::Dm {
        let text = commands::strip_prefix(&event.text, ConvKind::Dm)?;
        return Some((Origin::RocketChatDm { room }, text));
    }
    let text = commands::strip_prefix(&event.text, ConvKind::Channel)?;
    Some((Origin::RocketChatChannel { room }, text))
}

/// A command a connection heard, on its way to the intake.
struct Heard {
    member: MemberKey,
    text: String,
    origin: Origin,
}

/// Runs the commands that every Rocket.Chat connection hears, whichever
/// bot's connection delivered them.
///
/// Each command runs in its own task, so a slow one (a code exchange can
/// take 30 seconds) holds up nobody else, but one member's commands run one
/// at a time in the order they arrived: `logout` then `login` never swaps.
pub struct CommandIntake {
    commands: Commands,
    rx: mpsc::Receiver<Heard>,
}

impl std::fmt::Debug for CommandIntake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandIntake").finish_non_exhaustive()
    }
}

/// Where a connection sends the commands it hears. Clone it for each
/// connection.
///
/// The intake runs until every feed, and every sender made from one, is
/// dropped, so whatever keeps a feed to start connections later drops it
/// when agentd stops.
#[derive(Clone)]
pub struct CommandFeed {
    tx: mpsc::Sender<Heard>,
    manager: Arc<Binding>,
}

impl std::fmt::Debug for CommandFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandFeed")
            .field("manager", &self.manager.id)
            .finish_non_exhaustive()
    }
}

impl CommandIntake {
    /// An intake running `commands`, which tells commands from other
    /// messages with the manager bot's `manager` binding, and the first feed
    /// into it.
    pub fn new(commands: Commands, manager: Binding) -> (Self, CommandFeed) {
        let (tx, rx) = mpsc::channel(COMMAND_BUFFER);
        let feed = CommandFeed {
            tx,
            manager: Arc::new(manager),
        };
        (Self { commands, rx }, feed)
    }

    /// Runs every command fed in, until every feed is dropped, as the
    /// connections holding them stop. Then it runs the commands already
    /// received (the store recorded them as processed, so no other instance
    /// would) and waits for them.
    pub async fn run(self) {
        let Self { commands, mut rx } = self;
        let mut running = JoinSet::new();
        let mut last_of: HashMap<MemberKey, oneshot::Receiver<()>> = HashMap::new();
        loop {
            tokio::select! {
                heard = rx.recv() => match heard {
                    Some(heard) => start(&commands, &mut running, &mut last_of, heard),
                    None => break,
                },
                Some(joined) = running.join_next() => log_panic(joined),
            }
        }
        while let Some(joined) = running.join_next().await {
            log_panic(joined);
        }
    }
}

/// Starts `heard` in `running`, after the member's previous command.
fn start(
    commands: &Commands,
    running: &mut JoinSet<()>,
    last_of: &mut HashMap<MemberKey, oneshot::Receiver<()>>,
    heard: Heard,
) {
    last_of.retain(|_, finished| {
        matches!(
            finished.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        )
    });
    let (done, finished) = oneshot::channel();
    let previous = last_of.insert(heard.member.clone(), finished);
    let commands = commands.clone();
    running.spawn(async move {
        if let Some(previous) = previous {
            let _ = previous.await;
        }
        commands
            .handle_text(&heard.member, &heard.text, &heard.origin)
            .await;
        let _ = done.send(());
    });
}

fn log_panic(joined: Result<(), tokio::task::JoinError>) {
    if let Err(err) = joined {
        tracing::error!(error = %err, "a command task panicked");
    }
}

impl CommandFeed {
    /// Sends the command in `event`, if it is one, to the intake and
    /// returns `None`; returns any other message as it came.
    ///
    /// # Errors
    ///
    /// [`SendError`] if the intake has stopped.
    pub async fn offer(&self, event: InboundEvent) -> Result<Option<InboundEvent>, SendError> {
        let Some((origin, text)) = command_in(&event, &self.manager) else {
            return Ok(Some(event));
        };
        let heard = Heard {
            member: event.sender.clone(),
            text: text.to_owned(),
            origin,
        };
        self.tx.send(heard).await.map_err(|_| SendError)?;
        Ok(None)
    }

    /// The sender a connection delivers into: it [offers](Self::offer) each
    /// event to the intake and passes the rest to `onward`, or drops them
    /// without one.
    ///
    /// The connection that records a message first delivers it for every
    /// bot in the room, so once agents take turns, every connection, the
    /// manager bot's included, passes the rest to where turns are taken.
    /// Until then the manager bot's connection has nowhere to pass them.
    pub fn into_sender(self, onward: Option<Sender<InboundEvent>>) -> Sender<InboundEvent> {
        Sender::new(Feeding { feed: self, onward })
    }
}

struct Feeding {
    feed: CommandFeed,
    onward: Option<Sender<InboundEvent>>,
}

#[async_trait]
impl Sink<InboundEvent> for Feeding {
    async fn send(&self, event: InboundEvent) -> Result<(), SendError> {
        match (self.feed.offer(event).await?, &self.onward) {
            (Some(event), Some(onward)) => onward.send(event).await,
            _ => Ok(()),
        }
    }
}

/// Runs `binding`'s connection on `surface`, delivering into `events`,
/// until `stopping` becomes true or its sender is dropped. Then it stops
/// listening, which drops `events`.
///
/// # Errors
///
/// The error that ended the connection for good, such as
/// [`SurfaceError::Unauthorized`] for a rejected token.
pub async fn listen(
    surface: Arc<dyn Surface>,
    binding: Binding,
    events: Sender<InboundEvent>,
    mut stopping: watch::Receiver<bool>,
) -> Result<(), SurfaceError> {
    tokio::select! {
        biased;
        _ = stopping.wait_for(|stop| *stop) => Ok(()),
        ended = surface.events(&binding, events) => ended,
    }
}

/// [`Dedup`] over the store's processed events.
#[derive(Debug, Clone)]
pub struct StoreDedup(pub Store);

#[async_trait]
impl Dedup for StoreDedup {
    async fn mark_event_processed(
        &self,
        source: &str,
        event_id: &str,
    ) -> Result<bool, SurfaceError> {
        self.0
            .mark_event_processed(source, event_id, OffsetDateTime::now_utc())
            .await
            .map_err(|err| SurfaceError::Transport(err.to_string()))
    }
}

/// Opens the manager bot's DMs on Rocket.Chat: `users.info` for the
/// member's username, then `im.create`, which returns the existing DM if
/// there is one.
#[derive(Debug, Clone)]
pub struct RocketChatDms(pub RestClient);

#[async_trait]
impl OpenDm for RocketChatDms {
    async fn open_dm(&self, member: &MemberKey) -> Result<ConversationId, SurfaceError> {
        let user = self.0.user_info(&member.user).await?;
        self.0.create_dm(&user.username).await
    }
}
