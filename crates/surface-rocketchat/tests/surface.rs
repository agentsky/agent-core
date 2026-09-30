//! Tests of `RocketChatSurface`'s REST-backed methods against
//! `testkit::rocketchat::FakeRest`.

use std::path::PathBuf;
use std::sync::Arc;

use core_types::{
    ConvRef, Cursor, LengthUnit, Limit, MsgRef, OutFile, ReplyTarget, Surface, SurfaceError,
    SurfaceKind, ThreadKey, UserId,
};
use secrecy::{ExposeSecret, SecretString};
use surface_rocketchat::rest::{Credentials, NewBotUser, RestClient};
use surface_rocketchat::{BotRoles, Dedup, RocketChatConfig, RocketChatSurface};
use testkit::rocketchat::FakeRest;

const TEAM: &str = "chat.example";

struct NeverFirst;

#[async_trait::async_trait]
impl Dedup for NeverFirst {
    async fn mark_event_processed(&self, _: &str, _: &str) -> Result<bool, SurfaceError> {
        Ok(false)
    }
}

struct Setup {
    fake: FakeRest,
    surface: Arc<RocketChatSurface>,
    bot: String,
    alice: String,
}

async fn setup_with(limit: Option<Limit>) -> Setup {
    let fake = FakeRest::start().await;
    let manager = RestClient::new(
        &fake.uri(),
        Credentials {
            user_id: FakeRest::MANAGER_ID.into(),
            token: SecretString::from(FakeRest::MANAGER_TOKEN),
        },
    )
    .unwrap();
    let new = NewBotUser {
        username: "helper",
        name: "Helper",
        email: "helper@bots.invalid",
    };
    let (user, password) = manager.create_bot_user(&new).await.unwrap();
    let creds = manager
        .issue_bot_token(&user.username, password, "agentd")
        .await
        .unwrap();
    let alice = fake.add_user("alice");
    fake.add_room("GENERAL", "c", "general");
    fake.add_member("GENERAL", user.id.as_str());
    fake.add_member("GENERAL", &alice);
    let mut config = RocketChatConfig::new(fake.uri(), TEAM.into(), creds);
    if let Some(limit) = limit {
        config.message_limit = limit;
    }
    let surface =
        RocketChatSurface::new(config, Arc::new(NeverFirst), BotRoles::new(manager)).unwrap();
    Setup {
        fake,
        surface: Arc::new(surface),
        bot: user.id.to_string(),
        alice,
    }
}

async fn setup() -> Setup {
    setup_with(None).await
}

fn conv(room: &str) -> ConvRef {
    ConvRef {
        surface: SurfaceKind::RocketChat,
        team: TEAM.into(),
        conversation: room.into(),
    }
}

fn target(room: &str, thread_root: Option<&str>) -> ReplyTarget {
    ReplyTarget {
        conv: conv(room),
        thread_root: thread_root.map(Into::into),
    }
}

fn thread(room: &str, root: Option<&str>) -> ThreadKey {
    ThreadKey {
        conv: conv(room),
        root: root.map(Into::into),
    }
}

#[tokio::test]
async fn post_replies_in_the_thread_as_the_bot() {
    let s = setup().await;
    let root = s.fake.seed_message("GENERAL", &s.alice, "question", None);
    let posted = s
        .surface
        .post(&target("GENERAL", Some(&root)), "answer")
        .await
        .unwrap();
    assert_eq!(posted.conv, conv("GENERAL"));
    let stored = s.fake.message(posted.id.as_str()).unwrap();
    assert_eq!(stored.text, "answer");
    assert_eq!(stored.tmid.as_deref(), Some(root.as_str()));
    assert_eq!(stored.user_id, s.bot);
}

#[tokio::test]
async fn edit_and_react_act_on_the_posted_message() {
    let s = setup().await;
    let posted = s
        .surface
        .post(&target("GENERAL", None), "draft")
        .await
        .unwrap();
    s.surface.edit(&posted, "final").await.unwrap();
    s.surface.react(&posted, "eyes").await.unwrap();
    s.surface.react(&posted, "eyes").await.unwrap();
    let stored = s.fake.message(posted.id.as_str()).unwrap();
    assert_eq!(stored.text, "final");
    assert!(stored.edited);
    assert_eq!(stored.reactions, [(":eyes:".to_owned(), s.bot.clone())]);
}

