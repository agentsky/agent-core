//! [`Acknowledge`]: what happens to messages until agents take turns.

use async_trait::async_trait;
use core_types::{
    AgentId, BindingId, ConvKind, InboundEvent, MemberKey, SendError, Sink, SurfaceError,
};
use store::{AgentState, StoreError};
use surface_rocketchat::rest::Credentials;

use super::RocketChatAgents;

/// The reaction an agent's bot adds to a message addressed to it.
pub const ACK_EMOJI: &str = "eyes";

/// Where every Rocket.Chat connection passes the messages that aren't
/// commands, until the turn pipeline (T23) takes its place.
///
/// For each message a person sent, every active agent it addresses reacts
/// with [`ACK_EMOJI`] as its own bot: each agent it mentions, and in a
/// direct message the agent whose bot is in it. That shows which bots a
/// mention reaches. Messages from bots, the manager bot and agents
/// included, are ignored, and so are paused agents.
///
/// It reacts rather than replies: `chat.postMessage` makes a bot join a
/// public channel it isn't in, and a mention of an agent that isn't in the
/// room must not pull it in.
#[derive(Debug, Clone)]
pub struct Acknowledge {
    agents: RocketChatAgents,
    manager: MemberKey,
}

impl Acknowledge {
    /// Acknowledges for `agents`, ignoring the manager bot `manager`.
    pub fn new(agents: RocketChatAgents, manager: MemberKey) -> Self {
        Self { agents, manager }
    }

    /// The agents `event` addresses, with the binding each is addressed
    /// through: those it mentions, then, in a direct message, the one whose
    /// bot received it. Each appears once.
    async fn addressed(
        &self,
        event: &InboundEvent,
    ) -> Result<Vec<(AgentId, BindingId, AgentState)>, StoreError> {
        let store = self.agents.store();
        if event.sender_is_bot
            || event.sender_bot_user.is_some()
            || event.sender == self.manager
            || store.agent_for_bot(&event.sender).await?.is_some()
        {
            return Ok(Vec::new());
        }
        let mut addressed = Vec::new();
        for user in &event.mentions {
            let bot = MemberKey {
                surface: event.conv.surface,
                team: event.conv.team.clone(),
                user: user.clone(),
            };
            if let Some((agent, binding)) = store.agent_for_bot(&bot).await? {
                addressed.push((agent.id, binding, agent.state));
            }
        }
        if event.conv_kind == ConvKind::Dm
            && let Some(agent) = store.agent_for_binding(event.binding).await?
        {
            addressed.push((agent.id, event.binding, agent.state));
        }
        let mut seen = Vec::new();
        addressed.retain(|(agent, _, _)| {
            let first = !seen.contains(agent);
            seen.push(*agent);
            first
        });
        Ok(addressed)
    }

    async fn react(&self, event: &InboundEvent, binding: BindingId) -> Result<(), AckError> {
        let store = self.agents.store();
        let (Some(bot), Some(token)) = (
            store.binding(binding).await?.and_then(|b| b.bot_user),
            store.bot_token(binding).await?,
        ) else {
            return Ok(());
        };
        let rest = self.agents.as_bot(Credentials {
            user_id: bot,
            token,
        });
        rest.react(&event.message.id, ACK_EMOJI).await?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
enum AckError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Surface(#[from] SurfaceError),
}

#[async_trait]
impl Sink<InboundEvent> for Acknowledge {
    async fn send(&self, event: InboundEvent) -> Result<(), SendError> {
        let addressed = match self.addressed(&event).await {
            Ok(addressed) => addressed,
            Err(err) => {
                tracing::warn!(message = %event.message.id, error = %err, "couldn't look up the agents a message addresses");
                return Ok(());
            }
        };
        for (agent, binding, state) in addressed {
            if state != AgentState::Active {
                tracing::debug!(%agent, message = %event.message.id, "ignoring a message for a paused agent");
                continue;
            }
            if let Err(err) = self.react(&event, binding).await {
                tracing::warn!(%agent, message = %event.message.id, error = %err, "couldn't acknowledge a message");
            }
        }
        Ok(())
    }
}
