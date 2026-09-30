//! Commands on Rocket.Chat: which messages are commands, and the manager
//! bot's connection that hears them.
//!
//! Custom slash commands need an Apps-Engine app, so a Rocket.Chat command
//! is either a direct message to the manager bot, parsed as a whole, or a
//! message in any other room that starts with `!agent`, parsed after the
//! prefix (see [`commands::strip_prefix`]). A DM with an agent's bot counts
//! as another room: only the manager bot's DM is private enough for a
//! secret.

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

/// How many events may wait between the connection and dispatch.
const EVENT_BUFFER: usize = 64;

/// The command in `event`, heard by the manager bot's `binding`, and where
/// it came from; `None` if the message isn't a command.
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

/// Listens as the manager bot's `binding` on `surface` and runs every
/// command it hears, until `stopping` becomes true. Then it stops listening,
/// runs the commands already received (they are recorded as processed, so no
/// other instance would), and waits for them.
///
/// Each command runs in its own task, so a slow one (a code exchange can
/// take 30 seconds) holds up nobody else, but one member's commands run one
/// at a time in the order they arrived: `logout` then `login` never swaps.
///
/// # Errors
///
/// The error that ended the connection for good, such as
/// [`SurfaceError::Unauthorized`] for a rejected token.
pub async fn serve(
    surface: Arc<dyn Surface>,
    binding: Binding,
    commands: Commands,
    mut stopping: watch::Receiver<bool>,
) -> Result<(), SurfaceError> {
    let (tx, mut rx) = mpsc::channel(EVENT_BUFFER);
    let mut events = surface.events(&binding, Sender::new(Forward(tx)));
    let mut running = JoinSet::new();
    let mut last_of: HashMap<MemberKey, oneshot::Receiver<()>> = HashMap::new();
    let mut run = |running: &mut JoinSet<()>, event: InboundEvent| {
        let Some((origin, text)) = command_in(&event, &binding) else {
            return;
        };
        let commands = commands.clone();
        let member = event.sender.clone();
        let text = text.to_owned();
        last_of.retain(|_, finished| {
            matches!(
                finished.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            )
        });
        let (done, finished) = oneshot::channel();
        let previous = last_of.insert(member.clone(), finished);
        running.spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            commands.handle_text(&member, &text, &origin).await;
            let _ = done.send(());
        });
    };
    let ended = loop {
        tokio::select! {
            biased;
            _ = stopping.wait_for(|stop| *stop) => break Ok(()),
            Some(event) = rx.recv() => run(&mut running, event),
            Some(joined) = running.join_next() => log_panic(joined),
            ended = &mut events => break ended,
        }
    };
    drop(events);
    while let Ok(event) = rx.try_recv() {
        run(&mut running, event);
    }
    while let Some(joined) = running.join_next().await {
        log_panic(joined);
    }
    ended
}

fn log_panic(joined: Result<(), tokio::task::JoinError>) {
    if let Err(err) = joined {
        tracing::error!(error = %err, "a command task panicked");
    }
}

struct Forward(mpsc::Sender<InboundEvent>);

#[async_trait]
impl Sink<InboundEvent> for Forward {
    async fn send(&self, item: InboundEvent) -> Result<(), SendError> {
        self.0.send(item).await.map_err(|_| SendError)
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