#[tokio::test]
async fn unreact_removes_only_the_bots_reaction() {
    let s = setup().await;
    let root = s.fake.seed_message("GENERAL", &s.alice, "question", None);
    let msg = MsgRef {
        conv: conv("GENERAL"),
        id: root.as_str().into(),
    };
    s.surface.react(&msg, "hourglass").await.unwrap();
    s.surface.unreact(&msg, "hourglass").await.unwrap();
    s.surface.unreact(&msg, "hourglass").await.unwrap();
    assert!(s.fake.message(&root).unwrap().reactions.is_empty());
}

#[tokio::test]
async fn a_bot_never_posts_where_posting_would_join_it() {
    let s = setup().await;
    s.fake.add_room("OTHER", "c", "other");
    s.fake.add_member("OTHER", &s.alice);
    let root = s.fake.seed_message("OTHER", &s.alice, "@helper hi", None);
    assert!(s.surface.can_post(&conv("GENERAL")).await.unwrap());
    assert!(!s.surface.can_post(&conv("OTHER")).await.unwrap());
    let err = s
        .surface
        .post(&target("OTHER", Some(&root)), "answer")
        .await
        .unwrap_err();
    assert!(matches!(err, SurfaceError::Forbidden(_)), "{err:?}");
    let file = OutFile {
        name: "x.txt".into(),
        path: PathBuf::from("/nonexistent"),
    };
    let err = s
        .surface
        .upload(&target("OTHER", None), &[file])
        .await
        .unwrap_err();
    assert!(matches!(err, SurfaceError::Forbidden(_)), "{err:?}");
    assert!(s.fake.requests("chat.postMessage").await.is_empty());

    s.fake.add_member("OTHER", &s.bot);
    assert!(s.surface.can_post(&conv("OTHER")).await.unwrap());
    s.surface
        .post(&target("OTHER", Some(&root)), "answer")
        .await
        .unwrap();
    let listings = s.fake.requests("subscriptions.get").await.len();
    s.surface
        .post(&target("GENERAL", None), "again")
        .await
        .unwrap();
    assert_eq!(
        s.fake.requests("subscriptions.get").await.len(),
        listings,
        "a room known to be joined isn't listed again"
    );
}

#[tokio::test]
async fn upload_posts_each_file_in_the_thread() {
    let s = setup().await;
    let dir = std::env::temp_dir().join(format!("rc-upload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path: PathBuf = dir.join("report.txt");
    std::fs::write(&path, "done").unwrap();
    let root = s
        .fake
        .seed_message("GENERAL", &s.alice, "please report", None);
    let files = [
        OutFile {
            name: "report.txt".into(),
            path: path.clone(),
        },
        OutFile {
            name: "copy.txt".into(),
            path,
        },
    ];
    s.surface
        .upload(&target("GENERAL", Some(&root)), &files)
        .await
        .unwrap();
    s.surface
        .upload(&target("GENERAL", Some(&root)), &[])
        .await
        .unwrap();
    let confirms = s.fake.requests("rooms.mediaConfirm").await;
    assert_eq!(confirms.len(), 2);
    let thread = s
        .surface
        .history(&thread("GENERAL", Some(&root)), None, 10)
        .await
        .unwrap();
    let names: Vec<&str> = thread
        .iter()
        .flat_map(|m| m.files.iter().map(|f| f.name.as_str()))
        .collect();
    assert_eq!(names, ["report.txt", "copy.txt"]);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn another_surface_or_server_is_refused_before_any_call() {
    let s = setup().await;
    let mut other = target("GENERAL", None);
    other.conv.team = "other.example".into();
    let err = s.surface.post(&other, "hi").await.unwrap_err();
    assert!(matches!(err, SurfaceError::NotFound(_)));
    let msg = MsgRef {
        conv: ConvRef {
            surface: SurfaceKind::Slack,
            ..conv("GENERAL")
        },
        id: "m".into(),
    };
    assert!(s.surface.edit(&msg, "x").await.is_err());
    assert!(s.surface.react(&msg, "eyes").await.is_err());
    assert!(s.surface.upload(&other, &[]).await.is_err());
    let mut elsewhere = thread("GENERAL", None);
    elsewhere.conv.team = "other.example".into();
    assert!(s.surface.history(&elsewhere, None, 5).await.is_err());
    assert!(s.fake.requests("chat.postMessage").await.is_empty());
}

#[tokio::test]
async fn top_level_history_is_oldest_first_and_pages_backwards() {
    let s = setup().await;
    let ids: Vec<String> = (1..=4)
        .map(|n| {
            s.fake
                .seed_message("GENERAL", &s.alice, &format!("m{n}"), None)
        })
        .collect();
    s.fake
        .seed_message("GENERAL", &s.alice, "in a thread", Some(&ids[0]));
    let top = thread("GENERAL", None);
    let newest = s.surface.history(&top, None, 2).await.unwrap();
    let texts: Vec<&str> = newest.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["m3", "m4"]);
    assert_eq!(newest[0].sender.user.as_str(), s.alice);
    assert!(!newest[0].sender_is_bot);
    let older = s
        .surface
        .history(&top, Some(Cursor::from(newest[0].id.clone())), 10)
        .await
        .unwrap();
    let texts: Vec<&str> = older.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["m1", "m2"]);
    assert!(s.surface.history(&top, None, 0).await.unwrap().is_empty());
}

#[tokio::test]
async fn thread_history_includes_the_root_and_pages_backwards() {
    let s = setup().await;
    let root = s.fake.seed_message("GENERAL", &s.alice, "root", None);
    let replies: Vec<String> = (1..=3)
        .map(|n| {
            s.fake
                .seed_message("GENERAL", &s.alice, &format!("r{n}"), Some(&root))
        })
        .collect();
    s.surface
        .post(&target("GENERAL", Some(&root)), "bot reply")
        .await
        .unwrap();
    let key = thread("GENERAL", Some(&root));
    let all = s.surface.history(&key, None, 10).await.unwrap();
    let texts: Vec<&str> = all.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["root", "r1", "r2", "r3", "bot reply"]);
    assert!(all[4].sender_is_bot);
    assert!(!all[0].sender_is_bot);
    let newest_two = s.surface.history(&key, None, 2).await.unwrap();
    let texts: Vec<&str> = newest_two.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["r3", "bot reply"]);
    let before = s
        .surface
        .history(&key, Some(Cursor::new(replies[1].as_str())), 10)
        .await
        .unwrap();
    let texts: Vec<&str> = before.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["root", "r1"]);
    let before_root = s
        .surface
        .history(&key, Some(Cursor::new(root.as_str())), 10)
        .await
        .unwrap();
    assert!(before_root.is_empty());
}

