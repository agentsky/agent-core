//! [`CommandIntake`]: runs the commands every surface hears, whichever
//! connection or request delivered them.
//!
//! Rocket.Chat connections submit through a
//! [`CommandFeed`](super::rocketchat::CommandFeed), and the Slack queue
//! through the [`CommandSubmitter`] it holds. Each command runs in its own
//! task, so a slow one (a code exchange can take 30 seconds) holds up nobody
//! else, but one member's commands run one at a time in the order they
//! arrived: `logout` then `login` never swaps.

use std::collections::HashMap;
use std::fmt;

use core_types::{InFile, MemberKey, SendError};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

use super::{Commands, Origin};

/// How many commands may wait between the surfaces and the intake.
const COMMAND_BUFFER: usize = 64;

/// A command a surface heard, on its way to the intake.
struct Heard {
    member: MemberKey,
    text: String,
    origin: Origin,
    files: Vec<InFile>,
}

/// Runs the commands that every surface submits.
pub struct CommandIntake {
    commands: Commands,
    rx: mpsc::Receiver<Heard>,
}

impl fmt::Debug for CommandIntake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandIntake").finish_non_exhaustive()
    }
}

/// Hands commands to the intake. Clone it for each surface.
///
/// The intake runs until every submitter is dropped.
#[derive(Clone)]
pub struct CommandSubmitter {
    tx: mpsc::Sender<Heard>,
}

impl fmt::Debug for CommandSubmitter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandSubmitter").finish_non_exhaustive()
    }
}

impl CommandIntake {
    /// An intake running `commands`, and the first submitter into it.
    pub fn new(commands: Commands) -> (Self, CommandSubmitter) {
        let (tx, rx) = mpsc::channel(COMMAND_BUFFER);
        (Self { commands, rx }, CommandSubmitter { tx })
    }

    /// Runs every command submitted, until every submitter is dropped, as
    /// the surfaces holding them stop. Then it runs the commands already
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

impl CommandSubmitter {
    /// Hands `text` from `member`, sent from `origin` with `files`
    /// attached, to the intake, which parses and runs it.
    ///
    /// # Errors
    ///
    /// [`SendError`] if the intake has stopped.
    pub async fn submit(
        &self,
        member: MemberKey,
        text: String,
        origin: Origin,
        files: Vec<InFile>,
    ) -> Result<(), SendError> {
        let heard = Heard {
            member,
            text,
            origin,
            files,
        };
        self.tx.send(heard).await.map_err(|_| SendError)
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
            .handle_text(&heard.member, &heard.text, &heard.origin, &heard.files)
            .await;
        let _ = done.send(());
    });
}

fn log_panic(joined: Result<(), tokio::task::JoinError>) {
    if let Err(err) = joined {
        tracing::error!(error = %err, "a command task panicked");
    }
}
