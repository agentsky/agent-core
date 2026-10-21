//! [`Logs`]: what a test binary logs, captured by one global subscriber.
//!
//! A test that captures logs with a scoped subscriber
//! (`tracing::subscriber::set_default` or `with_default`) can miss its own
//! events. While that subscriber is the only dispatcher registered,
//! `tracing-core` works out a callsite's interest, the first time the
//! callsite is hit, from the dispatcher of whichever thread hits it, and
//! keeps it for every thread. Another test's thread, with no subscriber,
//! that hits a shared callsite first turns it off for everyone: a presence
//! check fails, and an absence check ("no secret in the logs") passes
//! whatever was logged.
//!
//! So each test binary installs one global subscriber, once, with
//! [`Logs::global`] or [`Logs::install`], and each test reads only its own
//! lines: those naming something only it has, such as a session id, with
//! [`Logged::matching`], or those logged inside a span it enters on its own
//! thread, with [`Logs::tag`]. A tag misses some of a test's lines, so an
//! absence check reads [`Logged::matching`] or the whole snapshot instead.
//! An absence check is meaningful only next to a presence check proving
//! capture was on: [`Logged::assert_lacks`] refuses an empty capture, and a
//! test also asserts the neighbouring line it expects with
//! [`Logged::assert_has`].

use std::any::{TypeId, type_name};
use std::fmt;
use std::io;
use std::ops::Deref;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use tracing::Subscriber;
use tracing::span::EnteredSpan;
use tracing_subscriber::fmt::MakeWriter;
use uuid::Uuid;

/// A buffer of log lines, written to as a subscriber's
/// [`MakeWriter`]. The binary's global capture is one; a test of a
/// subscriber itself can make its own with [`Logs::default`].
#[derive(Clone, Default)]
pub struct Logs(Arc<Mutex<Vec<u8>>>);

static GLOBAL: OnceLock<(Logs, TypeId, &str)> = OnceLock::new();

impl Logs {
    /// The binary's global capture, installing on first use a
    /// human-readable subscriber without colors that captures every level.
    ///
    /// # Panics
    ///
    /// If this installs the capture and another global subscriber is
    /// already installed.
    pub fn global() -> &'static Self {
        Self::install(|logs| {
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_ansi(false)
                .with_writer(logs)
                .finish()
        })
    }

    /// The binary's global capture, installing on first use the subscriber
    /// `make` builds to write into the buffer it is given. Only the first
    /// call's `make` runs, so a binary makes every call through one
    /// function, and every call passes the same `make`.
    ///
    /// # Panics
    ///
    /// If this installs the capture and another global subscriber is
    /// already installed, or if the capture was installed with a different
    /// `make`, such as [`Logs::global`]'s.
    pub fn install<F, S>(make: F) -> &'static Self
    where
        F: FnOnce(Self) -> S + 'static,
        S: Subscriber + Send + Sync + 'static,
    {
        let (logs, installed, installed_name) = GLOBAL.get_or_init(|| {
            let logs = Self::default();
            tracing::subscriber::set_global_default(make(logs.clone()))
                .expect("another global subscriber is installed");
            tracing::callsite::rebuild_interest_cache();
            (logs, TypeId::of::<F>(), type_name::<F>())
        });
        assert!(
            *installed == TypeId::of::<F>(),
            "the global log capture was installed with {installed_name}, not {}: \
             every call in a binary must pass the same `make`",
            type_name::<F>()
        );
        logs
    }

    /// Every line captured so far.
    pub fn snapshot(&self) -> Logged {
        Logged(
            String::from_utf8_lossy(&self.0.lock().unwrap_or_else(PoisonError::into_inner))
                .into_owned(),
        )
    }

    /// Enters a span with a unique id on this thread until the [`Tag`] is
    /// dropped. [`Tag::snapshot`] keeps the lines naming the id: those of
    /// events whose span parents lead back to this span. That is a line
    /// logged on this thread meanwhile, outside any other span or inside
    /// one made while the tag was entered, including by the tasks a
    /// current-thread runtime, as `#[tokio::test]` uses, polls here.
    ///
    /// A tag misses the lines logged inside a span made before it, such as
    /// those of a task spawned earlier with its own
    /// [`instrument`](tracing::Instrument::instrument), and every line
    /// logged on another thread, such as by `spawn_blocking`. So a tag
    /// suits presence checks, and an absence check reads the lines
    /// [`Logged::matching`] an id only this test logs, or the whole
    /// [`snapshot`](Self::snapshot), where those lines still are.
    pub fn tag(&self) -> Tag<'_> {
        let id = Uuid::new_v4().to_string();
        let entered = tracing::info_span!("test", tag = %id).entered();
        Tag {
            logs: self,
            id,
            _entered: entered,
        }
    }

    /// Runs `f` with `subscriber` as this thread's default, for a test of
    /// a subscriber itself. `self` is the binary's global capture, whose
    /// subscriber was registered first: with two dispatchers registered,
    /// `tracing-core` asks each of them about a callsite hit for the first
    /// time, whichever thread hits it, so `subscriber` has its say.
    pub fn scoped<S, T>(&'static self, subscriber: S, f: impl FnOnce() -> T) -> T
    where
        S: Subscriber + Send + Sync + 'static,
    {
        tracing::subscriber::with_default(subscriber, f)
    }
}

impl io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'w> MakeWriter<'w> for Logs {
    type Writer = Self;

    fn make_writer(&'w self) -> Self::Writer {
        self.clone()
    }
}

/// A span entered on this thread by [`Logs::tag`], naming the lines logged
/// inside it.
pub struct Tag<'a> {
    logs: &'a Logs,
    id: String,
    _entered: EnteredSpan,
}

impl Tag<'_> {
    /// The lines captured so far inside this tag's span.
    pub fn snapshot(&self) -> Logged {
        self.logs.snapshot().matching(&self.id)
    }
}

/// Captured lines, as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Logged(String);

impl Logged {
    /// Only the lines containing `needle`.
    #[must_use]
    pub fn matching(&self, needle: &str) -> Self {
        let mut kept = String::new();
        for line in self.0.lines().filter(|line| line.contains(needle)) {
            kept.push_str(line);
            kept.push('\n');
        }
        Self(kept)
    }

    /// # Panics
    ///
    /// Unless some line contains `needle`.
    #[track_caller]
    pub fn assert_has(&self, needle: &str) -> &Self {
        assert!(self.0.contains(needle), "{needle:?} wasn't logged:\n{self}");
        self
    }

    /// # Panics
    ///
    /// If a line contains `needle`, or if nothing was captured, since then
    /// its absence proves nothing.
    #[track_caller]
    pub fn assert_lacks(&self, needle: &str) -> &Self {
        assert!(
            !self.0.is_empty(),
            "nothing was captured, looking for {needle:?}"
        );
        assert!(
            !self.0.contains(needle),
            "{needle:?} reached the log:\n{self}"
        );
        self
    }
}

impl Deref for Logged {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Logged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
