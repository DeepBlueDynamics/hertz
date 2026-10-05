//! MockSdr end-to-end test (T4 brief Step 5). Boots the daemon on an ephemeral port
//! with a `MockFactory` whose closure synthesizes an NFM voice burst (noise → voice
//! → noise), then asserts the full lifecycle **over a real WebSocket connection**:
//! Hello → SquelchEvent(open) → binary audio frames → SquelchEvent(close) →
//! RecordingSaved, and that the WAV file lands in the temp data_dir.

use std::sync::Arc;
use std::time::Duration;

use hertz_channels::ChannelDb;
use hertz_daemon::{Daemon, MockFactory};
use hertz_dsp::testutil::{fm_multitone_signal, white_noise};
use hertz_sdr::{MockSdr, SdrDevice, SdrError};
use hertz_types::wire::WsServerMsg;
use hertz_types::{DaemonConfig, DaemonSettings, DongleConfig, DongleRole};
use num_complex::Complex32;

/// Build the mock u8 IQ stream: ~1 s noise → ~1.5 s NFM voice → ~4 s noise (long
/// enough to expire the 10-frame hang and close the squelch).
fn build_mock_iq_pairs() -> Vec<(u8, u8)> {
    let rate = 240_000u32;
    let n_noise1 = rate as usize; // 1 s
    let n_voice = (rate as f32 * 1.5) as usize; // 1.5 s
    let n_noise2 = (rate as f32 * 4.0) as usize; // 4 s — exceeds hang (2 s)

    let noise1 = white_noise(rate, n_noise1, 0.1, 11);
    let voice = fm_multitone_signal(rate, n_voice, 0.0, &[700.0, 1100.0, 1900.0], 4500.0, 1.0);
    let noise2 = white_noise(rate, n_noise2, 0.1, 23);

    noise1
        .into_iter()
        .chain(voice)
        .chain(noise2)
        .map(|c: Complex32| {
            let i = (c.re * 127.5 + 127.5).clamp(0.0, 255.0) as u8;
            let q = (c.im * 127.5 + 127.5).clamp(0.0, 255.0) as u8;
            (i, q)
        })
        .collect()
}

fn free_port() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    addr.to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mock_sdr_end_to_end_lifecycle_over_ws() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("warn,hertz_daemon=info"))
        .try_init();

    let tmp = tempfile::tempdir().expect("tmp");
    let pairs = Arc::new(build_mock_iq_pairs());
    let pairs_for_closure = Arc::clone(&pairs);
    let factory = MockFactory::from_closure(move |idx: u64| -> (u8, u8) {
        pairs_for_closure[(idx as usize) % pairs_for_closure.len()]
    });

    let listen = free_port();
    let config = DaemonConfig {
        daemon: DaemonSettings {
            listen: listen.clone(),
            data_dir: tmp.path().to_string_lossy().to_string(),
            auth_token: None,
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
            record: true,
            frequency_hz: Some(156_800_000),
            scan: None,
        }],
        transcription: None,
        tx: None,
    };

    let daemon = Daemon::start(config, Arc::new(factory), ChannelDb::new())
        .await
        .expect("daemon start");

    // Give the worker + DSP thread a moment to prime, then open a WS to /stream.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let url = format!("ws://{listen}/stream?audio=all");

    let mut ws = tokio::time::timeout(Duration::from_secs(3), async {
        tokio_tungstenite::connect_async(&url).await
    })
    .await
    .expect("ws connect timeout")
    .expect("ws connect")
    .0;

    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let mut saw_hello = false;
    let mut saw_open = false;
    let mut saw_audio = false;
    let mut saw_close = false;
    let mut saw_recording_saved = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let next = tokio::time::timeout_at(deadline, ws.next()).await;
        match next {
            Err(_) => break, // timeout
            Ok(None) => break,
            Ok(Some(Err(e))) => panic!("ws error: {e}"),
            Ok(Some(Ok(msg))) => match msg {
                Message::Text(t) => {
                    let parsed: WsServerMsg =
                        serde_json::from_str(&t).unwrap_or_else(|e| panic!("parse {t}: {e}"));
                    match parsed {
                        WsServerMsg::Hello { dongles, .. } => {
                            assert_eq!(dongles.len(), 1);
                            assert_eq!(dongles[0].serial, "MOCK01");
                            saw_hello = true;
                        }
                        WsServerMsg::Event(hertz_types::Event::SquelchEvent { open, .. }) => {
                            if open {
                                saw_open = true;
                            } else {
                                saw_close = true;
                            }
                        }
                        WsServerMsg::Event(hertz_types::Event::RecordingSaved { .. }) => {
                            saw_recording_saved = true;
                        }
                        _ => {}
                    }
                }
                Message::Binary(_) => {
                    saw_audio = true;
                }
                Message::Close(_) => break,
                _ => {}
            },
        }
        if saw_hello && saw_open && saw_audio && saw_close && saw_recording_saved {
            break;
        }
    }
    let _ = ws.close(None).await;

    assert!(saw_hello, "never received WS Hello");
    assert!(
        saw_open,
        "never saw squelch OPEN (voice burst not detected)"
    );
    assert!(saw_audio, "never saw any binary audio frames");
    assert!(saw_close, "never saw squelch CLOSE (hang did not expire)");
    assert!(
        saw_recording_saved,
        "never saw RecordingSaved (recorder did not write)"
    );

    // The WAV must exist on disk under data_dir/recordings.
    let rec_dir = tmp.path().join("recordings");
    let wavs: Vec<_> = std::fs::read_dir(&rec_dir)
        .expect("recordings dir")
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("wav"))
        .collect();
    assert!(!wavs.is_empty(), "no WAV written to {}", rec_dir.display());

    // Graceful shutdown.
    daemon.shutdown();
    daemon.join().await;
}

