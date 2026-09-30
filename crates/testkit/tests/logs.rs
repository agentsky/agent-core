//! `Logs` keeps the lines whose callsites another thread hit first.

use std::thread;

use testkit::Logs;

fn tagged_callsite(who: &str) {
    tracing::info!(who, "the tagged callsite");
}

fn scoped_callsite(who: &str) {
    tracing::info!(who, "the scoped callsite");
}

#[test]
fn a_tag_keeps_its_lines_when_another_thread_hit_the_callsite_first() {
    let logs = Logs::global();
    let tag = logs.tag();
    thread::spawn(|| tagged_callsite("other-thread"))
        .join()
        .unwrap();
    tagged_callsite("tagged-thread");
    tag.snapshot()
        .assert_has("who=\"tagged-thread\"")
        .assert_lacks("other-thread");
    logs.snapshot()
        .matching("the tagged callsite")
        .assert_has("other-thread");
}

#[test]
fn a_scoped_subscriber_hears_a_callsite_another_thread_hit_first() {
    let own = Logs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(own.clone())
        .finish();
    Logs::global().scoped(subscriber, || {
        thread::spawn(|| scoped_callsite("other-thread"))
            .join()
            .unwrap();
        scoped_callsite("scoped-thread");
    });
    own.snapshot()
        .assert_has("who=\"scoped-thread\"")
        .assert_lacks("other-thread");
}

#[test]
fn matching_keeps_only_the_lines_holding_the_needle() {
    let logs = Logs::default();
    let mut writer = logs.clone();
    std::io::Write::write_all(&mut writer, b"one apple\ntwo pears\nthree apples\n").unwrap();
    assert_eq!(
        logs.snapshot().matching("apple").to_string(),
        "one apple\nthree apples\n"
    );
}

#[test]
#[should_panic(expected = "nothing was captured")]
fn an_absence_check_refuses_an_empty_capture() {
    Logs::default().snapshot().assert_lacks("secret");
}

#[test]
#[should_panic(expected = "reached the log")]
fn an_absence_check_fails_on_a_captured_needle() {
    let logs = Logs::default();
    std::io::Write::write_all(&mut logs.clone(), b"the secret\n").unwrap();
    logs.snapshot().assert_lacks("secret");
}
