//! [`MockSurface`]: a [`Surface`] that records what the shared core does
//! with it.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use core_types::surface_trait::Result;
use core_types::{
    Binding, BindingId, Caps, Cursor, InboundEvent, LengthUnit, Limit, MessageId, Msg, MsgRef,
    OutFile, ReplyTarget, Sender, Surface, SurfaceError, ThreadKey,
};
use tokio::sync::Notify;

/// One call the shared core made on a [`MockSurface`], in the order made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// [`Surface::post`], with the reference the mock returned.
    Post {
        /// Where the message went.
        to: ReplyTarget,
        /// The text posted.
        text: String,
        /// The reference returned to the caller.
        msg: MsgRef,
    },
    /// [`Surface::edit`].
    Edit {
        /// The message edited.
        msg: MsgRef,
        /// Its new text.
        text: String,
    },
    /// [`Surface::react`].
    React {
        /// The message reacted to.
        msg: MsgRef,
        /// The emoji name, without colons.
        emoji: String,
    },
    /// [`Surface::upload`].
    Upload {
        /// Where the files went.
        to: ReplyTarget,
        /// The files, with their contents read at upload time.
        files: Vec<UploadedFile>,
    },
    /// [`Surface::history`].
    History {
        /// The thread read.
        thread: ThreadKey,
        /// The cursor passed.
        before: Option<Cursor>,
        /// The limit passed.
        limit: usize,
    },
}

/// A [`Surface`] operation that [`MockSurface::fail_next`] can make fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    /// [`Surface::post`].
    Post,
    /// [`Surface::edit`].
    Edit,
    /// [`Surface::react`].
    React,
    /// [`Surface::upload`].
    Upload,
    /// [`Surface::history`].
    History,
}

/// A file passed to [`Surface::upload`], as the mock saw it.
///
/// The contents are read when `upload` is called, so a caller that deletes
/// staged files afterwards can still be checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedFile {
    /// The file as passed.
    pub file: OutFile,
    /// Its contents at upload time.
    pub contents: Vec<u8>,
}

/// A [`Surface`] for tests.
///
/// - Every successful [`post`](Surface::post), [`edit`](Surface::edit),
///   [`react`](Surface::react), [`upload`](Surface::upload) and
///   [`history`](Surface::history) call is recorded; read them with
///   [`calls`](Self::calls) or [`posts`](Self::posts). A call that fails
///   is not recorded.
/// - Calls honor its [`Caps`]: without
///   [`supports_edit`](Caps::supports_edit), `edit` fails with
///   [`SurfaceError::Unsupported`]`("edit")`, and without
///   [`supports_threads`](Caps::supports_threads), a `post`, `upload` or
///   `history` in a thread (a target or key with a root) fails with
///   [`SurfaceError::Unsupported`]`("threads")`.
/// - [`fail_next`](Self::fail_next) makes the next call of an operation
///   fail with a given error, such as [`SurfaceError::RateLimited`] or
///   [`SurfaceError::Unauthorized`], as the platform would.
/// - `post` returns references `m1`, `m2`, … in the target conversation.
/// - `history` serves what [`set_history`](Self::set_history) stored for
///   the thread, honoring `before` and `limit` as the trait documents. A
///   thread with nothing stored has no messages.
/// - [`inject`](Self::inject) queues an event for the
///   [`events`](Surface::events) loop of the event's binding. Events
///   injected before the loop starts wait for it.
/// - [`render`](Surface::render) doesn't convert anything. It splits the
///   text into chunks that fit [`Caps::message_limit`].
///
/// Share it with the code under test through an `Arc`:
///
/// ```
/// use std::sync::Arc;
/// use core_types::Surface;
/// use testkit::MockSurface;
///
/// let mock = Arc::new(MockSurface::new());
/// let surface: Arc<dyn Surface> = mock.clone();
/// assert!(surface.caps().supports_threads);
/// assert!(mock.calls().is_empty());
/// ```
#[derive(Debug)]
pub struct MockSurface {
    caps: Caps,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    calls: Vec<Call>,
    posted: u64,
    history: HashMap<ThreadKey, Vec<Msg>>,
    failures: HashMap<Op, VecDeque<SurfaceError>>,
    queues: HashMap<BindingId, Queue>,
}

