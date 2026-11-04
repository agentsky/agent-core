mod common;

use std::io::Write;
use std::sync::{Arc, Mutex, PoisonError};

use common::{Harness, PLACEHOLDER};
use runner::{SessionStart, TurnOutcome};
use testkit::Turn;
use tracing_subscriber::fmt::MakeWriter;

const SECRET: &str = "sk-ant-api03-FAKE-SECRET-0123456789";

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(PoisonError::into_inner)).into_owned()
    }
}

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'w> MakeWriter<'w> for Captured {
    type Writer = Self;

    fn make_writer(&'w self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn a_secret_in_messages_and_tool_output_leaves_no_trace_in_logs() {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(captured.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let h = Harness::new(&[
        Turn::reply(format!("The key you asked for is {SECRET}."))
            .with_command(["/bin/echo", SECRET])
            .with_command(["/bin/sh", "-c", &format!("echo {SECRET} >&2; exit 3")])
            .with_extra_line(format!("not json {SECRET}"))
            .with_extra_line(format!(r#"{{"type":"mystery","body":"{SECRET}"}}"#))
            .with_extra_line(format!(r#"{{"type":"{SECRET}"}}"#))
            .with_extra_line(format!(r#"{{"type":"system","subtype":"{SECRET}"}}"#))
            .with_extra_line(format!(
                r#"{{"type":"result_of_something","result":"{SECRET}","is_error":false}}"#
            ))
            .with_extra_line(format!(
                r#"{{"type":"assistant","error":"{SECRET}","message":{{"content":[{{"type":"tool_use","name":"{SECRET}","input":{{"secret":"{SECRET}"}}}}]}}}}"#
            )),
        Turn::api_error(401, format!("API Error: 401 {SECRET}")),
    ])
    .await;
    let mut process = h.start(h.launch(SessionStart::New)).await;
    let outcome = process
        .send_turn(&format!("Please print {SECRET}"))
        .await
        .unwrap();
    let TurnOutcome::Finished(result) = &outcome else {
        panic!("expected a result, got {outcome:?}");
    };
    assert!(
        result.result.as_deref().unwrap().contains(SECRET),
        "the reply itself is kept for delivery"
    );
    assert_eq!(result.stats.tool_calls, ["Bash", "Bash", "<other>"]);
    assert_eq!(result.stats.malformed_lines, 1);
    let error = process.send_turn("again").await.unwrap();
    process.stop().await;
    assert_eq!(h.anthropic.message_requests().await.len(), 2);
    assert!(
        h.transcript_user_messages()[0].contains(SECRET),
        "the secret went through the turn"
    );

    let logs = captured.text();
    assert!(logs.contains("claude turn ended"), "{logs}");
    assert!(logs.contains("started claude"), "{logs}");
    assert!(
        logs.contains("skipping a stdout line that isn't JSON"),
        "{logs}"
    );
    assert!(logs.contains(&h.container.session().to_string()), "{logs}");
    for needle in [SECRET, PLACEHOLDER, "ctl-token-for-test"] {
        assert!(!logs.contains(needle), "{needle} reached the logs:\n{logs}");
    }
    for debug in [
        format!("{outcome:?}"),
        format!("{error:?}"),
        format!("{:?}", h.launch(SessionStart::New)),
    ] {
        assert!(!debug.contains(SECRET), "{debug}");
        assert!(!debug.contains(PLACEHOLDER), "{debug}");
        assert!(!debug.contains("ctl-token-for-test"), "{debug}");
    }
}
