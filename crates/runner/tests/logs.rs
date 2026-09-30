mod common;

use common::{Harness, PLACEHOLDER};
use runner::{SessionStart, TurnOutcome};
use testkit::{Logs, Turn};

const SECRET: &str = "sk-ant-api03-FAKE-SECRET-0123456789";

#[tokio::test]
async fn a_secret_in_messages_and_tool_output_leaves_no_trace_in_logs() {
    let logs = Logs::global();
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

    let logged = logs.snapshot();
    logged
        .assert_has("claude turn ended")
        .assert_has("started claude")
        .assert_has("skipping a stdout line that isn't JSON")
        .assert_has(&h.container.session().to_string());
    for needle in [SECRET, PLACEHOLDER, "ctl-token-for-test"] {
        logged.assert_lacks(needle);
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
