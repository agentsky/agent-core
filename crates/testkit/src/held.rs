//! [`Held`], a wiremock responder whose answer waits for the test.

use std::sync::{Mutex, mpsc};
use std::time::Duration;

use tokio::sync::oneshot;
use wiremock::{Request, Respond, ResponseTemplate};

/// Answers with its response only once the paired [`Hold`] lets it go, so a
/// test can act while a request is known to be in flight, and at once from
/// then on. Holding blocks the mock server's only thread: the server answers
/// nothing else meanwhile.
pub struct Held {
    response: ResponseTemplate,
    arrived: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<mpsc::Receiver<()>>,
}

/// The test's side of a [`Held`] response. Dropping it lets the response go.
pub struct Hold {
    arrived: oneshot::Receiver<()>,
    release: mpsc::Sender<()>,
}

impl Held {
    /// A responder that holds `response`, and the [`Hold`] that releases it.
    pub fn new(response: ResponseTemplate) -> (Self, Hold) {
        let (arrived, arrival) = oneshot::channel();
        let (release, releases) = mpsc::channel();
        (
            Self {
                response,
                arrived: Mutex::new(Some(arrived)),
                release: Mutex::new(releases),
            },
            Hold {
                arrived: arrival,
                release,
            },
        )
    }
}

impl Respond for Held {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        if let Some(arrived) = self.arrived.lock().unwrap().take() {
            let _ = arrived.send(());
        }
        let _ = self.release.lock().unwrap().recv();
        self.response.clone()
    }
}

impl Hold {
    /// Waits until the request is being held. Panics after 30 seconds, when
    /// the code under test failed or stopped short of sending it.
    pub async fn arrived(&mut self) {
        tokio::time::timeout(Duration::from_secs(30), &mut self.arrived)
            .await
            .expect("timed out waiting for the held request")
            .expect("the mock server dropped the held response");
    }

    /// Lets the held response go.
    pub fn release(self) {
        let _ = self.release.send(());
    }
}
