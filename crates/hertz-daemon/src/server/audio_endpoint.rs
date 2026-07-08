//! `GET /audio?dongle=&channel=` — chunked raw PCM `audio/L16;rate=48000;channels=1`
//! (big-endian i16, gnosis-compatible; VLC-playable at the nav desk). Subscribes to
//! the audio bus and streams the requested dongle/channel.

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use futures::StreamExt;

use crate::audio::AudioFrame;
use crate::runtime::DaemonState;
use crate::server::{audio_accepts, check_auth, AudioSel};

#[derive(serde::Deserialize, Debug, Default)]
pub struct AudioQuery {
    pub dongle: Option<String>,
    pub channel: Option<String>,
}

pub async fn audio_handler(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(q): Query<AudioQuery>,
) -> Response {
    if let Err(r) = check_auth(&state, &headers, peer) {
        return r;
    }
    let sel = match &q.dongle {
        Some(d) => AudioSel::One {
            dongle: d.clone(),
            channel: q.channel.clone(),
        },
        None => AudioSel::All,
    };

    let rx = state.bus.subscribe_audio();
    let stream = async_stream::stream! {
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    if !audio_accepts(&sel, &frame) {
                        continue;
                    }
                    let mut bytes = Vec::with_capacity(frame.pcm.len() * 2);
                    for &s in &frame.pcm {
                        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                        bytes.extend_from_slice(&v.to_be_bytes());
                    }
                    yield Ok::<_, std::convert::Infallible>(bytes);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "audio/L16;rate=48000;channels=1")
        .body(Body::from_stream(stream))
        .unwrap()
}
