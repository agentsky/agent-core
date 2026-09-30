//! The per-turn outbox: what agentctl queued for the turn pipeline to
//! deliver after the turn.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use core_types::{MsgRef, OutFile, ReplyTarget, TurnId};

/// The most files one turn may attach.
pub const MAX_ATTACHMENTS: usize = 10;
/// The most messages one turn may queue with `agentctl post`.
pub const MAX_POSTS: usize = 10;
/// The most reactions one turn may queue with `agentctl react`.
pub const MAX_REACTIONS: usize = 20;

/// A message queued with `agentctl post`. Its target already passed the
/// turn's target rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedPost {
    /// Where to post.
    pub to: ReplyTarget,
    /// The Markdown text to render and post.
    pub text: String,
}

/// A reaction queued with `agentctl react`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedReaction {
    /// The message to react to.
    pub msg: MsgRef,
    /// The emoji name, without colons.
    pub emoji: String,
}

/// What one turn queued through agentctl, handed to the turn pipeline by
/// [`Ctl::end_turn`](super::Ctl::end_turn).
///
/// Attachments are staged in a directory of their own under the data
/// directory. Dropping the outbox deletes that directory, so the pipeline
/// uploads the files and then drops it.
#[derive(Debug)]
pub struct Outbox {
    turn: TurnId,
    staging: PathBuf,
    attachments: Vec<OutFile>,
    posts: Vec<QueuedPost>,
    reactions: Vec<QueuedReaction>,
}

impl Outbox {
    /// An empty outbox whose attachments go in `staging`, which must exist.
    pub(crate) fn new(turn: TurnId, staging: PathBuf) -> Self {
        Self {
            turn,
            staging,
            attachments: Vec::new(),
            posts: Vec::new(),
            reactions: Vec::new(),
        }
    }

    /// The turn it belongs to.
    pub fn turn(&self) -> TurnId {
        self.turn
    }

    /// The staged files, in the order they were attached.
    pub fn attachments(&self) -> &[OutFile] {
        &self.attachments
    }

    /// The queued messages, in the order they were posted.
    pub fn posts(&self) -> &[QueuedPost] {
        &self.posts
    }

    /// The queued reactions, in order.
    pub fn reactions(&self) -> &[QueuedReaction] {
        &self.reactions
    }

    /// Whether nothing was queued.
    pub fn is_empty(&self) -> bool {
        self.attachments.is_empty() && self.posts.is_empty() && self.reactions.is_empty()
    }

    pub(crate) fn staging(&self) -> &Path {
        &self.staging
    }

    pub(crate) fn attachments_len(&self) -> usize {
        self.attachments.len()
    }

    pub(crate) fn push_attachment(&mut self, file: OutFile) {
        self.attachments.push(file);
    }

    /// Queues `post`, or returns false when the turn has queued
    /// [`MAX_POSTS`].
    pub(crate) fn push_post(&mut self, post: QueuedPost) -> bool {
        push_capped(&mut self.posts, post, MAX_POSTS)
    }

    /// Queues `reaction`, or returns false when the turn has queued
    /// [`MAX_REACTIONS`].
    pub(crate) fn push_reaction(&mut self, reaction: QueuedReaction) -> bool {
        push_capped(&mut self.reactions, reaction, MAX_REACTIONS)
    }
}

fn push_capped<T>(items: &mut Vec<T>, item: T, max: usize) -> bool {
    if items.len() >= max {
        return false;
    }
    items.push(item);
    true
}

impl Drop for Outbox {
    fn drop(&mut self) {
        remove_dir(&self.staging);
    }
}

/// Removes `dir` and everything in it, logging a failure other than its
/// absence.
pub(crate) fn remove_dir(dir: &Path) {
    match fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => tracing::warn!(error = %err, "removing an agentctl staging directory failed"),
    }
}

/// Creates `dir`, and its missing parents, readable by agentd's user only.
pub(crate) fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

#[cfg(test)]
mod tests {
    use core_types::{ConvRef, MessageId, SurfaceKind};

    use super::*;

    fn conv() -> ConvRef {
        ConvRef {
            surface: SurfaceKind::Slack,
            team: "T1".into(),
            conversation: "C1".into(),
        }
    }

    #[test]
    fn dropping_the_outbox_deletes_its_staging_directory() {
        let dir = std::env::temp_dir().join(format!("agentd-outbox-{}", uuid_like()));
        create_private_dir(&dir).unwrap();
        fs::write(dir.join("file"), b"x").unwrap();
        let outbox = Outbox::new(TurnId::new_v4(), dir.clone());
        assert!(outbox.is_empty());
        drop(outbox);
        assert!(!dir.exists());
        remove_dir(&dir);
    }

    #[test]
    fn queues_are_capped() {
        let dir = std::env::temp_dir().join(format!("agentd-outbox-{}", uuid_like()));
        let mut outbox = Outbox::new(TurnId::new_v4(), dir);
        for _ in 0..MAX_POSTS {
            assert!(outbox.push_post(QueuedPost {
                to: ReplyTarget {
                    conv: conv(),
                    thread_root: None,
                },
                text: "x".into(),
            }));
        }
        assert!(!outbox.push_post(QueuedPost {
            to: ReplyTarget {
                conv: conv(),
                thread_root: None,
            },
            text: "x".into(),
        }));
        for _ in 0..MAX_REACTIONS {
            assert!(outbox.push_reaction(QueuedReaction {
                msg: MsgRef {
                    conv: conv(),
                    id: MessageId::new("1"),
                },
                emoji: "eyes".into(),
            }));
        }
        assert!(!outbox.push_reaction(QueuedReaction {
            msg: MsgRef {
                conv: conv(),
                id: MessageId::new("1"),
            },
            emoji: "eyes".into(),
        }));
        assert_eq!(outbox.posts().len(), MAX_POSTS);
        assert_eq!(outbox.reactions().len(), MAX_REACTIONS);
        assert!(!outbox.is_empty());
    }

    fn uuid_like() -> String {
        TurnId::new_v4().to_string()
    }
}