#[tokio::test]
async fn history_of_an_unknown_cursor_fails() {
    let s = setup().await;
    let err = s
        .surface
        .history(&thread("GENERAL", None), Some(Cursor::new("missing")), 5)
        .await
        .unwrap_err();
    assert!(matches!(err, SurfaceError::NotFound(_)));
}

#[tokio::test]
async fn render_converts_and_splits_to_the_configured_limit() {
    let s = setup_with(Some(Limit {
        max: 20,
        unit: LengthUnit::Utf16,
    }))
    .await;
    let chunks = s
        .surface
        .render("**hello** @all, this is a longer reply that must be split");
    assert!(chunks.len() > 1);
    assert!(chunks.iter().all(|c| c.encode_utf16().count() <= 20));
    let joined: String = chunks.concat();
    assert!(joined.starts_with("**hello** @\u{200B}all"));
    assert_eq!(s.surface.caps().message_limit.max, 20);
    let s = setup().await;
    assert_eq!(s.surface.render("see @helper"), ["see @helper"]);
}

#[tokio::test]
async fn roles_the_caller_cannot_see_fail_naming_the_missing_permission() {
    let s = setup().await;
    let roles = BotRoles::new(s.surface.rest().clone());
    let err = roles
        .is_bot(&UserId::from(s.alice.as_str()))
        .await
        .unwrap_err();
    let SurfaceError::Forbidden(text) = &err else {
        panic!("unexpected error {err:?}");
    };
    assert!(text.contains("view-full-other-user-info"), "{text}");
    assert!(text.contains(&s.bot), "{text}");
    let token = s.surface.rest().credentials().token.expose_secret();
    assert!(!err.to_string().contains(token));
    assert!(roles.is_bot(&UserId::from(s.bot.as_str())).await.unwrap());
    let before = s.fake.requests("users.info").await.len();
    assert!(roles.is_bot(&UserId::from(s.bot.as_str())).await.unwrap());
    assert_eq!(s.fake.requests("users.info").await.len(), before);
    assert!(roles.is_bot(&UserId::from(s.alice.as_str())).await.is_err());
    assert_eq!(s.fake.requests("users.info").await.len(), before + 1);
}