/// T4.1: spectrum (waterfall) frames are opt-in. A client that requests
/// `?spectrum=all` receives decodable spectrum binary frames; a client that only
/// requests `?audio=all` receives audio but never a spectrum frame (the server
/// filters spectrum to the default `none`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spectrum_frames_arrive_only_when_requested() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("warn,hertz_daemon=info"))
        .try_init();

    let tmp = tempfile::tempdir().expect("tmp");
    let pairs = Arc::new(build_mock_iq_pairs());
    let pairs_for_closure = Arc::clone(&pairs);
    let factory = MockFactory::from_closure(move |idx: u64| -> (u8, u8) {
        pairs_for_closure[(idx as usize) % pairs_for_closure.len()]
    });

    let listen = free_port();
    let config = DaemonConfig {
        daemon: DaemonSettings {
            listen: listen.clone(),
            data_dir: tmp.path().to_string_lossy().to_string(),
            auth_token: None,
        },
        dongles: vec![DongleConfig {
            serial: "MOCKSPEC".into(),
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
    };

    let daemon = Daemon::start(config, Arc::new(factory), ChannelDb::new())
        .await
        .expect("daemon start");

    // Prime the worker + DSP thread.
    tokio::time::sleep(Duration::from_millis(300)).await;

    use futures::StreamExt;
    use hertz_types::wire::{decode_spectrum_frame, SPECTRUM_FRAME_MAGIC};
    use tokio_tungstenite::tungstenite::Message;

    // --- Connection A: requests spectrum. Must receive decodable spectrum frames.
    let listen_a = listen.clone();
    let collect_spectrum = tokio::spawn(async move {
        let url = format!("ws://{listen_a}/stream?spectrum=all");
        let mut ws = tokio::time::timeout(Duration::from_secs(3), async {
            tokio_tungstenite::connect_async(&url).await
        })
        .await
        .expect("ws A connect timeout")
        .expect("ws A connect")
        .0;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut spectrum_count = 0usize;
        let mut non_spectrum_binary = 0usize;
        loop {
            match tokio::time::timeout_at(deadline, ws.next()).await {
                Err(_) => break,
                Ok(None) => break,
                Ok(Some(Err(e))) => panic!("ws A error: {e}"),
                Ok(Some(Ok(msg))) => match msg {
                    Message::Binary(b) => {
                        let bytes: &[u8] = b.as_ref();
                        if !bytes.is_empty() && bytes[0] == SPECTRUM_FRAME_MAGIC {
                            let dec = decode_spectrum_frame(bytes).expect("spectrum decode");
                            assert!(
                                !dec.bins_db.is_empty(),
                                "spectrum frame carried no bins (n={})",
                                dec.bins_db.len()
                            );
                            assert_eq!(dec.dongle_idx, 0);
                            assert!(
                                (dec.center_hz - 156_800_000.0).abs() < 1e-6,
                                "center_hz {}",
                                dec.center_hz
                            );
                            assert!(
                                (dec.span_hz - 240_000.0).abs() < 1.0,
                                "span_hz {}",
                                dec.span_hz
                            );
                            spectrum_count += 1;
                            if spectrum_count >= 3 {
                                break;
                            }
                        } else {
                            non_spectrum_binary += 1;
                        }
                    }
                    Message::Text(_) => {} // Hello / events are fine
                    Message::Close(_) => break,
                    _ => {}
                },
            }
        }
        let _ = ws.close(None).await;
        (spectrum_count, non_spectrum_binary)
    });

    // --- Connection B: requests audio only. Must get audio, never spectrum.
    let listen_b = listen;
    let collect_audio = tokio::spawn(async move {
        let url = format!("ws://{listen_b}/stream?audio=all");
        let mut ws = tokio::time::timeout(Duration::from_secs(3), async {
            tokio_tungstenite::connect_async(&url).await
        })
        .await
        .expect("ws B connect timeout")
        .expect("ws B connect")
        .0;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut audio_frames = 0usize;
        let mut spectrum_leak = 0usize;
        let mut first_audio_at: Option<tokio::time::Instant> = None;
        loop {
            match tokio::time::timeout_at(deadline, ws.next()).await {
                Err(_) => break,
                Ok(None) => break,
                Ok(Some(Err(e))) => panic!("ws B error: {e}"),
                Ok(Some(Ok(msg))) => match msg {
                    Message::Binary(b) => {
                        let bytes: &[u8] = b.as_ref();
                        if !bytes.is_empty() && bytes[0] == SPECTRUM_FRAME_MAGIC {
                            spectrum_leak += 1;
                        } else {
                            audio_frames += 1;
                            if first_audio_at.is_none() {
                                first_audio_at = Some(tokio::time::Instant::now());
                            }
                        }
                    }
                    Message::Text(_) => {}
                    Message::Close(_) => break,
                    _ => {}
                },
            }
            // Once we've seen audio, keep draining ~1s to surface any spectrum
            // leak while the DSP is actively publishing, then stop.
            if let Some(t0) = first_audio_at {
                if t0.elapsed() >= Duration::from_secs(1) {
                    break;
                }
            }
        }
        let _ = ws.close(None).await;
        (audio_frames, spectrum_leak)
    });

    let (a_res, b_res) = tokio::time::timeout(Duration::from_secs(40), async {
        tokio::join!(collect_spectrum, collect_audio)
    })
    .await
    .expect("overall test timeout");

    let (spectrum_count, non_spectrum_binary) = a_res.expect("A join");
    let (audio_frames, spectrum_leak) = b_res.expect("B join");

    assert!(
        spectrum_count >= 3,
        "expected ≥3 spectrum frames on ?spectrum=all, got {spectrum_count}"
    );
    assert_eq!(
        non_spectrum_binary, 0,
        "spectrum stream received a non-spectrum binary frame"
    );
    assert!(
        audio_frames >= 1,
        "audio-only stream received no audio frames"
    );
    assert_eq!(
        spectrum_leak, 0,
        "audio-only stream leaked a spectrum frame (filter not applied)"
    );

    daemon.shutdown();
    daemon.join().await;
}

/// Minimal blocking HTTP/1.0 GET used to read `/api/status` as JSON. (kept simple
/// and dependency-free; tokio-tungstenite only speaks WebSocket).
async fn get_status_json(listen: &str) -> serde_json::Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(listen)
        .await
        .expect("http connect");
    s.write_all(b"GET /api/status HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .await
        .expect("http write");
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.expect("http read");
    let text = String::from_utf8_lossy(&buf).into_owned();
    let body = text.split("\r\n\r\n").nth(1).unwrap_or(&text);
    serde_json::from_str(body).expect("status json")
}

/// Startup open failure → offline, then reconnect-by-serial retry brings the dongle
/// fully online and its DSP thread runs. The first factory open returns NotFound
/// (device not enumerated yet); later opens succeed. Mirrors the live failure mode:
/// usbipd attach dropped at boot, dongle enumerates later.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dongle_reconnects_after_startup_open_failure() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("warn,hertz_daemon=info"))
        .try_init();

    let tmp = tempfile::tempdir().expect("tmp");
    let pairs = Arc::new(build_mock_iq_pairs());
    let factory = MockFactory::from_fallible_closure(
        move |attempt| -> Result<Box<dyn SdrDevice>, SdrError> {
            // Attempt 0 (the startup open) fails: the dongle isn't there yet.
            if attempt == 0 {
                return Err(SdrError::NotFound("dongle not enumerated yet".into()));
            }
            // Later attempts succeed with a fresh voice-burst mock device.
            let p = Arc::clone(&pairs);
            Ok(Box::new(MockSdr::new_with_closure(move |i| {
                let len = p.len();
                p[(i as usize) % len]
            })))
        },
    );

    let listen = free_port();
    let config = DaemonConfig {
        daemon: DaemonSettings {
            listen: listen.clone(),
            data_dir: tmp.path().to_string_lossy().to_string(),
            auth_token: None,
        },
        dongles: vec![DongleConfig {
            serial: "MOCKRECONN".into(),
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
    };

    // 200 ms retry interval so the test exercises the loop quickly (production
    // default is 5 s — see Daemon::start).
    let daemon = Daemon::start_with_reconnect_interval(
        config,
        Arc::new(factory),
        ChannelDb::new(),
        Duration::from_millis(200),
    )
    .await
    .expect("daemon start");

    // 1. The dongle is offline immediately after the startup open failed.
    let st = get_status_json(&listen).await;
    assert_eq!(
        st["dongles"][0]["online"].as_bool(),
        Some(false),
        "dongle should be offline after startup open failure"
    );

    // 2. Open the WS and wait for the offline→online transition + audio (proves the
    //    reconnect retry reopened the device, marked it online, and launched its DSP
    //    thread).
    let url = format!("ws://{listen}/stream?audio=all");
    let mut ws = tokio::time::timeout(Duration::from_secs(3), async {
        tokio_tungstenite::connect_async(&url).await
    })
    .await
    .expect("ws connect timeout")
    .expect("ws connect")
    .0;

    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let mut saw_online = false;
    let mut saw_audio = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Err(_) => break,
            Ok(None) => break,
            Ok(Some(Err(e))) => panic!("ws error: {e}"),
            Ok(Some(Ok(msg))) => match msg {
                Message::Text(t) => {
                    if let Ok(WsServerMsg::Event(hertz_types::Event::DongleStatus {
                        online: true,
                        ..
                    })) = serde_json::from_str(&t)
                    {
                        saw_online = true;
                    }
                }
                Message::Binary(_) => saw_audio = true,
                Message::Close(_) => break,
                _ => {}
            },
        }
        if saw_online && saw_audio {
            break;
        }
    }
    let _ = ws.close(None).await;

    assert!(
        saw_online,
        "never saw DongleStatus(online=true): reconnect did not bring the dongle online"
    );
    assert!(saw_audio, "DSP thread never produced audio after reconnect");

    // 3. /api/status now reflects online.
    let st2 = get_status_json(&listen).await;
    assert_eq!(
        st2["dongles"][0]["online"].as_bool(),
        Some(true),
        "dongle should be online after reconnect"
    );

    daemon.shutdown();
    daemon.join().await;
}
