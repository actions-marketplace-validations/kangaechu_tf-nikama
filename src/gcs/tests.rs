//! GCS client, plan storage and lock tests against a mock JSON API.

use super::*;
use crate::lock::{GcsLock, Lock};
use crate::storage::{GcsPlanStorage, PlanMeta, PlanStorage, StoredPlan};
use serde_json::json;
use wiremock::matchers::{body_string_contains, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const OBJ_PATH: &str = "/storage/v1/b/bkt/o/o%2Fr%23proj";

fn client(server: &MockServer) -> GcsClient {
    GcsClient::with_token(&server.uri(), "tok")
}

#[tokio::test]
async fn upload_sends_multipart_with_metadata() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/upload/storage/v1/b/bkt/o"))
        .and(query_param("uploadType", "multipart"))
        .and(query_param("ifGenerationMatch", "0"))
        .and(header("Authorization", "Bearer tok"))
        .and(body_string_contains(r#""name":"o/r#proj""#))
        .and(body_string_contains(r#""LockId":"42""#))
        .and(body_string_contains(
            "Content-Type: text/plain\r\n\r\nhello",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"name": "o/r#proj"})))
        .expect(1)
        .mount(&server)
        .await;
    let mut meta = BTreeMap::new();
    meta.insert("LockId".to_string(), "42".to_string());
    let out = client(&server)
        .upload("bkt", "o/r#proj", "text/plain", &meta, b"hello", true)
        .await
        .unwrap();
    assert_eq!(out, Upload::Written);
}

#[tokio::test]
async fn upload_precondition_failure_is_already_exists() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/upload/storage/v1/b/bkt/o"))
        .respond_with(ResponseTemplate::new(412))
        .mount(&server)
        .await;
    let c = client(&server);
    let out = c
        .upload("bkt", "k", "text/plain", &BTreeMap::new(), b"", true)
        .await
        .unwrap();
    assert_eq!(out, Upload::AlreadyExists);
    // Without the precondition a 412 is an error.
    let err = c
        .upload("bkt", "k", "text/plain", &BTreeMap::new(), b"", false)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("HTTP 412"), "{}", err);
}

