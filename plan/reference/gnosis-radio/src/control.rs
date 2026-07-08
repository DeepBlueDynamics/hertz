use crate::agentic::SharedAgenticState;
use crate::broadcast::{AudioBroadcaster, AudioMessage};
use crate::channels::{channel_to_freq, freq_to_channel};
use crate::pipeline::SharedEntropyPool;
use crate::voicepaint;
use crate::web::RADIO_HTML;
use serde::{Deserialize, Serialize};
use std::io::Write as IoWrite;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const CONTROL_PORT: u16 = 9080;

#[derive(Serialize, Deserialize)]
struct StatusResponse {
    channel: Option<u8>,
    frequency_hz: u32,
    recording: bool,
    listen: bool,
}

#[derive(Deserialize)]
struct SetChannelRequest {
    channel: Option<u8>,
    frequency_hz: Option<u32>,
}

#[derive(Deserialize)]
struct SetRecordingRequest {
    recording: bool,
}

#[derive(Deserialize)]
struct SetListenRequest {
    listen: bool,
}

#[derive(Deserialize)]
struct SetSquelchRequest {
    squelch: f32,
}

#[derive(Clone)]
pub struct ControlState {
    pub channel: Arc<AtomicU32>,
    pub recording: Arc<AtomicBool>,
    pub listen: Arc<AtomicBool>,
    pub frequency_hz: Arc<AtomicU32>,
    pub squelch_margin: Arc<Mutex<f32>>,
}

fn json_header() -> tiny_http::Header {
    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()
}

fn html_header() -> tiny_http::Header {
    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap()
}

fn cors_header() -> tiny_http::Header {
    tiny_http::Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap()
}

fn png_header() -> tiny_http::Header {
    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"image/png"[..]).unwrap()
}

fn wav_header() -> tiny_http::Header {
    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"audio/wav"[..]).unwrap()
}

fn octet_stream_header() -> tiny_http::Header {
    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/octet-stream"[..]).unwrap()
}

