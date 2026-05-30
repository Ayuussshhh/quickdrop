//! HTTP-level integration tests for QuickDrop Share.
//!
//! These boot a real server on `127.0.0.1` (ephemeral port) and drive
//! it with an HTTP client, exercising the full path: create session →
//! landing page → metadata → download → range → limits → password →
//! expiry → 404.

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::Duration;

use quickdrop_share::session::ShareOptions;
use quickdrop_share::{ShareConfig, ShareService};

fn write_temp(content: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("payload.bin");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(content).unwrap();
    (dir, path)
}

async fn start_service() -> ShareService {
    ShareService::start(ShareConfig {
        bind_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
        preferred_port: 0,
        hostname: Some("quickdrop".into()),
        sweep_interval: Duration::from_millis(200),
    })
    .await
    .unwrap()
}

fn base(service: &ShareService) -> String {
    format!("http://127.0.0.1:{}", service.port())
}

#[tokio::test]
async fn download_roundtrip_matches_bytes() {
    let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    let (_dir, path) = write_temp(&payload);
    let service = start_service().await;
    let ticket = service
        .share(ShareOptions {
            file_path: path,
            ttl_secs: 60,
            ..Default::default()
        })
        .unwrap();
    let id = ticket.session.session_id;
    let url = format!("{}/download/{id}", base(&service));

    let resp = reqwest::get(&url).await.unwrap();
    assert!(resp.status().is_success());
    assert_eq!(
        resp.headers()
            .get("content-length")
            .unwrap()
            .to_str()
            .unwrap(),
        payload.len().to_string()
    );
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), payload.as_slice());
}

#[tokio::test]
async fn unknown_session_is_404_everywhere() {
    let service = start_service().await;
    let fake = "f".repeat(64);
    for path in [
        format!("/share/{fake}"),
        format!("/api/session/{fake}"),
        format!("/download/{fake}"),
    ] {
        let resp = reqwest::get(format!("{}{}", base(&service), path))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "{path}");
    }
}

#[tokio::test]
async fn api_session_hides_file_path() {
    let (_dir, path) = write_temp(b"hello world");
    let service = start_service().await;
    let ticket = service
        .share(ShareOptions {
            file_path: path,
            ttl_secs: 60,
            ..Default::default()
        })
        .unwrap();
    let id = ticket.session.session_id;
    let url = format!("{}/api/session/{id}", base(&service));
    let json: serde_json::Value = reqwest::get(&url).await.unwrap().json().await.unwrap();
    assert_eq!(json["file_name"], "payload.bin");
    assert_eq!(json["file_size"], 11);
    assert!(json.get("file_path").is_none(), "must not leak path");
}

#[tokio::test]
async fn range_request_returns_partial_content() {
    let payload: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
    let (_dir, path) = write_temp(&payload);
    let service = start_service().await;
    let ticket = service
        .share(ShareOptions {
            file_path: path,
            ttl_secs: 60,
            ..Default::default()
        })
        .unwrap();
    let id = ticket.session.session_id;
    let url = format!("{}/download/{id}", base(&service));

    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("Range", "bytes=100-199")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206);
    assert_eq!(
        resp.headers().get("content-range").unwrap().to_str().unwrap(),
        "bytes 100-199/1000"
    );
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), 100);
    assert_eq!(body.as_ref(), &payload[100..200]);
}

#[tokio::test]
async fn max_downloads_is_enforced_over_http() {
    let (_dir, path) = write_temp(b"limited");
    let service = start_service().await;
    let ticket = service
        .share(ShareOptions {
            file_path: path,
            ttl_secs: 60,
            max_downloads: 1,
            ..Default::default()
        })
        .unwrap();
    let id = ticket.session.session_id;
    let url = format!("{}/download/{id}", base(&service));

    assert!(reqwest::get(&url).await.unwrap().status().is_success());
    let second = reqwest::get(&url).await.unwrap();
    assert_eq!(second.status(), 410); // Gone / limit reached
}

#[tokio::test]
async fn password_protected_download() {
    let (_dir, path) = write_temp(b"secret");
    let service = start_service().await;
    let ticket = service
        .share(ShareOptions {
            file_path: path,
            ttl_secs: 60,
            password: Some("hunter2".into()),
            ..Default::default()
        })
        .unwrap();
    let id = ticket.session.session_id;

    let no_pw = format!("{}/download/{id}", base(&service));
    assert_eq!(reqwest::get(&no_pw).await.unwrap().status(), 401);

    let bad = format!("{}/download/{id}?pw=nope", base(&service));
    assert_eq!(reqwest::get(&bad).await.unwrap().status(), 401);

    let good = format!("{}/download/{id}?pw=hunter2", base(&service));
    assert!(reqwest::get(&good).await.unwrap().status().is_success());
}

#[tokio::test]
async fn expired_session_is_gone() {
    let (_dir, path) = write_temp(b"ephemeral");
    let service = start_service().await;
    let ticket = service
        .share(ShareOptions {
            file_path: path,
            ttl_secs: 1,
            ..Default::default()
        })
        .unwrap();
    let id = ticket.session.session_id;
    let url = format!("{}/download/{id}", base(&service));

    assert!(reqwest::get(&url).await.unwrap().status().is_success());
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(reqwest::get(&url).await.unwrap().status(), 404);
}

#[tokio::test]
async fn stop_sharing_revokes_immediately() {
    let (_dir, path) = write_temp(b"revoke me");
    let service = start_service().await;
    let ticket = service
        .share(ShareOptions {
            file_path: path,
            ttl_secs: 60,
            ..Default::default()
        })
        .unwrap();
    let id = ticket.session.session_id;
    let url = format!("{}/download/{id}", base(&service));

    assert!(reqwest::get(&url).await.unwrap().status().is_success());
    assert!(service.stop(&id));
    assert_eq!(reqwest::get(&url).await.unwrap().status(), 404);
}