#[tokio::test]
async fn download_info_delete_handle_missing_objects() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(OBJ_PATH))
        .and(query_param("alt", "media"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/storage/v1/b/bkt/o/present"))
        .and(query_param("alt", "media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"plan".to_vec()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(OBJ_PATH))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/storage/v1/b/bkt/o/present"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "present", "generation": "17", "metadata": {"LockId": "7", "CreatedAt": "x"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(OBJ_PATH))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/storage/v1/b/bkt/o/present"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let c = client(&server);
    assert_eq!(c.download("bkt", "o/r#proj").await.unwrap(), None);
    assert_eq!(
        c.download("bkt", "present").await.unwrap(),
        Some(b"plan".to_vec())
    );
    assert_eq!(c.info("bkt", "o/r#proj").await.unwrap(), None);
    let info = c.info("bkt", "present").await.unwrap().unwrap();
    assert_eq!(info.generation, Some(17));
    assert_eq!(info.metadata["LockId"], "7");
    assert!(!c.delete("bkt", "o/r#proj").await.unwrap());
    assert!(c.delete("bkt", "present").await.unwrap());
}

#[tokio::test]
async fn server_errors_include_status_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/storage/v1/b/bkt/o/k"))
        .respond_with(
            ResponseTemplate::new(403).set_body_string("forbidden: no storage.objects.get"),
        )
        .mount(&server)
        .await;
    let err = client(&server)
        .info("bkt", "k")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("gs://bkt/k"), "{}", err);
    assert!(err.contains("HTTP 403"), "{}", err);
    assert!(err.contains("storage.objects.get"), "{}", err);
}

#[tokio::test]
async fn plan_storage_round_trip() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/upload/storage/v1/b/plans/o"))
        .and(query_param("uploadType", "multipart"))
        .and(body_string_contains(r#""name":"o-r-5-proj.tfplan""#))
        .and(body_string_contains(r#""head-sha":"abc123""#))
        .and(body_string_contains(r#""plan-no-changes":"true""#))
        .and(body_string_contains(
            "Content-Type: application/octet-stream\r\n\r\nPLAN",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/storage/v1/b/plans/o/o-r-5-proj.tfplan"))
        .and(query_param("alt", "media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"PLAN".to_vec()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/storage/v1/b/plans/o/o-r-5-proj.tfplan"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "o-r-5-proj.tfplan",
            "metadata": {"head-sha": "abc123", "plan-args": "[\"-target=module.x\"]", "plan-no-changes": "true"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/storage/v1/b/plans/o/o-r-5-proj.tfplan"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    let s = GcsPlanStorage::new(client(&server), "plans");
    assert_eq!(s.name(), "gcs");
    assert!(!s.is_noop());
    s.store(
        "o-r-5-proj.tfplan",
        b"PLAN",
        &PlanMeta {
            head_sha: Some("abc123".into()),
            extra_args: vec!["-target=module.x".into()],
            no_changes: true,
        },
    )
    .await
    .unwrap();
    assert!(s.exists("o-r-5-proj.tfplan").await.unwrap());
    assert_eq!(
        s.retrieve("o-r-5-proj.tfplan").await.unwrap(),
        Some(StoredPlan {
            bytes: b"PLAN".to_vec(),
            head_sha: Some("abc123".to_string()),
            extra_args: vec!["-target=module.x".into()],
            no_changes: true,
        })
    );
    s.delete("o-r-5-proj.tfplan").await.unwrap();
}

#[tokio::test]
async fn lock_creates_object_with_lock_id_and_loses_when_present() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/upload/storage/v1/b/locks/o"))
        .and(query_param("ifGenerationMatch", "0"))
        .and(body_string_contains(r#""name":"o/r#proj""#))
        .and(body_string_contains(r#""LockId":"12""#))
        .and(body_string_contains(r#""CreatedAt":""#))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/upload/storage/v1/b/locks/o"))
        .respond_with(ResponseTemplate::new(412))
        .mount(&server)
        .await;
    let l = GcsLock::new(client(&server), "locks");
    assert_eq!(l.name(), "gcs");
    assert!(l.lock(12, "o/r#proj").await.unwrap());
    assert!(!l.lock(13, "o/r#proj").await.unwrap());
}

#[tokio::test]
async fn lock_holder_and_unlock() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/storage/v1/b/locks/o/o%2Fr%23held"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "o/r#held", "generation": "3", "metadata": {"LockId": "99", "CreatedAt": "t"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/storage/v1/b/locks/o/o%2Fr%23bad"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "o/r#bad", "metadata": {"LockId": "abc"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/storage/v1/b/locks/o/o%2Fr%23free"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/storage/v1/b/locks/o/o%2Fr%23held"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/storage/v1/b/locks/o/o%2Fr%23free"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let l = GcsLock::new(client(&server), "locks");
    assert_eq!(l.holder("o/r#held").await.unwrap(), Some(99));
    assert_eq!(l.holder("o/r#free").await.unwrap(), None);
    let err = l.holder("o/r#bad").await.unwrap_err().to_string();
    assert!(err.contains("invalid LockId"), "{}", err);
    assert!(l.unlock("o/r#held").await.unwrap());
    assert!(!l.unlock("o/r#free").await.unwrap());
}

#[tokio::test]
async fn storage_from_env_selects_gcs() {
    for v in [
        "PLAN_UPLOAD_DESTINATION",
        "GOOGLE_STORAGE_PLAN_ARTEFACT_BUCKET",
        "STORAGE_EMULATOR_HOST",
    ] {
        std::env::remove_var(v);
    }
    std::env::set_var("PLAN_UPLOAD_DESTINATION", "gcp");
    let err = crate::storage::from_env().await.err().unwrap().to_string();
    assert!(
        err.contains("GOOGLE_STORAGE_PLAN_ARTEFACT_BUCKET"),
        "{}",
        err
    );

    std::env::set_var("GOOGLE_STORAGE_PLAN_ARTEFACT_BUCKET", "plans");
    std::env::set_var("STORAGE_EMULATOR_HOST", "localhost:1");
    assert_eq!(crate::storage::from_env().await.unwrap().name(), "gcs");

    std::env::set_var("PLAN_UPLOAD_DESTINATION", "azure");
    let err = crate::storage::from_env().await.err().unwrap().to_string();
    assert!(err.contains("not supported"), "{}", err);
    for v in [
        "PLAN_UPLOAD_DESTINATION",
        "GOOGLE_STORAGE_PLAN_ARTEFACT_BUCKET",
        "STORAGE_EMULATOR_HOST",
    ] {
        std::env::remove_var(v);
    }
}

#[tokio::test]
async fn list_pages_and_held_by_filters_on_lock_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/storage/v1/b/locks/o"))
        .and(query_param("prefix", "o/r#"))
        .and(wiremock::matchers::query_param_is_missing("pageToken"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [
                {"name": "o/r#a", "metadata": {"LockId": "7"}},
                {"name": "o/r#b", "metadata": {"LockId": "8"}}
            ],
            "nextPageToken": "p2"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/storage/v1/b/locks/o"))
        .and(query_param("prefix", "o/r#"))
        .and(query_param("pageToken", "p2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [
                {"name": "o/r#c", "metadata": {"LockId": " 7 "}},
                {"name": "o/r#d"}
            ]
        })))
        .mount(&server)
        .await;
    let objects = client(&server).list("locks", "o/r#").await.unwrap();
    assert_eq!(
        objects.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(),
        vec!["o/r#a", "o/r#b", "o/r#c", "o/r#d"]
    );
    let l = GcsLock::new(client(&server), "locks");
    assert_eq!(
        l.held_by(7, "o/r#").await.unwrap(),
        vec!["o/r#a".to_string(), "o/r#c".to_string()]
    );
}