/// Convert days since Unix epoch to (year, month, day). Civil calendar, valid through 2099.
fn epoch_days_to_ymd(days: i64) -> (i64, i64, i64) {
    // Algorithm from Howard Hinnant's chrono-compatible date library
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

pub fn start_control_server(
    state: ControlState,
    broadcaster: Arc<AudioBroadcaster>,
    running: Arc<AtomicBool>,
    agentic: SharedAgenticState,
    entropy_pool: SharedEntropyPool,
) {
    thread::spawn(move || {
        let server = match tiny_http::Server::http(("0.0.0.0", CONTROL_PORT)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("\n  ⚠️  PORT CONFLICT: HTTP server failed on port {}", CONTROL_PORT);
                eprintln!("  Error: {}", e);
                let err_str = e.to_string().to_lowercase();
                if err_str.contains("address") || err_str.contains("use") || err_str.contains("bind") {
                    eprintln!("\n  Another process is using port {}!", CONTROL_PORT);
                    eprintln!("  To find the conflicting process, run:");
                    eprintln!("    netstat -ano | findstr :{}", CONTROL_PORT);
                    eprintln!("  Then kill it with:");
                    eprintln!("    taskkill /F /PID <pid>\n");
                }
                return;
            }
        };

        println!(
            "Control server listening on http://0.0.0.0:{}",
            CONTROL_PORT
        );
        println!("  GET  /                    - VHF Radio Web UI");
        println!("  GET  /status              - Get current status");
        println!("  GET  /audio               - Raw PCM audio stream");
        println!("  POST /channel             - Set channel or frequency");
        println!("  POST /recording           - Toggle recording");
        println!("  POST /listen              - Toggle live audio output");
        println!("  POST /squelch             - Set squelch margin");
        println!("  GET  /api/status          - Full agentic state");
        println!("  GET  /api/activity        - Last 200 activity entries");
        println!("  GET  /api/transcriptions  - Last 50 transcriptions");
        println!("  GET  /api/screenshot      - Latest screenshot PNG");
        println!("  POST /api/screenshot      - Upload screenshot PNG");
        println!("  POST /api/voice-paint     - Trigger voice painting");
        println!("  GET  /api/voice-paint     - Get latest voice painting");
        println!("  POST /api/errors          - Frontend error reporting");
        println!("  GET  /api/entropy         - Drain entropy from radio noise");
        println!("  GET  /api/entropy/stream  - SSE entropy stream");
        println!("  GET  /api/time            - Current time from radio clock");
        println!("  GET  /api/layout          - Get window layout configuration");
        println!("  POST /api/layout          - Save window layout configuration");
        println!("  POST /api/layout/reset    - Reset to default layout\n");

        while running.load(Ordering::Relaxed) {
            let mut request = match server.recv_timeout(Duration::from_millis(500)) {
                Ok(Some(r)) => r,
                Ok(None) | Err(_) => continue,
            };

            let url = request.url().to_string();
            let method = request.method().to_string();
            eprintln!("[HTTP] {} {}", method, url);

            // Serve individual extract files
            if method == "GET" && url.starts_with("/api/extracts/") {
                let filename = &url["/api/extracts/".len()..];
                let safe = filename
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.');
                if safe && !filename.contains("..") {
                    let path = std::path::Path::new("extracts").join(filename);
                    if path.exists() {
                        match std::fs::read(&path) {
                            Ok(data) => {
                                let ct = if filename.ends_with(".wav") {
                                    wav_header()
                                } else if filename.ends_with(".png") {
                                    png_header()
                                } else {
                                    tiny_http::Header::from_bytes(
                                        &b"Content-Type"[..],
                                        &b"application/octet-stream"[..],
                                    )
                                    .unwrap()
                                };
                                let _ = request.respond(
                                    tiny_http::Response::from_data(data)
                                        .with_header(ct)
                                        .with_header(cors_header()),
                                );
                            }
                            Err(_) => {
                                let _ = request.respond(
                                    tiny_http::Response::from_string(
                                        "{\"error\":\"Read failed\"}",
                                    )
                                    .with_status_code(500)
                                    .with_header(json_header()),
                                );
                            }
                        }
                    } else {
                        let _ = request.respond(
                            tiny_http::Response::from_string("{\"error\":\"Not found\"}")
                                .with_status_code(404)
                                .with_header(json_header()),
                        );
                    }
                } else {
                    let _ = request.respond(
                        tiny_http::Response::from_string("{\"error\":\"Invalid filename\"}")
                            .with_status_code(400)
                            .with_header(json_header()),
                    );
                }
                continue;
            }

            // Serve individual recording WAV files
            if method == "GET" && url.starts_with("/api/recordings/") {
                let filename = &url["/api/recordings/".len()..];
                // Sanitize: only allow alphanumeric, underscore, dash, dot
                let safe = filename
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.');
                if safe && !filename.contains("..") && filename.ends_with(".wav") {
                    let path = std::path::Path::new("recordings").join(filename);
                    if path.exists() {
                        match std::fs::read(&path) {
                            Ok(data) => {
                                let _ = request.respond(
                                    tiny_http::Response::from_data(data)
                                        .with_header(wav_header())
                                        .with_header(cors_header()),
                                );
                            }
                            Err(_) => {
                                let _ = request.respond(
                                    tiny_http::Response::from_string("{\"error\":\"Read failed\"}")
                                        .with_status_code(500)
                                        .with_header(json_header()),
                                );
                            }
                        }
                    } else {
                        let _ = request.respond(
                            tiny_http::Response::from_string("{\"error\":\"Not found\"}")
                                .with_status_code(404)
                                .with_header(json_header()),
                        );
                    }
                } else {
                    let _ = request.respond(
                        tiny_http::Response::from_string("{\"error\":\"Invalid filename\"}")
                            .with_status_code(400)
                            .with_header(json_header()),
                    );
                }
                continue;
            }

            // Handle time endpoint — returns current epoch + UTC string
            if method == "GET" && (url == "/api/time" || url.starts_with("/api/time?")) {
                let epoch = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let secs_in_day = epoch % 86400;
                let h = secs_in_day / 3600;
                let m = (secs_in_day % 3600) / 60;
                let s = secs_in_day % 60;
                // Days since epoch for date calc (simplified: good through 2099)
                let days = (epoch / 86400) as i64;
                let (y, mo, d) = epoch_days_to_ymd(days);
                let utc = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z");
                let json = serde_json::json!({
                    "epoch": epoch,
                    "source": "system",
                    "utc": utc,
                });
                let _ = request.respond(
                    tiny_http::Response::from_string(json.to_string())
                        .with_header(json_header())
                        .with_header(cors_header()),
                );
                continue;
            }

            // Handle entropy stream — SSE, long-lived
            if method == "GET" && url.starts_with("/api/entropy/stream") {
                let ep = entropy_pool.clone();
                let r = running.clone();
                thread::spawn(move || {
                    handle_entropy_stream(request, ep, r);
                });
                continue;
            }

            // Handle entropy drain — supports ?bytes=N&format=hex|raw|json
            if method == "GET" && (url == "/api/entropy" || url.starts_with("/api/entropy?")) {
                let mut requested_bytes: usize = 32;
                let mut format = "json";
                if let Some(qs) = url.split('?').nth(1) {
                    for param in qs.split('&') {
                        let mut kv = param.splitn(2, '=');
                        if let (Some(k), Some(v)) = (kv.next(), kv.next()) {
                            match k {
                                "bytes" => {
                                    if let Ok(n) = v.parse::<usize>() {
                                        requested_bytes = n.min(4096);
                                    }
                                }
                                "format" => {
                                    if v == "hex" || v == "raw" {
                                        format = if v == "hex" { "hex" } else { "raw" };
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }

                if let Ok(mut pool) = entropy_pool.lock() {
                    let available = pool.len();
                    let drain_count = requested_bytes.min(available);
                    let drained: Vec<u8> = pool.drain(..drain_count).collect();
                    let remaining = pool.len();

                    let response = match format {
                        "raw" => {
                            tiny_http::Response::from_data(drained)
                                .with_header(octet_stream_header())
                                .with_header(cors_header())
                        }
                        "hex" => {
                            let hex: String = drained.iter().map(|b| format!("{:02x}", b)).collect();
                            tiny_http::Response::from_string(hex)
                                .with_header(tiny_http::Header::from_bytes(
                                    &b"Content-Type"[..],
                                    &b"text/plain"[..],
                                ).unwrap())
                                .with_header(cors_header())
                        }
                        _ => {
                            let hex: String = drained.iter().map(|b| format!("{:02x}", b)).collect();
                            let json = serde_json::json!({
                                "bytes_requested": requested_bytes,
                                "bytes_returned": drain_count,
                                "pool_remaining": remaining,
                                "entropy_hex": hex,
                            });
                            tiny_http::Response::from_string(json.to_string())
                                .with_header(json_header())
                                .with_header(cors_header())
                        }
                    };
                    let _ = request.respond(response);
                } else {
                    let _ = request.respond(
                        tiny_http::Response::from_string("{\"error\":\"State lock error\"}")
                            .with_status_code(500)
                            .with_header(json_header()),
                    );
                }
                continue;
            }

            // Handle audio stream separately (long-lived connection)
            if method == "GET" && url == "/audio" {
                let bc = broadcaster.clone();
                let r = running.clone();
                thread::spawn(move || {
                    handle_audio_stream(request, bc, r);
                });
                continue;
            }

            let response = match (method.as_str(), url.as_str()) {
                ("GET", "/") => {
                    eprintln!("[HTTP] Serving RADIO_HTML ({} bytes)", RADIO_HTML.len());
                    tiny_http::Response::from_string(RADIO_HTML)
                        .with_header(html_header())
                        .with_header(cors_header())
                }
                ("GET", "/test") => {
                    let test_html = r#"<!DOCTYPE html>
<html><head><title>Meridian Test</title></head>
<body style="background:#111;color:#0f0;font:14px monospace;padding:20px;">
<h2>Meridian Radio - Diagnostic Page</h2>
<div style="display:flex;gap:20px;">
<div style="flex:1;"><h3>System</h3><div id="log"></div></div>
<div style="flex:1;"><h3>Frontend Errors (from main page)</h3><div id="errlog" style="color:#f80;"></div></div>
<div style="flex:1;"><h3>Signal</h3><div id="signal"></div></div>
</div>
<script>
var log=document.getElementById('log');
var errlog=document.getElementById('errlog');
var sigEl=document.getElementById('signal');
function msg(s){var d=document.createElement('div');d.textContent=s;log.appendChild(d);log.scrollTop=log.scrollHeight;}
function errmsg(s){var d=document.createElement('div');d.textContent=s;errlog.insertBefore(d,errlog.firstChild);while(errlog.children.length>50)errlog.removeChild(errlog.lastChild);}
msg('Page loaded OK');

msg('Connecting WS...');
var ws=new WebSocket('ws://'+location.hostname+':9081');
ws.binaryType='arraybuffer';
var count=0,lastSig='';
ws.onopen=function(){msg('WS OPEN - receiving data');};
ws.onclose=function(e){msg('WS CLOSE code='+e.code);};
ws.onerror=function(){msg('WS ERROR');};
ws.onmessage=function(e){
  count++;
  if(e.data instanceof ArrayBuffer){
    if(count<=5||count%200===0) msg('#'+count+' audio '+e.data.byteLength+'B');
  } else {
    var d=JSON.parse(e.data);
    if(d.type==='signal_level'){
      lastSig='Signal: '+d.signal_db.toFixed(1)+'dB | Noise: '+d.noise_floor.toFixed(1)+'dB | Squelch: '+(d.squelch_open?'OPEN':'closed');
      sigEl.textContent=lastSig+' (msg #'+count+')';
    } else {
      msg('#'+count+' '+d.type+': '+JSON.stringify(d).slice(0,100));
    }
  }
};

msg('Fetching /status...');
fetch('/status').then(function(r){return r.json();}).then(function(d){
  msg('Status: CH'+d.channel+' '+(d.frequency_hz/1e6).toFixed(3)+'MHz rec='+d.recording+' listen='+d.listen);
}).catch(function(e){msg('Fetch error: '+e);});

msg('Fetching /api/status...');
fetch('/api/status').then(function(r){return r.json();}).then(function(d){
  msg('Signal: '+d.signal_db.toFixed(1)+'dB | Noise: '+d.noise_floor.toFixed(1)+'dB');
}).catch(function(e){msg('API error: '+e);});
</script></body></html>"#;
                    tiny_http::Response::from_string(test_html)
                        .with_header(html_header())
                        .with_header(cors_header())
                }
                ("GET", "/status") => {
                    let freq = state.frequency_hz.load(Ordering::Relaxed);
                    let channel = freq_to_channel(freq);
                    let recording = state.recording.load(Ordering::Relaxed);
                    let listen = state.listen.load(Ordering::Relaxed);
                    let status = StatusResponse {
                        channel,
                        frequency_hz: freq,
                        recording,
                        listen,
                    };
                    let json = serde_json::to_string(&status).unwrap();
                    tiny_http::Response::from_string(json)
                        .with_header(json_header())
                        .with_header(cors_header())
                }
                ("POST", "/channel") => {
                    let mut content = String::new();
                    if request.as_reader().read_to_string(&mut content).is_ok() {
                        if let Ok(req) = serde_json::from_str::<SetChannelRequest>(&content) {
                            if let Some(ch) = req.channel {
                                if let Some(freq) = channel_to_freq(ch) {
                                    state.frequency_hz.store(freq, Ordering::Relaxed);
                                    state.channel.store(ch as u32, Ordering::Relaxed);
                                    tiny_http::Response::from_string(format!(
                                        "{{\"status\":\"ok\",\"channel\":{},\"frequency_hz\":{}}}",
                                        ch, freq
                                    ))
                                    .with_header(json_header())
                                    .with_header(cors_header())
                                } else {
                                    tiny_http::Response::from_string(
                                        "{\"error\":\"Invalid channel\"}",
                                    )
                                    .with_status_code(400)
                                    .with_header(json_header())
                                }
                            } else if let Some(freq) = req.frequency_hz {
                                state.frequency_hz.store(freq, Ordering::Relaxed);
                                if let Some(c) = freq_to_channel(freq) {
                                    state.channel.store(c as u32, Ordering::Relaxed);
                                }
                                tiny_http::Response::from_string(format!(
                                    "{{\"status\":\"ok\",\"frequency_hz\":{}}}",
                                    freq
                                ))
                                .with_header(json_header())
                                .with_header(cors_header())
                            } else {
                                tiny_http::Response::from_string(
                                    "{\"error\":\"Must provide channel or frequency_hz\"}",
                                )
                                .with_status_code(400)
                                .with_header(json_header())
                            }
                        } else {
                            tiny_http::Response::from_string("{\"error\":\"Invalid JSON\"}")
                                .with_status_code(400)
                                .with_header(json_header())
                        }
                    } else {
                        tiny_http::Response::from_string(
                            "{\"error\":\"Failed to read request body\"}",
                        )
                        .with_status_code(400)
                        .with_header(json_header())
                    }
                }
                ("POST", "/recording") => {
                    let mut content = String::new();
                    if request.as_reader().read_to_string(&mut content).is_ok() {
                        if let Ok(req) = serde_json::from_str::<SetRecordingRequest>(&content) {
                            state.recording.store(req.recording, Ordering::Relaxed);
                            tiny_http::Response::from_string(format!(
                                "{{\"status\":\"ok\",\"recording\":{}}}",
                                req.recording
                            ))
                            .with_header(json_header())
                            .with_header(cors_header())
                        } else {
                            tiny_http::Response::from_string("{\"error\":\"Invalid JSON\"}")
                                .with_status_code(400)
                                .with_header(json_header())
                        }
                    } else {
                        tiny_http::Response::from_string(
                            "{\"error\":\"Failed to read request body\"}",
                        )
                        .with_status_code(400)
                        .with_header(json_header())
                    }
                }
                ("POST", "/listen") => {
                    let mut content = String::new();
                    if request.as_reader().read_to_string(&mut content).is_ok() {
                        if let Ok(req) = serde_json::from_str::<SetListenRequest>(&content) {
                            state.listen.store(req.listen, Ordering::Relaxed);
                            tiny_http::Response::from_string(format!(
                                "{{\"status\":\"ok\",\"listen\":{}}}",
                                req.listen
                            ))
                            .with_header(json_header())
                            .with_header(cors_header())
                        } else {
                            tiny_http::Response::from_string("{\"error\":\"Invalid JSON\"}")
                                .with_status_code(400)
                                .with_header(json_header())
                        }
                    } else {
                        tiny_http::Response::from_string(
                            "{\"error\":\"Failed to read request body\"}",
                        )
                        .with_status_code(400)
                        .with_header(json_header())
                    }
                }
                ("POST", "/squelch") => {
                    let mut content = String::new();
                    if request.as_reader().read_to_string(&mut content).is_ok() {
                        if let Ok(req) = serde_json::from_str::<SetSquelchRequest>(&content) {
                            let val = req.squelch.clamp(0.0, 30.0);
                            if let Ok(mut margin) = state.squelch_margin.lock() {
                                *margin = val;
                            }
                            tiny_http::Response::from_string(format!(
                                "{{\"status\":\"ok\",\"squelch\":{}}}",
                                val
                            ))
                            .with_header(json_header())
                            .with_header(cors_header())
                        } else {
                            tiny_http::Response::from_string("{\"error\":\"Invalid JSON\"}")
                                .with_status_code(400)
                                .with_header(json_header())
                        }
                    } else {
                        tiny_http::Response::from_string(
                            "{\"error\":\"Failed to read request body\"}",
                        )
                        .with_status_code(400)
                        .with_header(json_header())
                    }
                }
                ("GET", "/api/status") => {
                    let freq = state.frequency_hz.load(Ordering::Relaxed);
                    let channel = freq_to_channel(freq);
                    let recording = state.recording.load(Ordering::Relaxed);
                    let listen = state.listen.load(Ordering::Relaxed);
                    let (signal_db, noise_floor, squelch_open, audio_flatness) =
                        if let Ok(s) = agentic.lock() {
                            (s.signal_db, s.noise_floor, s.squelch_open, s.audio_flatness)
                        } else {
                            (-100.0, -20.0, false, 0.8)
                        };
                    let json = serde_json::json!({
                        "channel": channel,
                        "frequency_hz": freq,
                        "recording": recording,
                        "listen": listen,
                        "signal_db": signal_db,
                        "noise_floor": noise_floor,
                        "squelch_open": squelch_open,
                        "audio_flatness": audio_flatness,
                    });
                    tiny_http::Response::from_string(json.to_string())
                        .with_header(json_header())
                        .with_header(cors_header())
                }
                ("GET", "/api/activity") => {
                    let entries = if let Ok(s) = agentic.lock() {
                        serde_json::to_string(&s.activity.iter().collect::<Vec<_>>())
                            .unwrap_or_else(|_| "[]".to_string())
                    } else {
                        "[]".to_string()
                    };
                    tiny_http::Response::from_string(entries)
                        .with_header(json_header())
                        .with_header(cors_header())
                }
                ("GET", "/api/transcriptions") => {
                    let entries = if let Ok(s) = agentic.lock() {
                        serde_json::to_string(&s.transcriptions.iter().collect::<Vec<_>>())
                            .unwrap_or_else(|_| "[]".to_string())
                    } else {
                        "[]".to_string()
                    };
                    tiny_http::Response::from_string(entries)
                        .with_header(json_header())
                        .with_header(cors_header())
                }
                ("GET", "/api/recordings") => {
                    let mut recordings = Vec::new();
                    if let Ok(entries) = std::fs::read_dir("recordings") {
                        let mut files: Vec<_> = entries
                            .filter_map(|e| e.ok())
                            .filter(|e| {
                                e.path()
                                    .extension()
                                    .map(|ext| ext == "wav")
                                    .unwrap_or(false)
                            })
                            .collect();
                        files.sort_by(|a, b| {
                            b.metadata()
                                .and_then(|m| m.modified())
                                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
                                .cmp(
                                    &a.metadata()
                                        .and_then(|m| m.modified())
                                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                                )
                        });
                        for entry in files.iter().take(100) {
                            let name = entry.file_name().to_string_lossy().to_string();
                            let size = entry
                                .metadata()
                                .map(|m| m.len())
                                .unwrap_or(0);
                            recordings.push(serde_json::json!({
                                "filename": name,
                                "url": format!("/api/recordings/{}", name),
                                "size": size,
                            }));
                        }
                    }
                    let json = serde_json::to_string(&recordings)
                        .unwrap_or_else(|_| "[]".to_string());
                    tiny_http::Response::from_string(json)
                        .with_header(json_header())
                        .with_header(cors_header())
                }
                ("GET", "/api/extracts") => {
                    let mut extracts = Vec::new();
                    let _ = std::fs::create_dir_all("extracts");
                    if let Ok(entries) = std::fs::read_dir("extracts") {
                        let mut files: Vec<_> = entries
                            .filter_map(|e| e.ok())
                            .collect();
                        files.sort_by(|a, b| {
                            b.metadata()
                                .and_then(|m| m.modified())
                                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
                                .cmp(
                                    &a.metadata()
                                        .and_then(|m| m.modified())
                                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                                )
                        });
                        for entry in files.iter().take(100) {
                            let name = entry.file_name().to_string_lossy().to_string();
                            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                            extracts.push(serde_json::json!({
                                "filename": name,
                                "url": format!("/api/extracts/{}", name),
                                "size": size,
                            }));
                        }
                    }
                    let json = serde_json::to_string(&extracts)
                        .unwrap_or_else(|_| "[]".to_string());
                    tiny_http::Response::from_string(json)
                        .with_header(json_header())
                        .with_header(cors_header())
                }
                ("POST", "/api/extracts") => {
                    let _ = std::fs::create_dir_all("extracts");
                    let mut body = Vec::new();
                    let content_type = request
                        .headers()
                        .iter()
                        .find(|h| h.field.as_str().to_string().to_lowercase() == "content-type")
                        .map(|h| h.value.as_str().to_string())
                        .unwrap_or_default();
                    if request.as_reader().read_to_end(&mut body).is_ok() && !body.is_empty() {
                        let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
                        let ext = if content_type.contains("wav") {
                            "wav"
                        } else if content_type.contains("png") || content_type.contains("image") {
                            "png"
                        } else {
                            "stft.gz"
                        };
                        let filename = format!("extract_{}.{}", ts, ext);
                        let path = std::path::Path::new("extracts").join(&filename);
                        match std::fs::write(&path, &body) {
                            Ok(_) => {
                                let json = serde_json::json!({
                                    "status": "ok",
                                    "filename": filename,
                                    "url": format!("/api/extracts/{}", filename),
                                    "size": body.len(),
                                });
                                tiny_http::Response::from_string(json.to_string())
                                    .with_header(json_header())
                                    .with_header(cors_header())
                            }
                            Err(e) => {
                                tiny_http::Response::from_string(
                                    format!("{{\"error\":\"{}\"}}", e),
                                )
                                .with_status_code(500)
                                .with_header(json_header())
                            }
                        }
                    } else {
                        tiny_http::Response::from_string("{\"error\":\"Empty body\"}")
                            .with_status_code(400)
                            .with_header(json_header())
                    }
                }
                ("GET", "/api/screenshot") => {
                    if let Ok(s) = agentic.lock() {
                        if let Some(ref png) = s.screenshot_png {
                            tiny_http::Response::from_data(png.clone())
                                .with_header(png_header())
                                .with_header(cors_header())
                        } else {
                            tiny_http::Response::from_string("{\"error\":\"No screenshot available\"}")
                                .with_status_code(404)
                                .with_header(json_header())
                                .with_header(cors_header())
                        }
                    } else {
                        tiny_http::Response::from_string("{\"error\":\"State lock error\"}")
                            .with_status_code(500)
                            .with_header(json_header())
                    }
                }
                ("POST", "/api/screenshot") => {
                    let mut body = Vec::new();
                    if request.as_reader().read_to_end(&mut body).is_ok() && !body.is_empty() {
                        if let Ok(mut s) = agentic.lock() {
                            s.screenshot_png = Some(body);
                        }
                        tiny_http::Response::from_string("{\"status\":\"ok\"}")
                            .with_header(json_header())
                            .with_header(cors_header())
                    } else {
                        tiny_http::Response::from_string("{\"error\":\"Empty body\"}")
                            .with_status_code(400)
                            .with_header(json_header())
                    }
                }
                ("POST", "/api/voice-paint") => {
                    let ag = agentic.clone();
                    let bc = broadcaster.clone();
                    match voicepaint::trigger_voice_paint(ag, bc) {
                        Ok(painting) => {
                            let json = serde_json::to_string(&painting)
                                .unwrap_or_else(|_| "{}".to_string());
                            tiny_http::Response::from_string(json)
                                .with_header(json_header())
                                .with_header(cors_header())
                        }
                        Err(e) => {
                            let json = serde_json::json!({"error": e});
                            tiny_http::Response::from_string(json.to_string())
                                .with_status_code(500)
                                .with_header(json_header())
                                .with_header(cors_header())
                        }
                    }
                }
                ("GET", "/api/voice-paint") => {
                    if let Ok(s) = agentic.lock() {
                        if let Some(ref vp) = s.voice_painting {
                            let json = serde_json::to_string(vp)
                                .unwrap_or_else(|_| "{}".to_string());
                            tiny_http::Response::from_string(json)
                                .with_header(json_header())
                                .with_header(cors_header())
                        } else {
                            tiny_http::Response::from_string("{\"error\":\"No voice painting available\"}")
                                .with_status_code(404)
                                .with_header(json_header())
                                .with_header(cors_header())
                        }
                    } else {
                        tiny_http::Response::from_string("{\"error\":\"State lock error\"}")
                            .with_status_code(500)
                            .with_header(json_header())
                    }
                }
                ("POST", "/api/clean-transcript") => {
                    let mut body = Vec::new();
                    if request.as_reader().read_to_end(&mut body).is_ok() && !body.is_empty() {
                        let raw = String::from_utf8_lossy(&body).to_string();
                        match clean_transcript(&raw) {
                            Ok(cleaned) => {
                                let json = serde_json::json!({"cleaned": cleaned});
                                tiny_http::Response::from_string(json.to_string())
                                    .with_header(json_header())
                                    .with_header(cors_header())
                            }
                            Err(e) => {
                                let json = serde_json::json!({"cleaned": raw, "error": e});
                                tiny_http::Response::from_string(json.to_string())
                                    .with_header(json_header())
                                    .with_header(cors_header())
                            }
                        }
                    } else {
                        tiny_http::Response::from_string("{\"error\":\"Empty body\"}")
                            .with_status_code(400)
                            .with_header(json_header())
                    }
                }
                ("POST", "/api/errors") => {
                    let mut body = Vec::new();
                    if request.as_reader().read_to_end(&mut body).is_ok() && !body.is_empty() {
                        let text = String::from_utf8_lossy(&body);
                        eprintln!("[FRONTEND] {}", text);
                        tiny_http::Response::from_string("{\"status\":\"ok\"}")
                            .with_header(json_header())
                            .with_header(cors_header())
                    } else {
                        tiny_http::Response::from_string("{\"error\":\"Empty body\"}")
                            .with_status_code(400)
                            .with_header(json_header())
                    }
                }
                ("GET", "/api/layout") => {
                    if let Ok(s) = agentic.lock() {
                        let json = serde_json::json!({
                            "windows": s.layout
                        });
                        tiny_http::Response::from_string(json.to_string())
                            .with_header(json_header())
                            .with_header(cors_header())
                    } else {
                        tiny_http::Response::from_string("{\"error\":\"State lock error\"}")
                            .with_status_code(500)
                            .with_header(json_header())
                    }
                }
                ("POST", "/api/layout") => {
                    let mut content = String::new();
                    if request.as_reader().read_to_string(&mut content).is_ok() {
                        #[derive(serde::Deserialize)]
                        struct LayoutRequest {
                            windows: crate::agentic::WindowLayout,
                        }
                        if let Ok(req) = serde_json::from_str::<LayoutRequest>(&content) {
                            if let Ok(mut s) = agentic.lock() {
                                s.layout = req.windows;
                                tiny_http::Response::from_string("{\"status\":\"ok\"}")
                                    .with_header(json_header())
                                    .with_header(cors_header())
                            } else {
                                tiny_http::Response::from_string("{\"error\":\"State lock error\"}")
                                    .with_status_code(500)
                                    .with_header(json_header())
                            }
                        } else {
                            tiny_http::Response::from_string("{\"error\":\"Invalid JSON\"}")
                                .with_status_code(400)
                                .with_header(json_header())
                        }
                    } else {
                        tiny_http::Response::from_string(
                            "{\"error\":\"Failed to read request body\"}",
                        )
                        .with_status_code(400)
                        .with_header(json_header())
                    }
                }
                ("POST", "/api/layout/reset") => {
                    if let Ok(mut s) = agentic.lock() {
                        s.layout = crate::agentic::WindowLayout::default_layout();
                        let json = serde_json::json!({
                            "windows": s.layout
                        });
                        tiny_http::Response::from_string(json.to_string())
                            .with_header(json_header())
                            .with_header(cors_header())
                    } else {
                        tiny_http::Response::from_string("{\"error\":\"State lock error\"}")
                            .with_status_code(500)
                            .with_header(json_header())
                    }
                }
                ("GET", "/favicon.ico") => {
                    // Minimal 1x1 transparent ICO to suppress browser 404
                    #[rustfmt::skip]
                    static FAVICON: &[u8] = &[
                        0x00,0x00, // reserved
                        0x01,0x00, // ICO type
                        0x01,0x00, // 1 image
                        // image directory entry (16 bytes)
                        0x01, // width 1
                        0x01, // height 1
                        0x00, // no palette
                        0x00, // reserved
                        0x01,0x00, // 1 color plane
                        0x20,0x00, // 32 bpp
                        0x30,0x00,0x00,0x00, // 48 bytes of image data
                        0x16,0x00,0x00,0x00, // offset to image data (22)
                        // BMP info header (40 bytes)
                        0x28,0x00,0x00,0x00, // header size 40
                        0x01,0x00,0x00,0x00, // width 1
                        0x02,0x00,0x00,0x00, // height 2 (ICO doubles height)
                        0x01,0x00,             // 1 plane
                        0x20,0x00,             // 32 bpp
                        0x00,0x00,0x00,0x00,  // no compression
                        0x00,0x00,0x00,0x00,  // image size (can be 0)
                        0x00,0x00,0x00,0x00,  // x ppm
                        0x00,0x00,0x00,0x00,  // y ppm
                        0x00,0x00,0x00,0x00,  // colors used
                        0x00,0x00,0x00,0x00,  // important colors
                        // pixel data: 1 pixel BGRA (transparent)
                        0x00,0x00,0x00,0x00,
                        // AND mask: 1 byte padded to 4
                        0x80,0x00,0x00,0x00,
                    ];
                    let ico_header = tiny_http::Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"image/x-icon"[..],
                    )
                    .unwrap();
                    let cache_header = tiny_http::Header::from_bytes(
                        &b"Cache-Control"[..],
                        &b"public, max-age=604800"[..],
                    )
                    .unwrap();
                    tiny_http::Response::from_data(FAVICON.to_vec())
                        .with_header(ico_header)
                        .with_header(cache_header)
                        .with_header(cors_header())
                }
                _ => tiny_http::Response::from_string("{\"error\":\"Not found\"}")
                    .with_status_code(404)
                    .with_header(json_header()),
            };

            let _ = request.respond(response);
        }
    });
}

const CLEAN_PROMPT: &str = "You clean up marine VHF radio transcriptions. Convert the raw Whisper output into concise shorthand. Rules: fix obvious misheard words, use standard marine abbreviations (V/L=vessel, STN=station, CH=channel, SEC=securite, PAN=pan-pan, MSG=message, INBD/OUTBD=inbound/outbound, NM=nautical miles, HDG=heading, ETA=estimated time), remove filler/repetition, keep it short. Return ONLY the cleaned text, nothing else.";

fn clean_transcript(raw: &str) -> Result<String, String> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .map_err(|_| "ANTHROPIC_API_KEY not set".to_string())?;

    let body = serde_json::json!({
        "model": "claude-haiku-4-20250514",
        "max_tokens": 256,
        "messages": [{
            "role": "user",
            "content": format!("{}\n\nRaw transcript:\n{}", CLEAN_PROMPT, raw)
        }]
    });

    let resp = ureq::post("https://api.anthropic.com/v1/messages")
        .set("x-api-key", &api_key)
        .set("anthropic-version", "2023-06-01")
        .set("content-type", "application/json")
        .send_string(&body.to_string())
        .map_err(|e| format!("API error: {}", e))?;

    let resp_text = resp.into_string().map_err(|e| format!("Read error: {}", e))?;

    #[derive(serde::Deserialize)]
    struct Resp { content: Vec<Content> }
    #[derive(serde::Deserialize)]
    struct Content { text: Option<String> }

    let parsed: Resp = serde_json::from_str(&resp_text)
        .map_err(|e| format!("Parse error: {}", e))?;

    parsed.content.first()
        .and_then(|c| c.text.clone())
        .ok_or_else(|| "No text in response".to_string())
}

/// Handle GET /audio — raw PCM stream (16-bit signed, 48kHz, mono)
/// Content-Type: audio/L16;rate=48000;channels=1
fn handle_audio_stream(
    request: tiny_http::Request,
    broadcaster: Arc<AudioBroadcaster>,
    running: Arc<AtomicBool>,
) {
    let rx = broadcaster.subscribe();

    let mut writer = request.into_writer();

    // Write HTTP response header manually
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: audio/L16;rate=48000;channels=1\r\nTransfer-Encoding: chunked\r\nAccess-Control-Allow-Origin: *\r\nConnection: keep-alive\r\n\r\n"
    );
    if writer.write_all(header.as_bytes()).is_err() {
        return;
    }

    while running.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(AudioMessage::Audio { samples, .. }) => {
                // Convert f32 samples to 16-bit signed PCM
                let mut pcm_data = Vec::with_capacity(samples.len() * 2);
                for &s in &samples {
                    let s16 = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                    pcm_data.extend_from_slice(&s16.to_be_bytes());
                }

                // Write as chunked encoding
                let chunk_header = format!("{:x}\r\n", pcm_data.len());
                if writer.write_all(chunk_header.as_bytes()).is_err() {
                    break;
                }
                if writer.write_all(&pcm_data).is_err() {
                    break;
                }
                if writer.write_all(b"\r\n").is_err() {
                    break;
                }
                if writer.flush().is_err() {
                    break;
                }
            }
            Ok(_) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Send terminating chunk
    let _ = writer.write_all(b"0\r\n\r\n");
    let _ = writer.flush();
}

/// Handle GET /api/entropy/stream — Server-Sent Events stream of entropy hex.
/// Emits one event per second with whatever entropy has accumulated.
fn handle_entropy_stream(
    request: tiny_http::Request,
    pool: SharedEntropyPool,
    running: Arc<AtomicBool>,
) {
    let mut writer = request.into_writer();

    let header = "HTTP/1.1 200 OK\r\n\
                   Content-Type: text/event-stream\r\n\
                   Cache-Control: no-cache\r\n\
                   Access-Control-Allow-Origin: *\r\n\
                   Connection: keep-alive\r\n\r\n";
    if writer.write_all(header.as_bytes()).is_err() {
        return;
    }

    while running.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));

        let drained = if let Ok(mut p) = pool.lock() {
            if p.is_empty() {
                Vec::new()
            } else {
                let n = p.len().min(128); // cap per event
                p.drain(..n).collect()
            }
        } else {
            Vec::new()
        };

        let hex: String = drained.iter().map(|b| format!("{:02x}", b)).collect();
        let event = format!(
            "data: {{\"bytes\":{},\"hex\":\"{}\"}}\n\n",
            drained.len(),
            hex
        );
        if writer.write_all(event.as_bytes()).is_err() {
            break;
        }
        if writer.flush().is_err() {
            break;
        }
    }
}
