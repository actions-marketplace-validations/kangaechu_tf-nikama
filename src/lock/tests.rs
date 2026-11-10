//! PR lock semantics tests with an in-memory backend and a mock GitHub API.

use super::*;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::testing::MemLock;

async fn mock_pr(server: &MockServer, number: u64, state: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/repos/o/r/pulls/{}", number)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "head": {"ref": "x", "sha": "s"}, "base": {"ref": "main", "sha": "b"},
            "merged": false, "state": state, "draft": false
        })))
        .mount(server)
        .await;
}

fn locker(server: &MockServer, backend: Arc<dyn Lock>) -> PrLocker {
    PrLocker::new(
        backend,
        GithubClient::new("t", "o", "r", Some(&server.uri())),
    )
}

#[tokio::test]
async fn acquire_then_already_held() {
    let server = MockServer::start().await;
    let l = locker(&server, Arc::new(MemLock::default()));
    assert_eq!(l.lock(1, "o/r#a").await.unwrap(), LockOutcome::Acquired);
    assert_eq!(l.lock(1, "o/r#a").await.unwrap(), LockOutcome::AlreadyHeld);
    assert_eq!(l.backend.holder("o/r#a").await.unwrap(), Some(1));
}

#[tokio::test]
async fn open_pr_holder_blocks_and_is_cached() {
    let server = MockServer::start().await;
    mock_pr(&server, 9, "open").await;
    let backend = Arc::new(MemLock::default());
    backend.lock(9, "o/r#a").await.unwrap();
    let l = locker(&server, backend);
    assert_eq!(
        l.lock(1, "o/r#a").await.unwrap(),
        LockOutcome::HeldByOther(9)
    );
    assert_eq!(
        l.lock(1, "o/r#a").await.unwrap(),
        LockOutcome::HeldByOther(9)
    );
    // The PR state lookup is cached per holder.
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn closed_pr_holder_is_taken_over() {
    let server = MockServer::start().await;
    mock_pr(&server, 9, "closed").await;
    let backend = Arc::new(MemLock::default());
    backend.lock(9, "o/r#a").await.unwrap();
    let l = locker(&server, backend);
    assert_eq!(l.lock(1, "o/r#a").await.unwrap(), LockOutcome::Acquired);
    assert_eq!(l.backend.holder("o/r#a").await.unwrap(), Some(1));
}

#[tokio::test]
async fn unlock_only_by_holder_and_force() {
    let server = MockServer::start().await;
    let l = locker(&server, Arc::new(MemLock::default()));
    l.lock(1, "o/r#a").await.unwrap();
    assert!(!l.unlock(2, "o/r#a").await.unwrap());
    assert_eq!(l.backend.holder("o/r#a").await.unwrap(), Some(1));
    assert!(l.unlock(1, "o/r#a").await.unwrap());
    assert_eq!(l.backend.holder("o/r#a").await.unwrap(), None);
    l.lock(3, "o/r#a").await.unwrap();
    assert!(l.force_unlock("o/r#a").await.unwrap());
    assert!(!l.force_unlock("o/r#a").await.unwrap());
}

#[tokio::test]
async fn noop_lock_behaviour() {
    let n = NoopLock;
    assert!(n.lock(1, "x").await.unwrap());
    assert!(n.unlock("x").await.unwrap());
    assert_eq!(n.holder("x").await.unwrap(), None);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn from_env_selection() {
    let _g = crate::util::test_env_guard();
    for v in ["LOCK_PROVIDER", "DISABLE_LOCKING"] {
        std::env::remove_var(v);
    }
    assert_eq!(from_env(false).await.unwrap().name(), "noop");

    std::env::set_var("DISABLE_LOCKING", "true");
    assert_eq!(from_env(true).await.unwrap().name(), "noop");
    std::env::remove_var("DISABLE_LOCKING");

    std::env::set_var("LOCK_PROVIDER", "noop");
    assert_eq!(from_env(true).await.unwrap().name(), "noop");

    std::env::remove_var("GOOGLE_STORAGE_LOCK_BUCKET");
    std::env::set_var("LOCK_PROVIDER", "gcp");
    let err = from_env(true).await.err().unwrap().to_string();
    assert!(err.contains("GOOGLE_STORAGE_LOCK_BUCKET"), "{}", err);

    std::env::set_var("GOOGLE_STORAGE_LOCK_BUCKET", "Locks");
    std::env::set_var("STORAGE_EMULATOR_HOST", "localhost:1");
    assert_eq!(from_env(true).await.unwrap().name(), "gcs");
    std::env::remove_var("STORAGE_EMULATOR_HOST");
    std::env::remove_var("GOOGLE_STORAGE_LOCK_BUCKET");

    std::env::set_var("LOCK_PROVIDER", "azure");
    let err = from_env(true).await.err().unwrap().to_string();
    assert!(err.contains("not supported"), "{}", err);

    std::env::set_var("LOCK_PROVIDER", "bogus");
    let err = from_env(true).await.err().unwrap().to_string();
    assert!(err.contains("unknown LOCK_PROVIDER"), "{}", err);
    std::env::remove_var("LOCK_PROVIDER");
}

#[tokio::test]
async fn status_and_held_by() {
    let server = MockServer::start().await;
    mock_pr(&server, 2, "open").await;
    mock_pr(&server, 3, "closed").await;
    let mem = Arc::new(MemLock::holding(&[
        ("o/r#a", 1),
        ("o/r#b", 2),
        ("o/r#c", 3),
        ("o/r#d", 1),
        ("x/y#a", 1),
    ]));
    let l = locker(&server, mem.clone());
    assert_eq!(l.status(1, "o/r#a").await.unwrap(), HolderStatus::Ours);
    assert_eq!(l.status(1, "o/r#b").await.unwrap(), HolderStatus::Other(2));
    // Held by a closed PR: free for the taking.
    assert_eq!(l.status(1, "o/r#c").await.unwrap(), HolderStatus::Free);
    assert_eq!(l.status(1, "o/r#zzz").await.unwrap(), HolderStatus::Free);
    // status() never acquires.
    assert_eq!(mem.holder("o/r#c").await.unwrap(), Some(3));
    assert_eq!(
        l.held_by(1, "o/r#").await.unwrap(),
        vec!["o/r#a".to_string(), "o/r#d".to_string()]
    );
}