/// The events injected for one binding and not yet delivered.
#[derive(Debug, Default)]
struct Queue {
    events: VecDeque<InboundEvent>,
    closed: bool,
    running: bool,
    changed: Arc<Notify>,
}

impl Default for MockSurface {
    fn default() -> Self {
        Self::new()
    }
}

impl MockSurface {
    /// The capabilities [`MockSurface::new`] uses: a 4,000-character limit,
    /// edits and threads, no buttons, and one copy of each event.
    pub const DEFAULT_CAPS: Caps = Caps {
        message_limit: Limit {
            max: 4000,
            unit: LengthUnit::Chars,
        },
        supports_edit: true,
        supports_buttons: false,
        supports_threads: true,
        per_binding_delivery: false,
    };

    /// A mock with [`DEFAULT_CAPS`](Self::DEFAULT_CAPS).
    pub fn new() -> Self {
        Self::with_caps(Self::DEFAULT_CAPS)
    }

    /// A mock that reports `caps`.
    pub fn with_caps(caps: Caps) -> Self {
        Self {
            caps,
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every call recorded so far, oldest first.
    pub fn calls(&self) -> Vec<Call> {
        self.state().calls.clone()
    }

    /// The target and text of every [`Surface::post`] so far, oldest first.
    pub fn posts(&self) -> Vec<(ReplyTarget, String)> {
        self.state()
            .calls
            .iter()
            .filter_map(|call| match call {
                Call::Post { to, text, .. } => Some((to.clone(), text.clone())),
                _ => None,
            })
            .collect()
    }

    /// Stores the messages [`Surface::history`] serves for `thread`, oldest
    /// first, replacing what was there.
    pub fn set_history(&self, thread: ThreadKey, messages: Vec<Msg>) {
        self.state().history.insert(thread, messages);
    }

    /// Makes the next call of `op` fail with `error`, without recording
    /// it. Failures queued for one operation are used in order, one per
    /// call. A call the mock's [`Caps`] refuse fails with
    /// [`SurfaceError::Unsupported`] instead and leaves the queue as it is,
    /// as a real surface refuses it before asking the platform.
    pub fn fail_next(&self, op: Op, error: SurfaceError) {
        self.state()
            .failures
            .entry(op)
            .or_default()
            .push_back(error);
    }

    /// Queues `event` for the [`Surface::events`] loop of `event.binding`.
    ///
    /// # Panics
    ///
    /// If [`close_events`](Self::close_events) was called for that binding.
    pub fn inject(&self, event: InboundEvent) {
        let mut state = self.state();
        let queue = state.queues.entry(event.binding).or_default();
        assert!(
            !queue.closed,
            "events for binding {} were closed",
            event.binding
        );
        queue.events.push_back(event);
        queue.changed.notify_one();
    }

    /// Ends the [`Surface::events`] loop of `binding` once the events
    /// already injected are delivered, as a connection that ends for good
    /// would. The loop then returns `Ok(())`.
    pub fn close_events(&self, binding: BindingId) {
        let mut state = self.state();
        let queue = state.queues.entry(binding).or_default();
        queue.closed = true;
        queue.changed.notify_one();
    }

    /// Locks the state for a call of `op`: fails if the caps refuse it
    /// (`thread` is the root it posts or reads in) or a failure is queued.
    fn begin(&self, op: Op, thread: Option<&MessageId>) -> Result<MutexGuard<'_, State>> {
        if op == Op::Edit && !self.caps.supports_edit {
            return Err(SurfaceError::Unsupported("edit"));
        }
        if thread.is_some() && !self.caps.supports_threads {
            return Err(SurfaceError::Unsupported("threads"));
        }
        let mut state = self.state();
        match state.failures.get_mut(&op).and_then(VecDeque::pop_front) {
            Some(error) => Err(error),
            None => Ok(state),
        }
    }
}

#[async_trait]
impl Surface for MockSurface {
    /// Delivers injected events for `binding` to `tx`, until
    /// [`MockSurface::close_events`] or until `tx` is closed.
    ///
    /// Only one loop per binding runs at a time; a second one fails with
    /// [`SurfaceError::Api`]. An event leaves the queue only once `tx` has
    /// taken it, so when a loop ends, fails or is cancelled, every event
    /// not delivered stays for the next loop.
    async fn events(&self, binding: &Binding, tx: Sender<InboundEvent>) -> Result<()> {
        let changed = {
            let mut state = self.state();
            let queue = state.queues.entry(binding.id).or_default();
            if queue.running {
                return Err(SurfaceError::Api(format!(
                    "events for binding {} are already running",
                    binding.id
                )));
            }
            queue.running = true;
            Arc::clone(&queue.changed)
        };
        let _running = Running {
            mock: self,
            binding: binding.id,
        };
        loop {
            let next = {
                let mut state = self.state();
                let queue = state.queues.entry(binding.id).or_default();
                match queue.events.front() {
                    Some(event) => Some(event.clone()),
                    None if queue.closed => return Ok(()),
                    None => None,
                }
            };
            match next {
                Some(event) => {
                    tx.send(event).await?;
                    if let Some(queue) = self.state().queues.get_mut(&binding.id) {
                        queue.events.pop_front();
                    }
                }
                None => changed.notified().await,
            }
        }
    }

