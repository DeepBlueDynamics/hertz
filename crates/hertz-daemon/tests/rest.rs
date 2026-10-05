//! REST round-trip + auth tests (T4 brief Step 5).
//!
//! - `GET /api/status` returns 200 with the dongle roster.
//! - `POST /api/dongles/{id}/tune` round-trips and mutates worker control state.
//! - `check_auth`: non-loopback without a bearer token → 401; loopback + valid token
//!   pass. (Exercise the auth helper directly with a booted daemon's state.)

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hertz_channels::ChannelDb;
use hertz_daemon::server::check_auth;
use hertz_daemon::{Daemon, MockFactory};
use hertz_types::{DaemonConfig, DaemonSettings, DongleConfig, DongleRole};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn free_port() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    addr.to_string()
}

fn monitor_config(listen: String, data_dir: String, auth_token: Option<String>) -> DaemonConfig {
    DaemonConfig {
        daemon: DaemonSettings {
            listen,
            data_dir,
            auth_token,
        },
        dongles: vec![DongleConfig {
            serial: "MOCK01".into(),
            role: DongleRole::Monitor,
            bandplan: None,
            tap_channel: None,
            groups: None,
            dwell_ms: Some(150),
            priority: None,
            squelch_db: 6.0,
            record: false,
            frequency_hz: Some(156_800_000),
            scan: None,
        }],
        transcription: None,
        tx: None,
    }
}

/// Minimal raw-HTTP request: returns (status_code, body_string).
async fn http_request(
    addr: &str,
    method: &str,
    path: &str,
    body: &str,
    extra_headers: &str,
) -> (u16, String) {
    let mut s = tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(addr))
        .await
        .expect("connect")
        .expect("connect");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: hertz\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n{extra_headers}Connection: close\r\n\r\n{body}",
        len = body.len(),
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), s.read_to_end(&mut buf))
        .await
        .expect("read")
        .expect("read");
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
        .lines()
        .next()
        .and_then(|l| {
            l.split_whitespace()
                .nth(1)
                .and_then(|c| c.parse::<u16>().ok())
        })
        .unwrap_or(0);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_status_and_tune_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let listen = free_port();
    let factory = MockFactory::from_closure(|_| (128u8, 128u8)); // static mock
    let config = monitor_config(listen.clone(), tmp.path().to_string_lossy().into(), None);
    let daemon = Daemon::start(config, Arc::new(factory), ChannelDb::new())
        .await
        .expect("start");
    tokio::time::sleep(Duration::from_millis(200)).await;

    // GET /api/status
    let (code, body) = http_request(&listen, "GET", "/api/status", "", "").await;
    assert_eq!(code, 200, "status code {code}: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("status json");
    assert!(v["dongles"].is_array(), "body: {body}");
    assert_eq!(v["dongles"].as_array().unwrap().len(), 1);

    // The dongle id is "<serial>-0".
    let id = "MOCK01-0";
    let before = daemon.state.control_for(id).unwrap().snapshot().freq_hz;

    // POST tune to a new frequency.
    let new_freq = 156_900_000u64;
    let tune_body = format!("{{\"freq_hz\":{new_freq}}}");
    let (code, _b) = http_request(
        &listen,
        "POST",
        &format!("/api/dongles/{id}/tune"),
        &tune_body,
        "",
    )
    .await;
    assert_eq!(code, 200, "tune code {code}");
    // Give the DSP thread a tick to observe the epoch and retune.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after = daemon.state.control_for(id).unwrap().snapshot().freq_hz;
    assert_ne!(before, after, "freq not mutated ({before} == {after})");
    assert_eq!(after, new_freq, "freq should be {new_freq}, got {after}");

    daemon.shutdown();
    daemon.join().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_non_loopback_requires_token() {
    let tmp = tempfile::tempdir().unwrap();
    let listen = free_port();
    let factory = MockFactory::from_closure(|_| (128u8, 128u8));
    let config = monitor_config(
        listen.clone(),
        tmp.path().to_string_lossy().into(),
        Some("s3cret-token".into()),
    );
    let daemon = Daemon::start(config, Arc::new(factory), ChannelDb::new())
        .await
        .expect("start");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let state = &daemon.state;
    let empty = axum::http::HeaderMap::new();

    // Loopback peer: exempt regardless of token.
    let loopback: SocketAddr = "127.0.0.1:55555".parse().unwrap();
    assert!(check_auth(state, &empty, loopback).is_ok());

    // Non-loopback without token → 401.
    let remote: SocketAddr = "10.0.0.5:55555".parse().unwrap();
    assert!(check_auth(state, &empty, remote).is_err());

    // Non-loopback with correct token → ok.
    let mut hdrs = axum::http::HeaderMap::new();
    hdrs.insert(
        axum::http::header::AUTHORIZATION,
        "Bearer s3cret-token".parse().unwrap(),
    );
    assert!(check_auth(state, &hdrs, remote).is_ok());

    // Non-loopback with wrong token → 401.
    let mut bad = axum::http::HeaderMap::new();
    bad.insert(
        axum::http::header::AUTHORIZATION,
        "Bearer nope".parse().unwrap(),
    );
    assert!(check_auth(state, &bad, remote).is_err());

    daemon.shutdown();
    daemon.join().await;
}