    async fn post(&self, to: &ReplyTarget, text: &str) -> Result<MsgRef> {
        let mut state = self.begin(Op::Post, to.thread_root.as_ref())?;
        state.posted += 1;
        let msg = MsgRef {
            conv: to.conv.clone(),
            id: MessageId::new(format!("m{}", state.posted)),
        };
        state.calls.push(Call::Post {
            to: to.clone(),
            text: text.to_owned(),
            msg: msg.clone(),
        });
        Ok(msg)
    }

    async fn edit(&self, msg: &MsgRef, text: &str) -> Result<()> {
        self.begin(Op::Edit, None)?.calls.push(Call::Edit {
            msg: msg.clone(),
            text: text.to_owned(),
        });
        Ok(())
    }

    async fn react(&self, msg: &MsgRef, emoji: &str) -> Result<()> {
        self.begin(Op::React, None)?.calls.push(Call::React {
            msg: msg.clone(),
            emoji: emoji.to_owned(),
        });
        Ok(())
    }

    /// Reads every file and records the upload. A file that can't be read
    /// fails the call with [`SurfaceError::NotFound`], and nothing is
    /// recorded.
    async fn upload(&self, to: &ReplyTarget, files: &[OutFile]) -> Result<()> {
        drop(self.begin(Op::Upload, to.thread_root.as_ref())?);
        let mut uploaded = Vec::with_capacity(files.len());
        for file in files {
            let contents = tokio::fs::read(&file.path)
                .await
                .map_err(|err| SurfaceError::NotFound(format!("{}: {err}", file.name)))?;
            uploaded.push(UploadedFile {
                file: file.clone(),
                contents,
            });
        }
        self.state().calls.push(Call::Upload {
            to: to.clone(),
            files: uploaded,
        });
        Ok(())
    }

    /// Serves the stored messages. A `before` cursor that names no stored
    /// message fails with [`SurfaceError::NotFound`].
    async fn history(
        &self,
        thread: &ThreadKey,
        before: Option<Cursor>,
        limit: usize,
    ) -> Result<Vec<Msg>> {
        let mut state = self.begin(Op::History, thread.root.as_ref())?;
        let messages = state.history.get(thread).map_or(&[][..], Vec::as_slice);
        let end = match &before {
            None => messages.len(),
            Some(cursor) => messages
                .iter()
                .position(|msg| msg.id.as_str() == cursor.as_str())
                .ok_or_else(|| SurfaceError::NotFound("history cursor".into()))?,
        };
        let page = messages[end.saturating_sub(limit)..end].to_vec();
        state.calls.push(Call::History {
            thread: thread.clone(),
            before,
            limit,
        });
        Ok(page)
    }

    fn render(&self, markdown: &str) -> Vec<String> {
        split(markdown, self.caps.message_limit)
    }

    fn caps(&self) -> Caps {
        self.caps
    }
}

/// Marks a binding's [`Surface::events`] loop as running until the loop
/// ends or is cancelled.
struct Running<'a> {
    mock: &'a MockSurface,
    binding: BindingId,
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        if let Some(queue) = self.mock.state().queues.get_mut(&self.binding) {
            queue.running = false;
        }
    }
}

/// Splits `text` into chunks of at most `limit`, on character boundaries. A
/// character wider than the limit gets a chunk of its own.
fn split(text: &str, limit: Limit) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    let mut len = 0;
    for ch in text.chars() {
        let width = match limit.unit {
            LengthUnit::Chars => 1,
            LengthUnit::Utf16 => ch.len_utf16(),
        };
        if len + width > limit.max && !chunk.is_empty() {
            chunks.push(std::mem::take(&mut chunk));
            len = 0;
        }
        chunk.push(ch);
        len += width;
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use core_types::{AgentId, ConvKind, ConvRef, MemberKey, SendError, Sink, SurfaceKind};
    use time::macros::datetime;
    use tokio::sync::mpsc;

    use super::*;

    struct Forward(mpsc::Sender<InboundEvent>);

    #[async_trait]
    impl Sink<InboundEvent> for Forward {
        async fn send(&self, item: InboundEvent) -> std::result::Result<(), SendError> {
            self.0.send(item).await.map_err(|_| SendError)
        }
    }

    fn conv(id: &str) -> ConvRef {
        ConvRef {
            surface: SurfaceKind::Slack,
            team: "T1".into(),
            conversation: id.into(),
        }
    }

    fn member(user: &str) -> MemberKey {
        MemberKey {
            surface: SurfaceKind::Slack,
            team: "T1".into(),
            user: user.into(),
        }
    }

    fn binding() -> Binding {
        Binding {
            id: BindingId::new_v4(),
            agent: Some(AgentId::new_v4()),
            bot: member("UBOT"),
        }
    }

    fn event(binding: &Binding, text: &str) -> InboundEvent {
        InboundEvent {
            event_id: format!("Ev-{text}"),
            binding: binding.id,
            sender: member("U1"),
            sender_is_bot: false,
            sender_bot_user: None,
            conv: conv("C1"),
            conv_kind: ConvKind::Channel,
            thread_root: None,
            message: MsgRef {
                conv: conv("C1"),
                id: "1.1".into(),
            },
            text: text.into(),
            mentions: vec![],
            reply_to: None,
            files: vec![],
            received_at: datetime!(2026-09-30 00:00 UTC),
        }
    }

    fn msg(id: &str) -> Msg {
        Msg {
            id: id.into(),
            sender: member("U1"),
            sender_is_bot: false,
            text: format!("text {id}"),
            files: vec![],
            sent_at: datetime!(2026-09-30 00:00 UTC),
        }
    }

    fn target(conversation: &str, root: Option<&str>) -> ReplyTarget {
        ReplyTarget {
            conv: conv(conversation),
            thread_root: root.map(MessageId::from),
        }
    }

    fn ids(messages: &[Msg]) -> Vec<&str> {
        messages.iter().map(|m| m.id.as_str()).collect()
    }

    #[tokio::test]
    async fn every_call_is_logged_in_order() {
        let dir = std::env::temp_dir().join(format!("testkit-upload-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, b"attached").unwrap();
        let file = OutFile {
            name: "a.txt".into(),
            path: path.clone(),
        };

        let mock = MockSurface::new();
        let to = target("C1", Some("1.0"));
        mock.upload(&to, std::slice::from_ref(&file)).await.unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let first = mock.post(&to, "hello").await.unwrap();
        let second = mock.post(&target("C2", None), "other").await.unwrap();
        mock.edit(&first, "hello again").await.unwrap();
        mock.react(&second, "eyes").await.unwrap();

        assert_eq!(first.id.as_str(), "m1");
        assert_eq!(first.conv, conv("C1"));
        assert_eq!(second.id.as_str(), "m2");
        assert_eq!(second.conv, conv("C2"));
        assert_eq!(
            mock.calls(),
            [
                Call::Upload {
                    to: to.clone(),
                    files: vec![UploadedFile {
                        file,
                        contents: b"attached".to_vec(),
                    }],
                },
                Call::Post {
                    to: to.clone(),
                    text: "hello".into(),
                    msg: first.clone(),
                },
                Call::Post {
                    to: target("C2", None),
                    text: "other".into(),
                    msg: second.clone(),
                },
                Call::Edit {
                    msg: first,
                    text: "hello again".into(),
                },
                Call::React {
                    msg: second,
                    emoji: "eyes".into(),
                },
            ]
        );
        assert_eq!(
            mock.posts(),
            [
                (to, "hello".to_owned()),
                (target("C2", None), "other".to_owned())
            ]
        );
    }

    #[tokio::test]
    async fn uploading_a_missing_file_fails_and_is_not_logged() {
        let mock = MockSurface::new();
        let file = OutFile {
            name: "gone.txt".into(),
            path: std::env::temp_dir().join(format!("testkit-missing-{}", uuid::Uuid::new_v4())),
        };
        let err = mock.upload(&target("C1", None), &[file]).await.unwrap_err();
        assert!(matches!(err, SurfaceError::NotFound(ref what) if what.starts_with("gone.txt")));
        assert!(mock.calls().is_empty());
    }

    #[tokio::test]
    async fn history_serves_the_newest_messages_before_the_cursor_oldest_first() {
        let mock = MockSurface::new();
        let thread = ThreadKey {
            conv: conv("C1"),
            root: Some("1".into()),
        };
        mock.set_history(thread.clone(), ["1", "2", "3", "4", "5"].map(msg).to_vec());

        let all = mock.history(&thread, None, 10).await.unwrap();
        assert_eq!(ids(&all), ["1", "2", "3", "4", "5"]);
        let newest = mock.history(&thread, None, 2).await.unwrap();
        assert_eq!(ids(&newest), ["4", "5"]);
        let before = mock
            .history(&thread, Some(Cursor::new("4")), 2)
            .await
            .unwrap();
        assert_eq!(ids(&before), ["2", "3"]);
        let first = mock
            .history(&thread, Some(Cursor::new("1")), 2)
            .await
            .unwrap();
        assert!(first.is_empty());
        assert_eq!(
            mock.history(&thread, Some(Cursor::new("9")), 2).await,
            Err(SurfaceError::NotFound("history cursor".into()))
        );
        assert_eq!(mock.calls().len(), 4, "a failed call is not recorded");

        let other = ThreadKey {
            conv: conv("C1"),
            root: None,
        };
        assert!(mock.history(&other, None, 5).await.unwrap().is_empty());
        assert_eq!(
            mock.calls().last(),
            Some(&Call::History {
                thread: other,
                before: None,
                limit: 5,
            })
        );
        assert_eq!(mock.calls().len(), 5);
    }

    #[tokio::test]
    async fn injected_events_reach_the_events_loop_of_their_binding() {
        let mock = Arc::new(MockSurface::new());
        let (a, b) = (binding(), binding());
        mock.inject(event(&a, "early"));

        let (tx, mut rx) = mpsc::channel(8);
        let task = tokio::spawn({
            let mock = Arc::clone(&mock);
            let a = a.clone();
            async move { mock.events(&a, Sender::new(Forward(tx))).await }
        });
        mock.inject(event(&b, "for b"));
        mock.inject(event(&a, "late"));

        assert_eq!(rx.recv().await.unwrap().text, "early");
        assert_eq!(rx.recv().await.unwrap().text, "late");
        mock.close_events(a.id);
        assert_eq!(task.await.unwrap(), Ok(()));
        assert!(rx.recv().await.is_none());

        let (tx, mut rx) = mpsc::channel(8);
        mock.close_events(b.id);
        mock.events(&b, Sender::new(Forward(tx))).await.unwrap();
        assert_eq!(rx.recv().await.unwrap().text, "for b");
    }

    #[tokio::test]
    async fn events_end_with_closed_when_the_receiver_is_dropped_and_lose_nothing() {
        let mock = MockSurface::new();
        let a = binding();
        mock.inject(event(&a, "refused"));
        mock.inject(event(&a, "queued"));
        let (tx, rx) = mpsc::channel(8);
        drop(rx);
        assert_eq!(
            mock.events(&a, Sender::new(Forward(tx))).await,
            Err(SurfaceError::Closed)
        );

        let (tx, mut rx) = mpsc::channel(8);
        mock.close_events(a.id);
        mock.events(&a, Sender::new(Forward(tx))).await.unwrap();
        assert_eq!(rx.recv().await.unwrap().text, "refused");
        assert_eq!(rx.recv().await.unwrap().text, "queued");
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn a_second_events_loop_for_one_binding_is_refused() {
        let mock = Arc::new(MockSurface::new());
        let a = binding();
        let (tx, _rx) = mpsc::channel(8);
        let running = tokio::spawn({
            let mock = Arc::clone(&mock);
            let a = a.clone();
            async move { mock.events(&a, Sender::new(Forward(tx))).await }
        });
        while mock.state().queues.get(&a.id).is_none_or(|q| !q.running) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let (tx, _rx2) = mpsc::channel(8);
        let second = mock.events(&a, Sender::new(Forward(tx))).await;
        assert!(matches!(second, Err(SurfaceError::Api(_))));
        mock.close_events(a.id);
        assert_eq!(running.await.unwrap(), Ok(()));
    }

    #[test]
    #[should_panic(expected = "were closed")]
    fn injecting_after_close_panics() {
        let mock = MockSurface::new();
        let a = binding();
        mock.close_events(a.id);
        mock.inject(event(&a, "late"));
    }

    #[test]
    fn caps_are_configurable_and_render_splits_to_the_limit() {
        let mock = MockSurface::default();
        assert_eq!(mock.caps(), MockSurface::DEFAULT_CAPS);
        assert_eq!(mock.render("**hi**"), ["**hi**"]);
        assert!(mock.render("").is_empty());

        let caps = Caps {
            message_limit: Limit {
                max: 4,
                unit: LengthUnit::Utf16,
            },
            supports_edit: false,
            supports_buttons: true,
            supports_threads: false,
            per_binding_delivery: true,
        };
        let mock = MockSurface::with_caps(caps);
        assert_eq!(mock.caps(), caps);
        assert_eq!(mock.render("abcdefg"), ["abcd", "efg"]);
        assert_eq!(mock.render("ab😀😀"), ["ab😀", "😀"]);

        let chars = MockSurface::with_caps(Caps {
            message_limit: Limit {
                max: 2,
                unit: LengthUnit::Chars,
            },
            ..caps
        });
        assert_eq!(chars.render("😀😀😀"), ["😀😀", "😀"]);

        let tiny = MockSurface::with_caps(Caps {
            message_limit: Limit {
                max: 1,
                unit: LengthUnit::Utf16,
            },
            ..caps
        });
        assert_eq!(tiny.render("a😀b"), ["a", "😀", "b"]);
    }

    #[tokio::test]
    async fn caps_refuse_edits_and_threads_the_surface_does_not_support() {
        let mock = MockSurface::with_caps(Caps {
            supports_edit: false,
            supports_threads: false,
            ..MockSurface::DEFAULT_CAPS
        });
        let thread = target("C1", Some("1.0"));
        let top = target("C1", None);
        let file = OutFile {
            name: "unread.txt".into(),
            path: std::env::temp_dir().join(format!("testkit-unread-{}", uuid::Uuid::new_v4())),
        };
        let threads = SurfaceError::Unsupported("threads");

        assert_eq!(
            mock.post(&thread, "in a thread").await,
            Err(threads.clone())
        );
        assert_eq!(mock.upload(&thread, &[file]).await, Err(threads.clone()));
        let key = ThreadKey {
            conv: conv("C1"),
            root: Some("1.0".into()),
        };
        assert_eq!(mock.history(&key, None, 5).await, Err(threads));
        let posted = mock.post(&top, "top level").await.unwrap();
        assert_eq!(posted.id.as_str(), "m1");
        assert_eq!(
            mock.edit(&posted, "edited").await,
            Err(SurfaceError::Unsupported("edit"))
        );
        mock.react(&posted, "eyes").await.unwrap();
        assert_eq!(
            mock.calls(),
            [
                Call::Post {
                    to: top,
                    text: "top level".into(),
                    msg: posted.clone(),
                },
                Call::React {
                    msg: posted,
                    emoji: "eyes".into(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn fail_next_fails_the_next_calls_of_one_operation_in_order() {
        let mock = MockSurface::with_caps(Caps {
            supports_edit: false,
            ..MockSurface::DEFAULT_CAPS
        });
        let to = target("C1", Some("1.0"));
        let limited = SurfaceError::RateLimited {
            retry_after: Duration::from_secs(3),
        };
        mock.fail_next(Op::Post, limited.clone());
        mock.fail_next(Op::Post, SurfaceError::Forbidden("not_in_channel".into()));
        mock.fail_next(Op::Edit, SurfaceError::Unauthorized);
        mock.fail_next(Op::React, SurfaceError::Unauthorized);
        mock.fail_next(Op::Upload, SurfaceError::Transport("reset".into()));
        mock.fail_next(Op::History, SurfaceError::Api("ratelimited".into()));

        assert_eq!(mock.post(&to, "a").await, Err(limited));
        assert_eq!(
            mock.post(&to, "b").await,
            Err(SurfaceError::Forbidden("not_in_channel".into()))
        );
        let posted = mock.post(&to, "c").await.unwrap();
        assert_eq!(posted.id.as_str(), "m1");
        assert_eq!(
            mock.edit(&posted, "x").await,
            Err(SurfaceError::Unsupported("edit"))
        );
        assert_eq!(
            mock.react(&posted, "eyes").await,
            Err(SurfaceError::Unauthorized)
        );
        mock.react(&posted, "eyes").await.unwrap();
        assert_eq!(
            mock.upload(&to, &[]).await,
            Err(SurfaceError::Transport("reset".into()))
        );
        mock.upload(&to, &[]).await.unwrap();
        let key = ThreadKey {
            conv: conv("C1"),
            root: None,
        };
        assert_eq!(
            mock.history(&key, None, 5).await,
            Err(SurfaceError::Api("ratelimited".into()))
        );
        assert!(mock.history(&key, None, 5).await.unwrap().is_empty());
        assert_eq!(mock.calls().len(), 4);
        assert_eq!(mock.posts(), [(to, "c".to_owned())]);
        assert_eq!(
            mock.state().failures[&Op::Edit],
            [SurfaceError::Unauthorized]
        );
    }

    #[tokio::test]
    async fn a_cancelled_events_loop_leaves_its_events_for_the_next() {
        let mock = Arc::new(MockSurface::new());
        let a = binding();
        let (tx, mut rx) = mpsc::channel(1);
        let task = tokio::spawn({
            let mock = Arc::clone(&mock);
            let a = a.clone();
            async move { mock.events(&a, Sender::new(Forward(tx))).await }
        });
        mock.inject(event(&a, "first"));
        assert_eq!(rx.recv().await.unwrap().text, "first");
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        mock.inject(event(&a, "second"));

        let (tx, mut rx) = mpsc::channel(8);
        mock.close_events(a.id);
        mock.events(&a, Sender::new(Forward(tx))).await.unwrap();
        assert_eq!(rx.recv().await.unwrap().text, "second");
    }

    #[tokio::test]
    async fn a_loop_cancelled_while_delivering_keeps_that_event() {
        let mock = Arc::new(MockSurface::new());
        let a = binding();
        mock.inject(event(&a, "first"));
        mock.inject(event(&a, "blocked"));
        let (tx, mut rx) = mpsc::channel(1);
        let task = tokio::spawn({
            let mock = Arc::clone(&mock);
            let a = a.clone();
            async move { mock.events(&a, Sender::new(Forward(tx))).await }
        });
        while rx.is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!task.is_finished());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(rx.recv().await.unwrap().text, "first");
        assert!(rx.recv().await.is_none());

        let (tx, mut rx) = mpsc::channel(8);
        mock.close_events(a.id);
        mock.events(&a, Sender::new(Forward(tx))).await.unwrap();
        assert_eq!(rx.recv().await.unwrap().text, "blocked");
        assert!(rx.recv().await.is_none());
    }
}
