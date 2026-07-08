use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use tungstenite::accept;
use tungstenite::Message;

use crate::broadcast::{AudioBroadcaster, AudioMessage};

const WS_PORT: u16 = 9081;

/// Start WebSocket server on port 8081 in a background thread.
/// Each connected client subscribes to the broadcaster and receives
/// audio frames (binary) and metadata (JSON text).
pub fn start_websocket_server(
    broadcaster: Arc<AudioBroadcaster>,
    running: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let listener = match TcpListener::bind(("0.0.0.0", WS_PORT)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("\n  ⚠️  PORT CONFLICT: WebSocket server failed on port {}", WS_PORT);
                eprintln!("  Error: {}", e);
                if e.kind() == std::io::ErrorKind::AddrInUse {
                    eprintln!("\n  Another process is using port {}!", WS_PORT);
                    eprintln!("  To find the conflicting process, run:");
                    eprintln!("    netstat -ano | findstr :{}", WS_PORT);
                    eprintln!("  Then kill it with:");
                    eprintln!("    taskkill /F /PID <pid>\n");
                }
                return;
            }
        };
        listener.set_nonblocking(true).ok();
        println!("WebSocket server listening on ws://0.0.0.0:{}", WS_PORT);

        while running.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, addr)) => {
                    println!("WebSocket client connected: {}", addr);
                    let bc = broadcaster.clone();
                    let r = running.clone();
                    thread::spawn(move || {
                        handle_ws_client(stream, bc, r);
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => {
                    eprintln!("WebSocket accept error: {}", e);
                    thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    });
}

fn handle_ws_client(
    stream: TcpStream,
    broadcaster: Arc<AudioBroadcaster>,
    running: Arc<AtomicBool>,
) {
    stream.set_nonblocking(false).ok();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();

    let mut websocket = match accept(stream) {
        Ok(ws) => {
            eprintln!("[WS] handshake OK, client connected");
            ws
        }
        Err(e) => {
            eprintln!("[WS] handshake FAILED: {}", e);
            return;
        }
    };

    let rx = broadcaster.subscribe();
    eprintln!("[WS] subscribed to broadcaster, starting message loop");
    let mut ws_msg_count: u64 = 0;

    while running.load(Ordering::Relaxed) {
        match rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(msg) => {
                ws_msg_count += 1;
                if ws_msg_count <= 5 || ws_msg_count % 500 == 0 {
                    let kind = match &msg {
                        AudioMessage::Audio { samples, .. } => format!("Audio({}samp)", samples.len()),
                        AudioMessage::ChannelActivity { .. } => "ChannelActivity".to_string(),
                        AudioMessage::SquelchEvent { open, .. } => format!("Squelch(open={})", open),
                        AudioMessage::SignalLevel { signal_db, .. } => format!("SignalLevel({:.1}dB)", signal_db),
                        AudioMessage::Transcription { .. } => "Transcription".to_string(),
                        AudioMessage::VoicePaint { .. } => "VoicePaint".to_string(),
                    };
                    eprintln!("[WS] msg#{}: {}", ws_msg_count, kind);
                }
                let ws_msg = match &msg {
                    AudioMessage::Audio {
                        channel,
                        freq,
                        samples,
                        signal_db,
                    } => {
                        // Binary: 4 bytes channel_id + 4 bytes freq + 4 bytes signal_db + f32 PCM
                        let ch = channel.unwrap_or(0) as u32;
                        let mut data =
                            Vec::with_capacity(12 + samples.len() * 4);
                        data.extend_from_slice(&ch.to_le_bytes());
                        data.extend_from_slice(&freq.to_le_bytes());
                        data.extend_from_slice(&signal_db.to_le_bytes());
                        for &s in samples {
                            data.extend_from_slice(&s.to_le_bytes());
                        }
                        Message::Binary(data.into())
                    }
                    AudioMessage::ChannelActivity { active, noise_floor } => {
                        let json = serde_json::json!({
                            "type": "channel_activity",
                            "active": active,
                            "noise_floor": noise_floor,
                        });
                        Message::Text(json.to_string().into())
                    }
                    AudioMessage::SquelchEvent {
                        channel,
                        freq,
                        open,
                        signal_db,
                        classification,
                    } => {
                        let json = serde_json::json!({
                            "type": "squelch",
                            "channel": channel,
                            "freq": freq,
                            "open": open,
                            "signal_db": signal_db,
                            "classification": classification,
                        });
                        Message::Text(json.to_string().into())
                    }
                    AudioMessage::SignalLevel {
                        channel,
                        freq,
                        signal_db,
                        noise_floor,
                        squelch_open,
                        audio_flatness,
                    } => {
                        let json = serde_json::json!({
                            "type": "signal_level",
                            "channel": channel,
                            "freq": freq,
                            "signal_db": signal_db,
                            "noise_floor": noise_floor,
                            "squelch_open": squelch_open,
                            "audio_flatness": audio_flatness,
                        });
                        Message::Text(json.to_string().into())
                    }
                    AudioMessage::Transcription {
                        channel,
                        freq,
                        text,
                    } => {
                        let json = serde_json::json!({
                            "type": "transcription",
                            "channel": channel,
                            "freq": freq,
                            "text": text,
                        });
                        Message::Text(json.to_string().into())
                    }
                    AudioMessage::VoicePaint { painting } => {
                        let json = format!(
                            "{{\"type\":\"voice_paint\",\"painting\":{}}}",
                            painting
                        );
                        Message::Text(json.into())
                    }
                };

                if websocket.send(ws_msg).is_err() {
                    eprintln!("[WS] send failed, client disconnected (after {} msgs)", ws_msg_count);
                    break;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                eprintln!("[WS] broadcaster disconnected");
                break;
            }
        }
    }

    eprintln!("[WS] closing connection (sent {} msgs total)", ws_msg_count);
    let _ = websocket.close(None);
}

/// The embedded HTML/CSS/JS radio UI served at GET /
pub const RADIO_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Meridian Radio</title>
<style>
* { margin: 0; padding: 0; box-sizing: border-box; }
body { background: #1a1a2e; font-family: 'Courier New', 'Consolas', monospace; min-height: 100vh; color: #e0e0e0; overflow: hidden; }

/* Window System */
.window {
  position: fixed;
  background: linear-gradient(145deg, #2a2a3a, #1e1e2e);
  border-radius: 8px;
  box-shadow: 0 4px 20px rgba(0,0,0,0.5), inset 0 1px 0 rgba(255,255,255,0.05);
  border: 1px solid #333;
  display: flex;
  flex-direction: column;
  min-width: 150px;
  min-height: 80px;
  transition: box-shadow 0.15s;
}
.window.focused {
  box-shadow: 0 8px 32px rgba(0,0,0,0.7), inset 0 1px 0 rgba(255,255,255,0.08), 0 0 0 1px #00cc55;
}
.window.minimized .window-content { display: none; }
.window.minimized { min-height: auto; height: auto !important; }
.window-titlebar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 6px 10px;
  background: linear-gradient(180deg, #333 0%, #2a2a2a 100%);
  border-bottom: 1px solid #222;
  border-radius: 8px 8px 0 0;
  cursor: move;
  user-select: none;
  flex-shrink: 0;
}
.window.minimized .window-titlebar { border-radius: 8px; border-bottom: none; }
.window-title {
  font-size: 11px;
  color: #888;
  text-transform: uppercase;
  letter-spacing: 2px;
  font-weight: bold;
}
.window.focused .window-title { color: #00cc55; }
.window-buttons {
  display: flex;
  gap: 6px;
}
.window-btn {
  width: 14px;
  height: 14px;
  border-radius: 50%;
  border: none;
  cursor: pointer;
  font-size: 9px;
  line-height: 14px;
  text-align: center;
  color: transparent;
  transition: all 0.15s;
}
.window-btn:hover { color: #111; }
.window-btn.minimize { background: #ffcc00; }
.window-btn.minimize:hover { background: #ffdd44; }
.window-btn.close { background: #ff5555; }
.window-btn.close:hover { background: #ff7777; }
.window-content {
  flex: 1;
  overflow: auto;
  padding: 8px;
  min-height: 0;
}
.window-resize {
  position: absolute;
  bottom: 0;
  right: 0;
  width: 16px;
  height: 16px;
  cursor: se-resize;
  background: linear-gradient(135deg, transparent 50%, #444 50%);
  border-radius: 0 0 8px 0;
}
.window.minimized .window-resize { display: none; }

/* Reset Layout Button */
#resetLayoutBtn {
  position: fixed;
  top: 10px;
  right: 10px;
  z-index: 10000;
  padding: 6px 12px;
  background: #333;
  color: #888;
  border: 1px solid #555;
  border-radius: 4px;
  cursor: pointer;
  font-size: 10px;
  text-transform: uppercase;
  letter-spacing: 1px;
  font-family: 'Courier New', monospace;
}
#resetLayoutBtn:hover { background: #444; color: #ddd; }

/* LCD Panel Styles */
.lcd-panel { background: #0a1a0a; border: 2px solid #333; border-radius: 6px; padding: 12px; position: relative; }
.lcd-panel::before { content: ''; position: absolute; top: 0; left: 0; right: 0; bottom: 0; background: linear-gradient(rgba(0,255,0,0.02) 50%, transparent 50%); background-size: 100% 4px; pointer-events: none; border-radius: 4px; }
.lcd-row { display: flex; justify-content: space-between; align-items: center; margin-bottom: 6px; }
.lcd-row:last-child { margin-bottom: 0; }
.channel-display { font-size: 36px; font-weight: bold; color: #00ff66; text-shadow: 0 0 20px rgba(0,255,102,0.5); }
.channel-label { font-size: 14px; color: #00cc55; text-shadow: 0 0 10px rgba(0,204,85,0.3); }
.freq-display { font-size: 16px; color: #00dd55; }
.smeter-container { margin: 6px 0; }
.smeter-label { font-size: 9px; color: #666; margin-bottom: 3px; }
.smeter { display: flex; gap: 2px; height: 16px; align-items: flex-end; }
.smeter-bar { width: 6px; background: #1a3a1a; border: 1px solid #2a4a2a; border-radius: 1px; transition: all 0.1s; }
.smeter-bar.active { background: #00cc44; box-shadow: 0 0 4px rgba(0,204,68,0.5); }
.smeter-bar.active.high { background: #ffaa00; box-shadow: 0 0 4px rgba(255,170,0,0.5); }
.smeter-bar.active.over { background: #ff4444; box-shadow: 0 0 4px rgba(255,68,68,0.5); }
.smeter-scale { display: flex; gap: 2px; margin-top: 2px; }
.smeter-scale span { width: 6px; font-size: 6px; color: #555; text-align: center; }
.lcd-ticker { margin-top: 4px; padding: 3px 0 2px 0; border-top: 1px solid #1a2a1a; font-size: 9px; color: #00cc55; line-height: 1.3; text-shadow: 0 0 4px rgba(0,204,85,0.2); max-height: 36px; overflow-y: auto; overflow-x: hidden; }
.lcd-ticker.idle { color: #1a2a1a; }
.lcd-ticker .tx-ch { color: #00cc55; font-weight: bold; }
.lcd-ticker .tx-hdr { color: #1a4a1a; font-size: 8px; letter-spacing: 1px; white-space: nowrap; overflow: hidden; }
.lcd-ticker .tx-body { white-space: nowrap; overflow-x: auto; display: block; scrollbar-width: none; }
.lcd-ticker .tx-body::-webkit-scrollbar { display: none; }

/* Controls */
.controls-inner { display: flex; gap: 6px; align-items: center; flex-wrap: wrap; }
.ctrl-grp { display: flex; align-items: center; gap: 4px; padding: 3px 6px; border-radius: 4px; background: rgba(25,25,35,0.6); border: 1px solid #2a2a3a; }
.ctrl-grp label { font-size: 9px; color: #666; text-transform: uppercase; letter-spacing: 1px; }
input[type="range"] { -webkit-appearance: none; width: 70px; height: 5px; background: #333; border-radius: 3px; outline: none; }
input[type="range"]::-webkit-slider-thumb { -webkit-appearance: none; width: 14px; height: 14px; background: #666; border-radius: 50%; cursor: pointer; border: 2px solid #888; }
.btn { padding: 4px 10px; background: #333; color: #aaa; border: 1px solid #555; border-radius: 4px; cursor: pointer; font-size: 10px; text-transform: uppercase; letter-spacing: 1px; transition: all 0.15s; }
.btn:hover { background: #444; color: #ddd; }
.btn.active { background: #ff3333; color: white; border-color: #ff5555; box-shadow: 0 0 8px rgba(255,51,51,0.4); }
.btn.rec-active { background: #ff3333; color: white; animation: rec-blink 1s infinite; }
.btn.icon-btn { font-size: 13px; padding: 2px 5px; line-height: 1; background: transparent; border-color: #2a2a3a; }
@keyframes rec-blink { 50% { opacity: 0.7; } }
.volume-val, .squelch-val { font-size: 9px; color: #555; }
.indicator { display: flex; align-items: center; gap: 4px; font-size: 10px; color: #888; }
.indicator .dot { width: 10px; height: 10px; border-radius: 50%; background: #333; border: 1px solid #555; transition: all 0.15s; }
.indicator .dot.rx-active { background: #00ff66; box-shadow: 0 0 12px #00ff66; }
.indicator .dot.squelch-open { background: #00ff66; box-shadow: 0 0 12px #00ff66; }
.indicator .dot.squelch-closed { background: #333; }
.indicator .dot.connected { background: #00aaff; box-shadow: 0 0 8px #00aaff; }
.indicator .dot.disconnected { background: #ff3333; box-shadow: 0 0 8px #ff3333; }

/* Viz panels */
.viz-inner { background: #0a0a14; border-radius: 4px; }
canvas { width: 100%; border-radius: 3px; display: block; }

/* Activity panels */
.activity-inner { background: #0a0a14; border-radius: 4px; height: 100%; overflow-y: auto; }
.panel-title { font-size: 9px; color: #555; padding: 6px 8px 4px 8px; text-transform: uppercase; letter-spacing: 1px; border-bottom: 1px solid #222; }
.activity-entry { font-size: 10px; padding: 3px 8px; border-bottom: 1px solid #1a1a1a; color: #888; }
.activity-entry .time { color: #555; }
.activity-entry .ch { color: #00cc55; font-weight: bold; }
.activity-entry .open { color: #00ff66; }
.activity-entry .close { color: #ff6644; }

/* Extract panel */
.extract-inner { display: flex; align-items: center; gap: 8px; }
.extract-inner audio { flex: 1; height: 24px; filter: invert(0.8) hue-rotate(120deg); }

/* Play badges */
.play-badge { display: inline-block; background: #1a3a1a; color: #00cc55; border: 1px solid #2a4a2a; border-radius: 3px; padding: 1px 5px; font-size: 10px; cursor: pointer; opacity: 0.35; transition: all 0.2s; margin-left: 4px; vertical-align: middle; }
.play-badge.ready { opacity: 1; }
.play-badge:hover { background: #2a4a2a; color: #00ff66; box-shadow: 0 0 6px rgba(0,255,102,0.4); }
.play-badge .sz { font-size: 8px; color: #556; margin-left: 2px; }

::-webkit-scrollbar { width: 5px; }
::-webkit-scrollbar-track { background: #111; }
::-webkit-scrollbar-thumb { background: #333; border-radius: 3px; }
</style>
</head>
<body>
<div id="errFlash" style="display:none;position:fixed;top:0;left:0;right:0;height:3px;background:#ff3333;z-index:9999;opacity:1;transition:opacity 0.6s;"></div>
<button id="resetLayoutBtn" onclick="WM.resetLayout()">Reset Layout</button>
<script>
console.log('[DBG] === MERIDIAN RADIO BOOT ===');
console.log('[DBG] document.readyState:', document.readyState);
console.log('[DBG] location:', location.href);
console.time('[DBG] total-init');
/* Debug overlay — toggle with ~ key */
var _dbgEl=document.createElement('div');
_dbgEl.id='dbgOverlay';
_dbgEl.style.cssText='position:fixed;top:0;left:0;right:0;z-index:99999;background:rgba(0,0,0,0.92);color:#0f0;font:11px monospace;padding:8px 12px;max-height:40vh;overflow-y:auto;white-space:pre-wrap;border-bottom:2px solid #0f0;display:none;';
document.body.appendChild(_dbgEl);
var _dbgLines=[];
var _dbgVisible=false;
document.addEventListener('keydown',function(e){
  if(e.key==='~'||e.key==='`'){
    _dbgVisible=!_dbgVisible;
    _dbgEl.style.display=_dbgVisible?'block':'none';
    if(_dbgVisible) _dbgEl.scrollTop=_dbgEl.scrollHeight;
  }
});
function _dbg(msg){
  var now=new Date();
  var ts=now.getFullYear()+'-'+(now.getMonth()+1).toString().padStart(2,'0')+'-'+now.getDate().toString().padStart(2,'0')+' '+now.getHours().toString().padStart(2,'0')+':'+now.getMinutes().toString().padStart(2,'0')+':'+now.getSeconds().toString().padStart(2,'0')+'.'+now.getMilliseconds().toString().padStart(3,'0');
  var line=ts+' '+msg;
  _dbgLines.push(line);
  if(_dbgLines.length>100) _dbgLines.shift();
  _dbgEl.textContent=_dbgLines.join('\n');
  if(_dbgVisible) _dbgEl.scrollTop=_dbgEl.scrollHeight;
  console.log('[DBG]',msg);
  try{fetch('/api/errors',{method:'POST',body:line}).catch(function(){});}catch(e){}
}
</script>
<script>
(function(){
  var errFlash=document.getElementById('errFlash');
  var seen={};
  function flash(){errFlash.style.display='block';errFlash.style.opacity='1';setTimeout(function(){errFlash.style.opacity='0';setTimeout(function(){errFlash.style.display='none';},600);},400);}
  function send(payload){
    var key=payload.message+'|'+(payload.source||'')+'|'+(payload.lineno||0);
    if(seen[key])return;seen[key]=1;
    flash();
    try{fetch('/api/errors',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(payload)}).catch(function(){});}catch(e){}
  }
  window.onerror=function(message,source,lineno,colno,error){
    send({type:'uncaught-error',message:String(message),source:source||'',lineno:lineno||0,colno:colno||0,stack:(error&&error.stack)||'',url:location.href,userAgent:navigator.userAgent,timestamp:new Date().toISOString()});
  };
  window.onunhandledrejection=function(event){
    var reason=event.reason||{};
    send({type:'unhandled-rejection',message:String(reason),source:'',lineno:0,colno:0,stack:(reason&&reason.stack)||'',url:location.href,userAgent:navigator.userAgent,timestamp:new Date().toISOString()});
  };
})();

_dbg('error handler wired');
/* ══════════════════════════════════════════
   Window Manager
   ══════════════════════════════════════════ */
var WM = (function(){
  var windows = {};
  var zCounter = 100;
  var dragState = null;
  var resizeState = null;
  var STORAGE_KEY = 'meridian_window_layout';

  function createWindow(id, title, contentHtml, x, y, w, h) {
    _dbg('createWindow: '+id+' '+title+' '+x+','+y+' '+w+'x'+h);
    var win = document.createElement('div');
    win.className = 'window';
    win.id = 'win-' + id;
    win.style.left = x + 'px';
    win.style.top = y + 'px';
    win.style.width = w + 'px';
    win.style.height = h + 'px';
    win.style.zIndex = zCounter++;

    win.innerHTML =
      '<div class="window-titlebar">' +
        '<span class="window-title">' + title + '</span>' +
        '<div class="window-buttons">' +
          '<button class="window-btn minimize" title="Minimize">-</button>' +
          '<button class="window-btn close" title="Close">x</button>' +
        '</div>' +
      '</div>' +
      '<div class="window-content">' + contentHtml + '</div>' +
      '<div class="window-resize"></div>';

    document.body.appendChild(win);

    windows[id] = {
      el: win,
      id: id,
      title: title,
      minimized: false,
      x: x, y: y, w: w, h: h
    };

    // Event handlers
    var titlebar = win.querySelector('.window-titlebar');
    titlebar.addEventListener('mousedown', function(e) {
      if (e.target.classList.contains('window-btn')) return;
      bringToFront(id);
      dragState = { id: id, startX: e.clientX, startY: e.clientY, origX: win.offsetLeft, origY: win.offsetTop };
      e.preventDefault();
    });

    win.addEventListener('mousedown', function() { bringToFront(id); });

    win.querySelector('.window-btn.minimize').addEventListener('click', function() { toggleMinimize(id); });
    win.querySelector('.window-btn.close').addEventListener('click', function() { toggleMinimize(id); }); // Close = minimize for now

    var resizer = win.querySelector('.window-resize');
    resizer.addEventListener('mousedown', function(e) {
      bringToFront(id);
      resizeState = { id: id, startX: e.clientX, startY: e.clientY, origW: win.offsetWidth, origH: win.offsetHeight };
      e.preventDefault();
      e.stopPropagation();
    });

    return win;
  }

  function bringToFront(id) {
    for (var wid in windows) {
      windows[wid].el.classList.remove('focused');
    }
    if (windows[id]) {
      windows[id].el.style.zIndex = zCounter++;
      windows[id].el.classList.add('focused');
    }
  }

  function toggleMinimize(id) {
    var w = windows[id];
    if (!w) return;
    w.minimized = !w.minimized;
    if (w.minimized) {
      w.el.classList.add('minimized');
    } else {
      w.el.classList.remove('minimized');
    }
    saveLayout();
  }

  function saveLayout() {
    var layout = {};
    for (var id in windows) {
      var w = windows[id];
      layout[id] = {
        x: w.el.offsetLeft,
        y: w.el.offsetTop,
        w: w.el.offsetWidth,
        h: w.el.offsetHeight,
        minimized: w.minimized,
        z: parseInt(w.el.style.zIndex) || 100
      };
    }
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(layout));
    } catch(e) {}
  }

  function loadLayout() {
    try {
      var saved = localStorage.getItem(STORAGE_KEY);
      if (saved) return JSON.parse(saved);
    } catch(e) {}
    return null;
  }

  function applyLayout(layout) {
    for (var id in layout) {
      var w = windows[id];
      if (!w) continue;
      var l = layout[id];
      w.el.style.left = l.x + 'px';
      w.el.style.top = l.y + 'px';
      w.el.style.width = l.w + 'px';
      w.el.style.height = l.h + 'px';
      w.el.style.zIndex = l.z || 100;
      w.minimized = l.minimized;
      if (l.minimized) {
        w.el.classList.add('minimized');
      } else {
        w.el.classList.remove('minimized');
      }
    }
  }

  function resetLayout() {
    try { localStorage.removeItem(STORAGE_KEY); } catch(e) {}
    location.reload();
  }

  // Global mouse handlers for drag/resize
  document.addEventListener('mousemove', function(e) {
    if (dragState) {
      var dx = e.clientX - dragState.startX;
      var dy = e.clientY - dragState.startY;
      var w = windows[dragState.id];
      if (w) {
        var newX = Math.max(0, dragState.origX + dx);
        var newY = Math.max(0, dragState.origY + dy);
        w.el.style.left = newX + 'px';
        w.el.style.top = newY + 'px';
      }
    }
    if (resizeState) {
      var dx = e.clientX - resizeState.startX;
      var dy = e.clientY - resizeState.startY;
      var w = windows[resizeState.id];
      if (w) {
        var newW = Math.max(150, resizeState.origW + dx);
        var newH = Math.max(80, resizeState.origH + dy);
        w.el.style.width = newW + 'px';
        w.el.style.height = newH + 'px';
      }
    }
  });

  document.addEventListener('mouseup', function() {
    if (dragState || resizeState) {
      saveLayout();
    }
    dragState = null;
    resizeState = null;
  });

  return {
    createWindow: createWindow,
    bringToFront: bringToFront,
    loadLayout: loadLayout,
    applyLayout: applyLayout,
    resetLayout: resetLayout,
    saveLayout: saveLayout,
    windows: windows
  };
})();

/* ══════════════════════════════════════════
   Create Windows with Content
   ══════════════════════════════════════════ */

// Default positions (will be overridden by saved layout)
var defaultLayout = {
  radio:       { x: 20,  y: 20,  w: 340, h: 280 },
  clock:       { x: 20,  y: 310, w: 340, h: 280 },
  spectrogram: { x: 370, y: 20,  w: 600, h: 300 },
  analysis:    { x: 370, y: 330, w: 600, h: 260 },
  recordings:  { x: 980, y: 20,  w: 280, h: 570 }
};

// Radio Window (LCD Panel + Controls)
WM.createWindow('radio', 'Radio',
  '<div class="lcd-panel">' +
    '<div class="lcd-row">' +
      '<div style="flex:1;">' +
        '<div class="channel-display" id="channelNum">VHF CH --</div>' +
        '<div class="channel-label" id="channelLabel">SCAN</div>' +
        '<div class="freq-display" id="freqDisplay" style="margin-top:2px;">---.--- MHz</div>' +
      '</div>' +
      '<div style="display:flex;flex-direction:column;gap:4px;align-items:center;">' +
        '<div class="indicator"><div class="dot" id="rxDot"></div><span>RX</span></div>' +
        '<div class="indicator"><div class="dot" id="wsDot"></div><span>WS</span></div>' +
      '</div>' +
    '</div>' +
    '<div class="smeter-container">' +
      '<div class="smeter-label">S</div>' +
      '<div class="smeter" id="smeter"></div>' +
      '<div class="smeter-scale" id="smeterScale"></div>' +
    '</div>' +
    '<div class="lcd-ticker idle" id="txbox">STANDBY</div>' +
  '</div>' +
  '<div style="padding:6px 8px;display:flex;flex-direction:column;gap:4px;">' +
    '<div class="ctrl-grp" style="justify-content:space-between;">' +
      '<label>Vol</label>' +
      '<input type="range" id="volumeSlider" min="0" max="100" value="85">' +
      '<span class="volume-val" id="volumeVal">85%</span>' +
      '<button class="btn icon-btn" id="muteBtn" onclick="toggleMute()" title="Mute">&#x1f50a;</button>' +
    '</div>' +
    '<div class="ctrl-grp" style="justify-content:space-between;">' +
      '<div class="indicator"><div class="dot" id="squelchDot"></div></div>' +
      '<label>Sql</label>' +
      '<input type="range" id="squelchSlider" min="0" max="30" value="12" step="0.5">' +
      '<span class="squelch-val" id="squelchVal">12dB</span>' +
    '</div>' +
    '<div style="text-align:center;">' +
      '<button class="btn" id="recBtn" onclick="toggleRecording()" style="padding:4px 16px;">REC</button>' +
    '</div>' +
  '</div>',
  defaultLayout.radio.x, defaultLayout.radio.y, defaultLayout.radio.w, defaultLayout.radio.h
);

// Clock Window
WM.createWindow('clock', 'Clock',
  '<div style="padding:6px 8px;font-family:monospace;">' +
    '<div id="gmtClock" style="font-size:18px;color:#00ff66;text-shadow:0 0 10px rgba(0,255,102,0.4);letter-spacing:2px;text-align:center;padding:4px 0;border-bottom:1px solid #222;">00:00:00 <span style="font-size:10px;color:#00cc55;">GMT</span></div>' +
    '<div id="worldTime" style="font-size:9px;color:#778;margin-top:6px;line-height:1.8;"></div>' +
  '</div>',
  defaultLayout.clock.x, defaultLayout.clock.y, defaultLayout.clock.w, defaultLayout.clock.h
);

// Spectrogram Window
WM.createWindow('spectrogram', 'Spectrogram',
  '<div class="viz-inner" style="height:100%;">' +
    '<div style="display:flex;justify-content:space-between;align-items:center;padding:4px 8px;border-bottom:1px solid #222;">' +
      '<span style="font-size:9px;color:#555;text-transform:uppercase;letter-spacing:1px;">STFT</span>' +
      '<div style="display:flex;align-items:center;gap:6px;">' +
        '<label style="font-size:8px;color:#555;display:flex;align-items:center;gap:2px;">Zoom<input type="range" id="timeSlider" min="1" max="8" value="2" style="width:50px;"><span class="volume-val" id="timeVal">2x</span></label>' +
        '<label style="font-size:8px;color:#555;cursor:pointer;display:flex;align-items:center;gap:3px;">' +
          '<input type="checkbox" id="autoExtract" style="width:10px;height:10px;accent-color:#00cc55;">AUTO' +
        '</label>' +
        '<button class="btn" id="extractBtn" onclick="extractAudio()" style="padding:2px 8px;font-size:9px;">EXT</button>' +
      '</div>' +
    '</div>' +
    '<div style="position:relative;height:calc(100% - 28px);">' +
      '<canvas id="sgCanvas" style="height:100%;"></canvas>' +
      '<div id="sgLabels" style="position:absolute;top:0;left:0;bottom:0;width:32px;pointer-events:none;"></div>' +
    '</div>' +
  '</div>',
  defaultLayout.spectrogram.x, defaultLayout.spectrogram.y, defaultLayout.spectrogram.w, defaultLayout.spectrogram.h
);

// Analysis Window (Waveform + Spectrum)
WM.createWindow('analysis', 'Analysis',
  '<div class="viz-inner" style="height:100%;display:flex;flex-direction:column;gap:4px;padding:4px;">' +
    '<div style="flex:1;min-height:0;">' +
      '<div style="font-size:8px;color:#555;text-transform:uppercase;letter-spacing:1px;margin-bottom:2px;">Wave</div>' +
      '<canvas id="waveformCanvas" style="height:calc(100% - 14px);"></canvas>' +
    '</div>' +
    '<div style="flex:1.5;min-height:0;">' +
      '<div style="font-size:8px;color:#555;text-transform:uppercase;letter-spacing:1px;margin-bottom:2px;">Spec <span style="font-size:7px;color:#444;float:right;">300-3400 Hz</span></div>' +
      '<canvas id="spectrumCanvas" style="height:calc(100% - 14px);"></canvas>' +
    '</div>' +
  '</div>',
  defaultLayout.analysis.x, defaultLayout.analysis.y, defaultLayout.analysis.w, defaultLayout.analysis.h
);

// Recordings Window (merged: WAV recordings + transcripts + extracts)
WM.createWindow('recordings', 'Recordings',
  '<div style="display:flex;flex-direction:column;height:100%;gap:0;">' +
    '<div id="extractPanel" style="display:none;background:#0a0a14;border:1px solid #333;border-radius:4px;padding:4px;margin:4px;">' +
      '<div class="extract-inner">' +
        '<span style="font-size:8px;color:#555;text-transform:uppercase;letter-spacing:1px;">EXT</span>' +
        '<audio id="extractPlayer" controls></audio>' +
        '<span id="extractInfo" style="font-size:8px;color:#555;"></span>' +
      '</div>' +
    '</div>' +
    '<div style="display:flex;border-bottom:1px solid #333;flex-shrink:0;">' +
      '<button class="rec-tab active" data-tab="wav" style="flex:1;padding:4px 0;font-size:9px;text-transform:uppercase;letter-spacing:1px;background:transparent;color:#00cc55;border:none;border-bottom:2px solid #00cc55;cursor:pointer;font-family:inherit;">WAV</button>' +
      '<button class="rec-tab" data-tab="transcripts" style="flex:1;padding:4px 0;font-size:9px;text-transform:uppercase;letter-spacing:1px;background:transparent;color:#666;border:none;border-bottom:2px solid transparent;cursor:pointer;font-family:inherit;">Transcripts</button>' +
      '<button class="rec-tab" data-tab="extracts" style="flex:1;padding:4px 0;font-size:9px;text-transform:uppercase;letter-spacing:1px;background:transparent;color:#666;border:none;border-bottom:2px solid transparent;cursor:pointer;font-family:inherit;">Extracts</button>' +
    '</div>' +
    '<div class="activity-inner" style="flex:1;overflow-y:auto;">' +
      '<div id="tabWav"><div id="activityLog"></div></div>' +
      '<div id="tabTranscripts" style="display:none;"><div id="transcriptLog"></div></div>' +
      '<div id="tabExtracts" style="display:none;"><div class="panel-title">Waveforms</div><div id="waveformsList"></div></div>' +
    '</div>' +
  '</div>',
  defaultLayout.recordings.x, defaultLayout.recordings.y, defaultLayout.recordings.w, defaultLayout.recordings.h
);
/* Tab switching for Recordings window */
(function(){
  var tabs=document.querySelectorAll('.rec-tab');
  var panes={wav:document.getElementById('tabWav'),transcripts:document.getElementById('tabTranscripts'),extracts:document.getElementById('tabExtracts')};
  for(var i=0;i<tabs.length;i++){
    tabs[i].addEventListener('click',function(){
      for(var j=0;j<tabs.length;j++){tabs[j].style.color='#666';tabs[j].style.borderBottom='2px solid transparent';tabs[j].classList.remove('active');}
      this.style.color='#00cc55';this.style.borderBottom='2px solid #00cc55';this.classList.add('active');
      var t=this.getAttribute('data-tab');
      for(var k in panes){if(panes[k])panes[k].style.display=k===t?'block':'none';}
    });
  }
})();

// Apply saved layout if exists
_dbg('loading saved layout...');
var savedLayout = WM.loadLayout();
_dbg('savedLayout: '+(savedLayout ? Object.keys(savedLayout).join(',') : 'null'));
if (savedLayout) {
  WM.applyLayout(savedLayout);
  _dbg('layout applied');
}
</script>
<script>_dbg('script2 canary');</script>
<script>
_dbg('windows created, entering state init');
/* ── State ── */
let ws=null,audioCtx=null,volume=0.85,muted=false,squelchOpen=false;
let currentChannel=null,currentFreq=0,signalDb=-100,noiseFloor=-20,audioFlatness=0.8;
let audioQueue=[],isPlaying=false,nextPlayTime=0,audioFadeIn=0;
const SR=48000,JITTER_MS=200,SMETER_BARS=20;
let lastAudioSamples=new Float32Array(1024);
let transcriptHistory=[];
let timeZoom=2;
let voicePaintRegions=null;
let voicePaintDesc='';
let lastVc=0,vcGateHang=0,vcFadeGain=0;
const VC_GREEN=0.5,VC_GATE=0.2,VC_HANG_MS=400;
var vcHangSamples=Math.floor(SR*VC_HANG_MS/1000);
var vcFadeLen=Math.floor(SR*0.03); /* 30ms fade-out to avoid click */
/* ── FFT engine: 2048-pt radix-2 Cooley-Tukey ── */
const FFT_N=2048,HOP=1024;
/* Always-on spectrogram: no vizGate, continuous rendering */
/* Frequency-to-color mapping for VHF marine band */
function freqToHue(freq){
  var lo=156050000,hi=157425000;
  var t=Math.max(0,Math.min(1,(freq-lo)/(hi-lo)));
  return 120+t*160; /* 120(green) -> 200(cyan) -> 280(violet) */
}
function freqToColor(freq,alpha){
  return 'hsla('+freqToHue(freq)+',100%,50%,'+(alpha||1)+')';
}
/* STFT frame storage for spectral extraction (iSTFT reconstruction) */
const STFT_MAX_FRAMES=500; /* ~10.7s at HOP=1024, SR=48000 */
let stftStore=[];
const VOCAL_LO=300,VOCAL_HI=3400;
const BIN_HZ=SR/FFT_N;
const LO_BIN=Math.floor(VOCAL_LO/BIN_HZ);
const HI_BIN=Math.ceil(VOCAL_HI/BIN_HZ);
const N_BINS=HI_BIN-LO_BIN;

/* Hann window */
const hann=new Float32Array(FFT_N);
for(let i=0;i<FFT_N;i++) hann[i]=0.5*(1-Math.cos(2*Math.PI*i/(FFT_N-1)));

/* Ring buffer */
const ring=new Float32Array(FFT_N);
let rPos=0,hopCnt=FFT_N;
const fR=new Float32Array(FFT_N),fI=new Float32Array(FFT_N);
let curSpec=new Float32Array(N_BINS);
let sgPending=[];
/* Per-bin noise floor tracking (EMA) for signal detection in noise */
var nfBins=new Float32Array(N_BINS);
for(var _i=0;_i<N_BINS;_i++) nfBins[_i]=0.01; /* initial low floor */
var NF_ALPHA=0.02; /* EMA smoothing for noise floor bins */

/* Inferno-style color LUT (256 entries) — boosted saturation */
const cLUT=new Uint8Array(768);
(function(){
  for(let i=0;i<256;i++){
    const t=i/255;let r,g,b;
    if(t<0.10){const s=t/0.10;r=s*20;g=0;b=s*60;}
    else if(t<0.25){const s=(t-0.10)/0.15;r=20+s*85;g=0;b=60+s*80;}
    else if(t<0.42){const s=(t-0.25)/0.17;r=105+s*130;g=s*30;b=140-s*50;}
    else if(t<0.58){const s=(t-0.42)/0.16;r=235+s*20;g=30+s*110;b=90-s*80;}
    else if(t<0.75){const s=(t-0.58)/0.17;r=255;g=140+s*70;b=10+s*20;}
    else if(t<0.88){const s=(t-0.75)/0.13;r=255;g=210+s*30;b=30+s*50;}
    else{const s=(t-0.88)/0.12;r=255;g=240+s*15;b=80+s*175;}
    cLUT[i*3]=Math.min(255,Math.floor(r));
    cLUT[i*3+1]=Math.min(255,Math.floor(g));
    cLUT[i*3+2]=Math.min(255,Math.floor(b));
  }
})();

function fft(re,im){
  const N=re.length;
  for(let i=1,j=0;i<N;i++){
    let b=N>>1;while(j&b){j^=b;b>>=1;}j^=b;
    if(i<j){let t=re[i];re[i]=re[j];re[j]=t;t=im[i];im[i]=im[j];im[j]=t;}
  }
  for(let len=2;len<=N;len<<=1){
    const a=-2*Math.PI/len,wR=Math.cos(a),wI=Math.sin(a);
    for(let i=0;i<N;i+=len){
      let cR=1,cI=0;const h=len>>1;
      for(let j=0;j<h;j++){
        const u=i+j,v=u+h;
        const tR=re[v]*cR-im[v]*cI,tI=re[v]*cI+im[v]*cR;
        re[v]=re[u]-tR;im[v]=im[u]-tI;re[u]+=tR;im[u]+=tI;
        const nr=cR*wR-cI*wI;cI=cR*wI+cI*wR;cR=nr;
      }
    }
  }
}

function pushFFT(samples){
  for(let i=0;i<samples.length;i++){
    ring[rPos]=samples[i];rPos=(rPos+1)%FFT_N;hopCnt--;
    if(hopCnt<=0){doFFT();hopCnt=HOP;}
  }
}

function doFFT(){
  for(let i=0;i<FFT_N;i++){fR[i]=ring[(rPos+i)%FFT_N]*hann[i];fI[i]=0;}
  fft(fR,fI);
  /* Store complex FFT frame for iSTFT extraction, tagged with squelch state */
  stftStore.push({re:new Float32Array(fR),im:new Float32Array(fI),voice:squelchOpen,vc:0});
  if(stftStore.length>STFT_MAX_FRAMES) stftStore.shift();
  const m=new Float32Array(N_BINS);
  const mags=new Float32Array(N_BINS);
  var totalE=0,peakCount=0,geoSum=0,ariSum=0;
  for(let k=0;k<N_BINS;k++){
    const idx=k+LO_BIN;
    const mag=Math.sqrt(fR[idx]*fR[idx]+fI[idx]*fI[idx])/FFT_N;
    mags[k]=mag;
    const db=20*Math.log10(Math.max(mag,1e-10));
    m[k]=Math.max(0,Math.min(1,(db+60)/55));
    totalE+=mag*mag;
    ariSum+=mag;
    geoSum+=Math.log(Math.max(mag,1e-20));
  }
  /* Voice confidence: harmonic peak counting + spectral flatness */
  var meanMag=ariSum/N_BINS;
  for(let k=2;k<N_BINS-2;k++){
    if(mags[k]>mags[k-1]&&mags[k]>mags[k+1]&&mags[k]>meanMag*2.5) peakCount++;
  }
  /* Spectral flatness: geometric mean / arithmetic mean (1=flat noise, 0=tonal) */
  var geoMean=Math.exp(geoSum/N_BINS);
  var flatness=(ariSum>1e-10)?geoMean/(ariSum/N_BINS):1;
  /* Voice confidence 0-1: high peaks + low flatness = voice */
  var vc=0;
  if(squelchOpen){
    var peakScore=Math.min(1,peakCount/8); /* 8+ peaks = full score */
    var tonalScore=Math.max(0,1-flatness*2); /* flatness<0.5 = tonal */
    var energyScore=Math.min(1,totalE*5000); /* enough energy present */
    vc=peakScore*0.4+tonalScore*0.35+energyScore*0.25;
  }
  m.vc=vc;m.peaks=peakCount;
  lastVc=vc;
  if(vc>=VC_GATE) vcGateHang=vcHangSamples;
  /* Tag the last stored STFT frame with its vc */
  if(stftStore.length>0) stftStore[stftStore.length-1].vc=vc;
  /* Update per-bin noise floor EMA during idle (squelch closed) */
  if(!squelchOpen){
    for(var nk=0;nk<N_BINS;nk++){
      nfBins[nk]=nfBins[nk]*(1-NF_ALPHA)+m[nk]*NF_ALPHA;
    }
  }
  curSpec=m;
  /* Always-on: push every frame to spectrogram render queue */
  sgPending.push(m);
}

/* ── S-Meter ── */
const smeterEl=document.getElementById('smeter'),smeterScaleEl=document.getElementById('smeterScale');
const smL=['1','','3','','5','','7','','9','','+10','','+20','','+30','','+40','','+50',''];
for(let i=0;i<SMETER_BARS;i++){
  const bar=document.createElement('div');bar.className='smeter-bar';bar.style.height=(8+i*0.6)+'px';smeterEl.appendChild(bar);
  const lbl=document.createElement('span');lbl.textContent=smL[i]||'';smeterScaleEl.appendChild(lbl);
}

/* ── Canvases ── */
_dbg('acquiring canvases...');
const waveC=document.getElementById('waveformCanvas');
const specC=document.getElementById('spectrumCanvas');
const sgC=document.getElementById('sgCanvas');
_dbg('waveC:'+!!waveC+' specC:'+!!specC+' sgC:'+!!sgC);
if(!waveC) _dbg('!! MISSING: waveformCanvas');
if(!specC) _dbg('!! MISSING: spectrumCanvas');
if(!sgC) _dbg('!! MISSING: sgCanvas');
const waveX=waveC.getContext('2d'),specX=specC.getContext('2d'),sgX=sgC.getContext('2d');
_dbg('canvas contexts: '+!!waveX+' '+!!specX+' '+!!sgX);
let dpr=1;
function resizeAll(){
  dpr=devicePixelRatio||1;
  [waveC,specC,sgC].forEach(function(c){c.width=c.clientWidth*dpr;c.height=c.clientHeight*dpr;});
  sgX.fillStyle='#0a0a14';sgX.fillRect(0,0,sgC.width,sgC.height);
}
resizeAll();initSgLabels();
window.addEventListener('resize',function(){resizeAll();initSgLabels();});
/* ResizeObserver to handle canvas resize when windows are resized */
if(typeof ResizeObserver!=='undefined'){
  var _resizeTimer=null;
  var _resizeCount=0;
  var canvasObserver=new ResizeObserver(function(){
    _resizeCount++;
    if(_resizeCount<=5) _dbg('ResizeObserver fired #'+_resizeCount);
    if(_resizeTimer) clearTimeout(_resizeTimer);
    _resizeTimer=setTimeout(function(){resizeAll();initSgLabels();},100);
  });
  canvasObserver.observe(waveC.parentElement);
  canvasObserver.observe(specC.parentElement);
  canvasObserver.observe(sgC.parentElement);
}

/* ── WebSocket ── */
var wsWasConnected=false;
function connectWS(){
  const host=location.hostname||'localhost';
  const wsUrl='ws://'+host+':9081';
  _dbg('connectWS -> '+wsUrl);
  ws=new WebSocket(wsUrl);ws.binaryType='arraybuffer';
  ws.onopen=function(){
    _dbg('WS OPEN');
    document.getElementById('wsDot').className='dot connected';
    if(wsWasConnected){_dbg('WS reconnected, reloading');location.reload();}
    wsWasConnected=true;
  };
  ws.onclose=function(ev){_dbg('WS CLOSE code='+ev.code+' reason='+ev.reason);document.getElementById('wsDot').className='dot disconnected';setTimeout(connectWS,2000);};
  ws.onerror=function(ev){_dbg('WS ERROR');document.getElementById('wsDot').className='dot disconnected';};
  ws.onmessage=function(e){
    try{
      if(e.data instanceof ArrayBuffer) handleAudio(e.data);
      else handleText(JSON.parse(e.data));
    }catch(err){console.error('WS message error:',err);}
  };
}

var _dbgAudioCount=0;
function handleAudio(buf){
  _dbgAudioCount++;if(_dbgAudioCount<=3)console.log('[DBG] handleAudio #'+_dbgAudioCount+' bytes='+buf.byteLength);
  const v=new DataView(buf);if(buf.byteLength<12)return;
  const ch=v.getUint32(0,true),freq=v.getUint32(4,true),sig=v.getFloat32(8,true);
  currentChannel=ch||null;currentFreq=freq;signalDb=sig;
  const cnt=(buf.byteLength-12)/4,samples=new Float32Array(cnt);
  for(let i=0;i<cnt;i++) samples[i]=v.getFloat32(12+i*4,true);
  /* Client-side fade-in: ramp first 4800 samples (~100ms) after squelch opens */
  if(audioFadeIn<4800){
    for(let i=0;i<samples.length;i++){
      if(audioFadeIn<4800){
        samples[i]*=(audioFadeIn/4800);
        audioFadeIn++;
      }
    }
  }
  /* Pre-gain: boost incoming signal 3x before voice gate + playback */
  for(let i=0;i<samples.length;i++){samples[i]*=3.0;if(samples[i]>1)samples[i]=1;if(samples[i]<-1)samples[i]=-1;}
  if(samples.length>0){
    lastAudioSamples=samples.length>1024?samples.slice(0,1024):samples;
    pushFFT(samples);
  }
  /* Voice gate: play when vc>=0.2 (green/orange) + 400ms hang, fade to silence */
  for(let i=0;i<samples.length;i++){
    if(lastVc>=VC_GATE||vcGateHang>0){
      if(vcGateHang>0) vcGateHang--;
      vcFadeGain=1;
    } else {
      /* Smooth fade-out over 30ms then silence */
      if(vcFadeGain>0){
        vcFadeGain-=1/vcFadeLen;
        if(vcFadeGain<0) vcFadeGain=0;
        samples[i]*=vcFadeGain;
      } else {
        samples[i]=0;
      }
    }
  }
  audioQueue.push(samples);
  if(!isPlaying) startPlayback();
  updateDisplay();updateSMeter();
}

var _dbgTextCount=0;
function handleText(d){
  _dbgTextCount++;if(_dbgTextCount<=10)console.log('[DBG] handleText #'+_dbgTextCount+' type='+d.type,d);
  if(d.type==='squelch'){
    squelchOpen=d.open;
    document.getElementById('squelchDot').className=d.open?'dot squelch-open':'dot squelch-closed';
    document.getElementById('rxDot').className=d.open?'dot rx-active':'dot';
    if(d.open){
      currentChannel=d.channel;currentFreq=d.freq;signalDb=d.signal_db;
      audioFadeIn=0; /* Reset fade-in ramp for new transmission */
      audioQueue.length=0; /* Clear stale audio from transition */
      var bx=document.getElementById('txbox');
      bx.removeAttribute('data-has-tx');
      var chTag=d.channel?'CH'+d.channel:'--';
      var fTag=d.freq?(d.freq/1e6).toFixed(3):'---';
      var ts=new Date().toLocaleTimeString();
      bx.innerHTML='<span class="tx-ch">'+chTag+'</span> '+fTag+' MHz '+d.signal_db.toFixed(1)+'dB <span style="color:#1a4a1a;">'+ts+'</span>';
      bx.className='lcd-ticker';
    } else {
      audioQueue.length=0;
      if(document.getElementById('autoExtract').checked){
        setTimeout(function(){extractAudio();},500);
      }
      setTimeout(function(){
        if(!squelchOpen){
          currentChannel=null;currentFreq=0;signalDb=-100;updateDisplay();updateSMeter();
          /* Don't idle the ticker if a transcript is displayed — keep it until next message */
          var bx=document.getElementById('txbox');
          if(!bx.getAttribute('data-has-tx')) bx.className='lcd-ticker idle';
        }
      },3000);
    }
    updateDisplay();addActivity(d);
  } else if(d.type==='signal_level'){
    signalDb=d.signal_db;noiseFloor=d.noise_floor;
    if(typeof d.audio_flatness==='number') audioFlatness=d.audio_flatness;
    if(d.squelch_open) squelchOpen=true;
    updateSMeter();
  } else if(d.type==='transcription'){
    var bx=document.getElementById('txbox');
    bx.className='lcd-ticker';
    bx.setAttribute('data-has-tx','1');
    var now=new Date();
    var ds=(now.getMonth()+1)+'/'+now.getDate()+' '+now.getHours().toString().padStart(2,'0')+':'+now.getMinutes().toString().padStart(2,'0')+':'+now.getSeconds().toString().padStart(2,'0');
    var ch=d.channel?'CH'+d.channel:'--';
    /* Format: ====== date ====== / transcript / ======END date====== */
    function txRender(txt){
      bx.innerHTML='<div class="tx-hdr">====== '+ds+' ======</div><div class="tx-body"><span class="tx-ch">'+ch+'</span> '+txt+' <span style="color:#1a4a1a;">======END '+ds+'======</span></div>';
    }
    txRender(d.text);
    /* Add to history with raw text, then request agent cleanup */
    var entry={t:now,ch:d.channel,text:d.text,clean:null};
    transcriptHistory.unshift(entry);
    if(transcriptHistory.length>20) transcriptHistory.pop();
    renderTranscriptLog();
    /* Send to agent for shorthand cleanup */
    (function(e,renderFn){
      fetch('/api/clean-transcript',{method:'POST',body:d.text}).then(function(r){return r.json();}).then(function(j){
        if(j.cleaned){
          e.clean=j.cleaned;
          renderFn(j.cleaned);
          renderTranscriptLog();
        }
      }).catch(function(){});
    })(entry,txRender);
  } else if(d.type==='voice_paint'&&d.painting){
    voicePaintRegions=d.painting.regions||[];
    voicePaintDesc=d.painting.description||'';
  }
}

function updateDisplay(){
  const chE=document.getElementById('channelNum'),lE=document.getElementById('channelLabel'),fE=document.getElementById('freqDisplay');
  if(currentChannel&&currentChannel>0){
    chE.textContent='VHF CH '+String(currentChannel).padStart(2,'0');
    lE.textContent=getChanLabel(currentChannel);
  } else {
    chE.textContent='VHF CH --';
    lE.textContent=squelchOpen?'RX':'SCAN';
    fE.textContent='---.--- MHz';
  }
  if(currentFreq>0) fE.textContent=(currentFreq/1e6).toFixed(3)+' MHz';
}

function updateSMeter(){
  const bars=smeterEl.children;
  /* Only show signal strength when squelch is open; idle = show relative to noise floor */
  var db=signalDb;
  var a=0;
  if(squelchOpen){
    /* Map signal: noise_floor -> full scale to 0 -> SMETER_BARS */
    var range=Math.max(1,(-noiseFloor));
    var above=db-noiseFloor;
    var n=Math.max(0,Math.min(1,above/range));
    a=Math.floor(n*SMETER_BARS);
  } else {
    /* Idle: show 0-2 bars based on how close signal is to noise floor */
    if(db>noiseFloor-3) a=2;
    else if(db>noiseFloor-10) a=1;
    else a=0;
  }
  for(let i=0;i<SMETER_BARS;i++){
    if(i<a){let c='smeter-bar active';if(i>=14)c+=' over';else if(i>=10)c+=' high';bars[i].className=c;}
    else bars[i].className='smeter-bar';
  }
}

var lastActivityState={};
var lastOpenTime={};
function addActivity(d){
  var key=d.channel||0;
  if(d.open){
    lastActivityState[key]=true;
    lastOpenTime[key]=Date.now();
    return; /* Don't show open events — wait for close */
  }
  /* Dedup: skip consecutive CLOSED */
  if(lastActivityState[key]===false) return;
  lastActivityState[key]=false;
  var log=document.getElementById('activityLog');
  var e=document.createElement('div');
  e.className='activity-entry';
  e.style.cssText='display:flex;align-items:center;justify-content:space-between;';
  var t=new Date().toLocaleTimeString();
  var ch=d.channel?'CH'+d.channel:'--';
  var dur='';
  if(lastOpenTime[key]){var sec=Math.round((Date.now()-lastOpenTime[key])/1000);dur=sec+'s';}
  e.innerHTML='<span><span class="time">'+t+'</span> <span class="ch">'+ch+'</span> '+dur+'</span><span class="play-badge" title="Play">&#9654;</span>';
  log.insertBefore(e,log.firstChild);
  while(log.children.length>60) log.removeChild(log.lastChild);
  /* Wire play badge to latest recording after server finishes writing */
  (function(el,channel){
    setTimeout(function(){
      fetch('/api/recordings').then(function(r){return r.json();}).then(function(recs){
        if(!recs.length)return;
        /* Find best match: latest recording for this channel */
        var best=null;
        for(var i=0;i<recs.length;i++){
          var cm=recs[i].filename.match(/Ch(\d+)/);
          if(cm&&parseInt(cm[1])===channel){best=recs[i];break;}
          if(!best) best=recs[i];
        }
        if(best){
          var badge=el.querySelector('.play-badge');
          if(badge){
            badge.setAttribute('data-url',best.url);
            badge.className='play-badge ready';
            var kb=Math.round(best.size/1024);
            badge.innerHTML='&#9654;<span class="sz">'+kb+'K</span>';
          }
        }
      }).catch(function(){});
    },2500);
  })(e,d.channel||0);
}
/* Event delegation: click play badges in activity log */
document.getElementById('activityLog').addEventListener('click',function(ev){
  var badge=ev.target.closest('.play-badge');
  if(!badge)return;
  var url=badge.getAttribute('data-url');
  if(!url)return;
  initAudio();
  var player=document.getElementById('extractPlayer');
  if(player.src) URL.revokeObjectURL(player.src);
  player.src=url;
  document.getElementById('extractPanel').style.display='block';
  player.play();
});

/* ── Audio Playback with vocal bandpass filter ── */
var gainNode=null,bpLo=null,bpHi=null;
function initAudio(){
  _dbg('initAudio called, existing:'+!!audioCtx);
  if(audioCtx)return;
  audioCtx=new(window.AudioContext||window.webkitAudioContext)({sampleRate:SR});
  _dbg('AudioContext state='+audioCtx.state+' sr='+audioCtx.sampleRate);
  /* Vocal bandpass: highpass at 300Hz + lowpass at 3400Hz */
  bpLo=audioCtx.createBiquadFilter();
  bpLo.type='highpass';bpLo.frequency.value=300;bpLo.Q.value=0.7;
  bpHi=audioCtx.createBiquadFilter();
  bpHi.type='lowpass';bpHi.frequency.value=3400;bpHi.Q.value=0.7;
  gainNode=audioCtx.createGain();
  gainNode.gain.value=4.0;
  bpLo.connect(bpHi);bpHi.connect(gainNode);gainNode.connect(audioCtx.destination);
}
function startPlayback(){if(!audioCtx)initAudio();isPlaying=true;nextPlayTime=audioCtx.currentTime+JITTER_MS/1000;schedBuf();}
function schedBuf(){
  if(!isPlaying||!audioCtx)return;
  while(audioQueue.length>0){
    const s=audioQueue.shift();if(!s||!s.length)continue;
    const b=audioCtx.createBuffer(1,s.length,SR),d=b.getChannelData(0);
    const vol=muted?0:volume;for(let i=0;i<s.length;i++) d[i]=s[i]*vol;
    const src=audioCtx.createBufferSource();src.buffer=b;
    src.connect(bpLo||audioCtx.destination);
    if(nextPlayTime<audioCtx.currentTime) nextPlayTime=audioCtx.currentTime+0.01;
    src.start(nextPlayTime);nextPlayTime+=b.duration;
  }
  if(!audioQueue.length) isPlaying=false;
  requestAnimationFrame(schedBuf);
}

/* ══════════════════════════════════════════
   Visualization — high-res vocal-range STFT
   ══════════════════════════════════════════ */

function drawWaveform(){
  const w=waveC.width,h=waveC.height;
  waveX.fillStyle='#0a0a14';waveX.fillRect(0,0,w,h);
  waveX.strokeStyle='#1a1a2a';waveX.lineWidth=dpr;
  for(let y=0;y<=h;y+=h/4){waveX.beginPath();waveX.moveTo(0,y);waveX.lineTo(w,y);waveX.stroke();}
  const samples=lastAudioSamples;if(samples.length<2)return;
  var waveColor=currentFreq>0?freqToColor(currentFreq):'#00ff66';
  waveX.strokeStyle=waveColor;waveX.lineWidth=1.5*dpr;waveX.beginPath();
  const step=samples.length/w;
  for(let i=0;i<w;i++){const s=samples[Math.floor(i*step)]||0;const y=(1-s)*h/2;if(i===0)waveX.moveTo(i,y);else waveX.lineTo(i,y);}
  waveX.stroke();
  waveX.strokeStyle=currentFreq>0?freqToColor(currentFreq,0.12):'rgba(0,255,102,0.12)';waveX.lineWidth=4*dpr;waveX.stroke();
}

function drawSpectrum(){
  const w=specC.width,h=specC.height;
  specX.fillStyle='#0a0a14';specX.fillRect(0,0,w,h);

  /* Frequency grid (vocal range) */
  specX.strokeStyle='#1a1a2a';specX.lineWidth=dpr;
  specX.fillStyle='rgba(120,120,120,0.45)';specX.font=(9*dpr)+'px Courier New';
  const gf=[500,1000,1500,2000,2500,3000];
  for(let fi=0;fi<gf.length;fi++){
    const f=gf[fi];
    const x=(f-VOCAL_LO)/(VOCAL_HI-VOCAL_LO)*w;
    specX.beginPath();specX.moveTo(x,0);specX.lineTo(x,h);specX.stroke();
    specX.fillText(f>=1000?(f/1000)+'k':f+'',x+2*dpr,h-3*dpr);
  }
  /* dB grid */
  specX.fillStyle='rgba(80,80,80,0.3)';
  for(let db=-50;db<=-10;db+=10){
    const y=h*(1-((db+60)/55)*0.9);
    specX.beginPath();specX.moveTo(0,y);specX.lineTo(w,y);specX.stroke();
    specX.fillText(db+'dB',3*dpr,y-2*dpr);
  }

  if(curSpec.length<2)return;
  /* Filled area under curve */
  specX.beginPath();specX.moveTo(0,h);
  for(let i=0;i<curSpec.length;i++){
    const x=i/(curSpec.length-1)*w,y=h*(1-curSpec[i]*0.9);
    specX.lineTo(x,y);
  }
  specX.lineTo(w,h);specX.closePath();
  const gr=specX.createLinearGradient(0,0,0,h);
  if(squelchOpen){
    var specFillColor=currentFreq>0?freqToColor(currentFreq,0.45):'rgba(0,255,102,0.45)';
    var specMidColor=currentFreq>0?freqToColor(currentFreq,0.15):'rgba(0,150,60,0.15)';
    gr.addColorStop(0,specFillColor);gr.addColorStop(0.5,specMidColor);gr.addColorStop(1,'rgba(0,60,30,0.02)');
  } else {
    gr.addColorStop(0,'rgba(25,50,35,0.25)');gr.addColorStop(1,'rgba(10,20,15,0.02)');
  }
  specX.fillStyle=gr;specX.fill();

  /* Top line */
  var specLineColor=squelchOpen?(currentFreq>0?freqToColor(currentFreq):'#00ff66'):'#1a3a1a';
  specX.strokeStyle=specLineColor;specX.lineWidth=(squelchOpen?1.5:1)*dpr;
  specX.beginPath();
  for(let i=0;i<curSpec.length;i++){
    const x=i/(curSpec.length-1)*w,y=h*(1-curSpec[i]*0.9);
    if(i===0)specX.moveTo(x,y);else specX.lineTo(x,y);
  }
  specX.stroke();
  if(squelchOpen){specX.strokeStyle=currentFreq>0?freqToColor(currentFreq,0.08):'rgba(0,255,102,0.08)';specX.lineWidth=4*dpr;specX.stroke();}
  /* Noise floor reference line (dashed) */
  var nfDb=noiseFloor;
  if(nfDb>-60&&nfDb<0){
    var nfY=h*(1-((nfDb+60)/55)*0.9);
    specX.setLineDash([4*dpr,4*dpr]);specX.strokeStyle='rgba(100,100,255,0.3)';specX.lineWidth=dpr;
    specX.beginPath();specX.moveTo(0,nfY);specX.lineTo(w,nfY);specX.stroke();
    specX.fillStyle='rgba(100,100,255,0.4)';specX.font=(8*dpr)+'px Courier New';
    specX.fillText('NF '+nfDb.toFixed(0)+'dB',w-55*dpr,nfY-3*dpr);
    specX.setLineDash([]);
  }
  /* Squelch threshold line */
  var sqlVal=parseFloat(document.getElementById('squelchSlider').value)||12;
  var sqlDb=-sqlVal;
  if(sqlDb>-60&&sqlDb<0){
    var sqlY=h*(1-((sqlDb+60)/55)*0.9);
    specX.setLineDash([2*dpr,6*dpr]);specX.strokeStyle='rgba(255,170,0,0.25)';specX.lineWidth=dpr;
    specX.beginPath();specX.moveTo(0,sqlY);specX.lineTo(w,sqlY);specX.stroke();
    specX.fillStyle='rgba(255,170,0,0.35)';specX.font=(8*dpr)+'px Courier New';
    specX.fillText('SQL',w-30*dpr,sqlY-3*dpr);
    specX.setLineDash([]);
  }
  /* Entropy (flatness) indicator — top-left of spectrum */
  var flatLabel=audioFlatness<0.45?'VOICE':audioFlatness<0.55?'SIGNAL':audioFlatness<0.70?'WEAK':'NOISE';
  var flatColor=audioFlatness<0.45?'rgba(0,255,102,0.7)':audioFlatness<0.55?'rgba(0,200,255,0.6)':audioFlatness<0.70?'rgba(255,170,0,0.5)':'rgba(255,60,60,0.35)';
  specX.fillStyle=flatColor;specX.font='bold '+(9*dpr)+'px Courier New';
  specX.fillText('H='+audioFlatness.toFixed(2)+' '+flatLabel,4*dpr,12*dpr);
}

var sgColCount=0;
var sgTickInterval=Math.round(SR/HOP); /* 1 column per HOP samples, so SR/HOP cols = 1 second */
function drawSpectrogram(){
  const w=sgC.width,h=sgC.height,z=timeZoom;
  if(sgPending.length===0)return;
  const stripH=4*dpr;
  const specH=h-stripH;
  while(sgPending.length>0){
    const m=sgPending.shift();
    const vc=m.vc||0;
    sgColCount++;
    sgX.drawImage(sgC,z,0,w-z,h,0,0,w-z,h);
    sgX.clearRect(w-z,0,z,h);
    /* Voice confidence masking + color coding (freq-tinted when voice detected) */
    var dim,tR=0,tG=0,tB=0;
    if(vc>0.5){
      dim=1.0;
      if(currentFreq>0){
        /* Frequency-based hue tint for voice */
        var hue=freqToHue(currentFreq);
        var hr=(hue%360)/60;var hi=Math.floor(hr);var hf=hr-hi;
        var cR=0,cG=0,cB=0;
        switch(hi){
          case 0:cR=1;cG=hf;cB=0;break;case 1:cR=1-hf;cG=1;cB=0;break;
          case 2:cR=0;cG=1;cB=hf;break;case 3:cR=0;cG=1-hf;cB=1;break;
          case 4:cR=hf;cG=0;cB=1;break;default:cR=1;cG=0;cB=1-hf;break;
        }
        tR=Math.floor(cR*60*vc);tG=Math.floor(cG*80*vc);tB=Math.floor(cB*60*vc);
      } else {
        tG=Math.floor(60+40*vc); /* fallback green */
      }
    } else if(vc>0.2){
      dim=0.5+vc*1.0;
      tR=Math.floor(50*vc);tG=Math.floor(30*vc); /* amber */
    } else if(squelchOpen){
      /* STATIC DURING RX: bright red tint so it's obvious */
      dim=0.25;
      tR=Math.floor(60+80*(0.2-vc)); /* red increases as vc drops */
    } else {
      /* Idle/no signal: very dim */
      dim=0.06;
    }
    for(let y=0;y<specH;y++){
      const fi=Math.floor((1-y/specH)*N_BINS);
      const binIdx=Math.min(Math.max(0,fi),N_BINS-1);
      const v=m[binIdx];
      /* Boost color intensity: raise v by 1.3x for more vivid spectrogram */
      /* Signal detection: extra boost when bin exceeds 2x noise floor (6dB above) */
      var vb=Math.min(1,v*1.3);
      if(!squelchOpen&&v>nfBins[binIdx]*2&&v>0.03){vb=Math.min(1,vb*1.6);}
      const ci=Math.floor(Math.max(0,Math.min(255,vb*255)))*3;
      var r=Math.floor(cLUT[ci]*dim)+tR;
      var g=Math.floor(cLUT[ci+1]*dim)+tG;
      var b=Math.floor(cLUT[ci+2]*dim)+tB;
      if(r>255)r=255;if(g>255)g=255;if(b>255)b=255;
      sgX.fillStyle='rgb('+r+','+g+','+b+')';
      sgX.fillRect(w-z,y,z,1);
    }
    /* Confidence strip at bottom — freq-tinted for voice, amber/red/dim otherwise */
    var sr,sg,sb;
    if(vc>0.5&&currentFreq>0){
      var sHue=freqToHue(currentFreq);var shr=(sHue%360)/60;var shi=Math.floor(shr);var shf=shr-shi;
      var scR=0,scG=0,scB=0;
      switch(shi){case 0:scR=1;scG=shf;break;case 1:scR=1-shf;scG=1;break;case 2:scG=1;scB=shf;break;case 3:scG=1-shf;scB=1;break;case 4:scR=shf;scB=1;break;default:scR=1;scB=1-shf;break;}
      sr=Math.floor(scR*255*vc);sg=Math.floor(scG*255*vc);sb=Math.floor(scB*255*vc);
    } else if(vc>0.5){sr=0;sg=Math.floor(120+135*vc);sb=Math.floor(20*vc);}
    else if(vc>0.2){sr=Math.floor(220*vc);sg=Math.floor(160*vc);sb=0;}        /* amber */
    else if(squelchOpen){sr=Math.floor(140+100*(0.2-vc));sg=10;sb=10;}         /* RED for static */
    else{sr=8;sg=8;sb=8;}                                                       /* dark idle */
    sgX.fillStyle='rgb('+sr+','+sg+','+sb+')';
    sgX.fillRect(w-z,specH,z,stripH);
    /* Time ticks: mark every 1s on the confidence strip */
    if(sgColCount%sgTickInterval===0){
      sgX.fillStyle='rgba(255,255,255,0.5)';
      sgX.fillRect(w-z,specH,z,stripH);
      /* Small notch at top */
      sgX.fillStyle='rgba(255,255,255,0.15)';
      sgX.fillRect(w-z,0,1,specH);
    }
    /* Minor tick every 0.5s */
    else if(sgColCount%(sgTickInterval/2|0)===0){
      sgX.fillStyle='rgba(255,255,255,0.25)';
      sgX.fillRect(w-z,specH,z,stripH);
    }
  }
}

function drawSgLabels(){
  const w=sgC.width,h=sgC.height;
  sgX.strokeStyle='rgba(80,80,80,0.12)';sgX.lineWidth=dpr;sgX.setLineDash([2*dpr,6*dpr]);
  const ff=[500,1000,1500,2000,2500,3000];
  for(let fi=0;fi<ff.length;fi++){
    const f=ff[fi];
    const y=h*(1-(f-VOCAL_LO)/(VOCAL_HI-VOCAL_LO));
    sgX.beginPath();sgX.moveTo(0,y);sgX.lineTo(w,y);sgX.stroke();
  }
  sgX.setLineDash([]);
}
/* Fixed frequency labels outside the scrolling canvas */
function initSgLabels(){
  const el=document.getElementById('sgLabels');if(!el)return;
  el.innerHTML='';
  const ff=[500,1000,1500,2000,2500,3000];
  const ch=sgC.clientHeight;
  for(let fi=0;fi<ff.length;fi++){
    const f=ff[fi];
    const pct=(1-(f-VOCAL_LO)/(VOCAL_HI-VOCAL_LO))*100;
    const lbl=document.createElement('div');
    lbl.style.cssText='position:absolute;left:2px;top:'+pct+'%;transform:translateY(-50%);font-size:9px;color:rgba(180,180,180,0.6);font-family:Courier New,monospace;pointer-events:none;';
    lbl.textContent=f>=1000?(f/1000)+'k':f+'';
    el.appendChild(lbl);
  }
}

function drawVoicePaint(){
  if(!voicePaintRegions||voicePaintRegions.length===0)return;
  const w=sgC.width,h=sgC.height;
  const VOCAL_RANGE=VOCAL_HI-VOCAL_LO;
  for(let i=0;i<voicePaintRegions.length;i++){
    const r=voicePaintRegions[i];
    const y1=h*(1-(r.freq_hi-VOCAL_LO)/VOCAL_RANGE);
    const y2=h*(1-(r.freq_lo-VOCAL_LO)/VOCAL_RANGE);
    const x1=w*(r.time_start/5.0);
    const x2=w*(r.time_end/5.0);
    const rh=y2-y1,rw=x2-x1;
    sgX.save();
    sgX.globalAlpha=r.opacity||0.3;
    if(r.style==='outline'){
      sgX.strokeStyle=r.color||'#ff6600';
      sgX.lineWidth=2*dpr;
      sgX.strokeRect(x1,y1,rw,rh);
    } else if(r.style==='glow'){
      sgX.shadowColor=r.color||'#ff6600';
      sgX.shadowBlur=12*dpr;
      sgX.fillStyle=r.color||'#ff6600';
      sgX.fillRect(x1,y1,rw,rh);
      sgX.shadowBlur=0;
    } else if(r.style==='gradient'){
      var gr=sgX.createLinearGradient(x1,y1,x1,y2);
      gr.addColorStop(0,r.color||'#ff6600');
      gr.addColorStop(1,'transparent');
      sgX.fillStyle=gr;
      sgX.fillRect(x1,y1,rw,rh);
    } else {
      sgX.fillStyle=r.color||'#ff6600';
      sgX.fillRect(x1,y1,rw,rh);
    }
    sgX.globalAlpha=0.9;
    sgX.fillStyle='#ffffff';
    sgX.font=(9*dpr)+'px Courier New';
    sgX.fillText(r.label||'',x1+3*dpr,y1+11*dpr);
    sgX.restore();
  }
}

let sgLabelTimer=0;
var _renderCount=0;
var _slowFrames=0;
function render(){
  _renderCount++;
  var _t0=performance.now();
  /* DEBUG: re-enabling draws one at a time */
  try{drawWaveform();}catch(e){if(_renderCount<=3)_dbg('CRASH waveform: '+e);}
  try{drawSpectrum();}catch(e){if(_renderCount<=3)_dbg('CRASH spectrum: '+e);}
  try{drawSpectrogram();}catch(e){if(_renderCount<=3)_dbg('CRASH spectrogram: '+e);}
  sgLabelTimer++;if(sgLabelTimer>=30){sgLabelTimer=0;try{drawSgLabels();drawVoicePaint();}catch(e){_dbg('CRASH sgLabels/voicePaint: '+e);}}
  if(_renderCount<=3) _dbg('render #'+_renderCount+' FULL OK');
  requestAnimationFrame(render);
}

/* ── Controls ── */
document.getElementById('volumeSlider').addEventListener('input',function(e){volume=e.target.value/100;document.getElementById('volumeVal').textContent=e.target.value+'%';});
document.getElementById('squelchSlider').addEventListener('input',function(e){document.getElementById('squelchVal').textContent=e.target.value+'dB';});
document.getElementById('squelchSlider').addEventListener('change',function(e){fetch('/squelch',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({squelch:parseFloat(e.target.value)})}).catch(function(){});});
document.getElementById('timeSlider').addEventListener('input',function(e){timeZoom=parseInt(e.target.value);document.getElementById('timeVal').textContent=timeZoom+'x';});
function toggleMute(){muted=!muted;var b=document.getElementById('muteBtn');b.className=muted?'btn icon-btn active':'btn icon-btn';b.innerHTML=muted?'&#x1f507;':'&#x1f50a;';}
function toggleRecording(){var b=document.getElementById('recBtn');var r=b.classList.contains('rec-active');fetch('/recording',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({recording:!r})}).then(function(r){return r.json();}).then(function(d){if(d.recording){b.className='btn rec-active';b.textContent='REC ON';}else{b.className='btn';b.textContent='REC';}}).catch(function(){});}
/* Inverse FFT via conjugate trick: iFFT(X) = conj(FFT(conj(X))) / N */
function ifft(re,im){
  var N=re.length;
  for(var i=0;i<N;i++) im[i]=-im[i];
  fft(re,im);
  for(var i=0;i<N;i++){re[i]/=N;im[i]=-im[i]/N;}
}
function extractAudio(){
  if(stftStore.length<4){return;}
  var btn=document.getElementById('extractBtn');btn.className='btn active';btn.textContent='...';
  /* Find green zone: first to last frame with vc>=0.5 */
  /* Pre-pad: ~3 seconds before first green (captures lead-in) */
  /* Post-pad: ~1 second after last green (captures tail) */
  var FPS=Math.round(SR/HOP); /* frames per second (~47) */
  var PRE_PAD=FPS*3;  /* 3 seconds before */
  var POST_PAD=FPS*1; /* 1 second after */
  var firstGreen=-1,lastGreen=-1;
  for(var f=0;f<stftStore.length;f++){
    if(stftStore[f].vc>=VC_GREEN){
      if(firstGreen<0) firstGreen=f;
      lastGreen=f;
    }
  }
  if(firstGreen<0){btn.className='btn';btn.textContent='EXT';return;}
  var sliceStart=Math.max(0,firstGreen-PRE_PAD);
  var sliceEnd=Math.min(stftStore.length-1,lastGreen+POST_PAD);
  var voiceFrames=[];
  for(var f=sliceStart;f<=sliceEnd;f++) voiceFrames.push(stftStore[f]);
  /* Noise reference from outside the green zone */
  var noiseFrames=[];
  for(var f=0;f<sliceStart;f++) noiseFrames.push(stftStore[f]);
  for(var f=sliceEnd+1;f<stftStore.length;f++) noiseFrames.push(stftStore[f]);
  if(voiceFrames.length<2){btn.className='btn';btn.textContent='EXT';return;}
  /* Estimate per-bin noise floor from noise frames (spectral subtraction) */
  var noiseFloorBins=new Float32Array(FFT_N);
  if(noiseFrames.length>0){
    for(var k=LO_BIN;k<HI_BIN;k++){
      var sum=0;
      for(var n=0;n<noiseFrames.length;n++){
        var nr=noiseFrames[n].re[k],ni=noiseFrames[n].im[k];
        sum+=Math.sqrt(nr*nr+ni*ni);
      }
      noiseFloorBins[k]=(sum/noiseFrames.length)*1.5; /* 1.5x overestimate for clean removal */
    }
  }
  var nf=voiceFrames.length;
  var outLen=(nf-1)*HOP+FFT_N;
  var output=new Float32Array(outLen);
  var winSum=new Float32Array(outLen);
  /* Pack vocal-range spectral data for compression (16-bit quantized) */
  var spectralPacked=new Int16Array(nf*N_BINS*2);
  var spIdx=0;
  /* Overlap-add iSTFT reconstruction — vocal only, noise subtracted */
  for(var f=0;f<nf;f++){
    var frame=voiceFrames[f];
    var re=new Float32Array(FFT_N);
    var im=new Float32Array(FFT_N);
    /* Spectral subtraction: reduce magnitude by noise floor, preserve phase */
    for(var k=LO_BIN;k<HI_BIN;k++){
      var mag=Math.sqrt(frame.re[k]*frame.re[k]+frame.im[k]*frame.im[k]);
      var cleaned=Math.max(0,mag-noiseFloorBins[k]);
      if(mag>1e-10){
        var scale=cleaned/mag;
        re[k]=frame.re[k]*scale;
        im[k]=frame.im[k]*scale;
      }
    }
    /* Mirror conjugate for real-valued output */
    for(var k=LO_BIN;k<HI_BIN;k++){
      re[FFT_N-k]=re[k];
      im[FFT_N-k]=-im[k];
    }
    /* Pack vocal bins as 16-bit for compressed storage */
    for(var k=0;k<N_BINS;k++){
      var idx=k+LO_BIN;
      spectralPacked[spIdx++]=Math.max(-32767,Math.min(32767,Math.round(frame.re[idx]*32767)));
      spectralPacked[spIdx++]=Math.max(-32767,Math.min(32767,Math.round(frame.im[idx]*32767)));
    }
    ifft(re,im);
    var off=f*HOP;
    for(var i=0;i<FFT_N&&off+i<outLen;i++){
      output[off+i]+=re[i]*hann[i];
      winSum[off+i]+=hann[i]*hann[i];
    }
  }
  /* Normalize by window overlap sum */
  for(var i=0;i<outLen;i++){if(winSum[i]>1e-8)output[i]/=winSum[i];}
  /* Peak normalize to full scale */
  var peak=0;
  for(var i=0;i<outLen;i++){var a=Math.abs(output[i]);if(a>peak)peak=a;}
  if(peak>1e-6)for(var i=0;i<outLen;i++) output[i]/=peak;
  /* RMS boost: bring average energy up to -6dB target */
  var rms=0;
  for(var i=0;i<outLen;i++) rms+=output[i]*output[i];
  rms=Math.sqrt(rms/outLen);
  if(rms>1e-6&&rms<0.25){
    var boost=0.25/rms;
    if(boost>12)boost=12;
    for(var i=0;i<outLen;i++){
      output[i]*=boost;
      if(output[i]>1)output[i]=1;
      if(output[i]<-1)output[i]=-1;
    }
  }
  /* Encode as WAV (48kHz 16-bit mono) */
  var pcm=new Int16Array(outLen);
  for(var i=0;i<outLen;i++) pcm[i]=Math.max(-32768,Math.min(32767,Math.floor(output[i]*32700)));
  var wavLen=44+outLen*2;
  var wav=new ArrayBuffer(wavLen);
  var v=new DataView(wav);
  function ws(o,s){for(var i=0;i<s.length;i++)v.setUint8(o+i,s.charCodeAt(i));}
  ws(0,'RIFF');v.setUint32(4,wavLen-8,true);ws(8,'WAVE');
  ws(12,'fmt ');v.setUint32(16,16,true);v.setUint16(20,1,true);v.setUint16(22,1,true);
  v.setUint32(24,SR,true);v.setUint32(28,SR*2,true);v.setUint16(32,2,true);v.setUint16(34,16,true);
  ws(36,'data');v.setUint32(40,outLen*2,true);
  new Int16Array(wav,44).set(pcm);
  var wavBlob=new Blob([wav],{type:'audio/wav'});
  var url=URL.createObjectURL(wavBlob);
  var player=document.getElementById('extractPlayer');
  if(player.src) URL.revokeObjectURL(player.src);
  player.src=url;
  document.getElementById('extractPanel').style.display='block';
  /* Save WAV to server */
  fetch('/api/extracts',{method:'POST',headers:{'Content-Type':'audio/wav'},body:wavBlob}).then(function(){loadWaveforms();}).catch(function(){});
  /* Save spectrogram snapshot PNG */
  if(typeof sgC!=='undefined'&&sgC.toBlob){
    sgC.toBlob(function(blob){
      if(blob) fetch('/api/extracts',{method:'POST',headers:{'Content-Type':'image/png'},body:blob}).then(function(){loadWaveforms();}).catch(function(){});
    },'image/png');
  }
  /* Compress and upload spectral data */
  var rawBytes=new Uint8Array(spectralPacked.buffer,0,spIdx*2);
  var rawSize=rawBytes.length;
  if(typeof CompressionStream!=='undefined'){
    var cs=new CompressionStream('gzip');
    var writer=cs.writable.getWriter();
    var reader=cs.readable.getReader();
    var chunks=[];
    reader.read().then(function pump(result){
      if(result.done){
        var compressed=new Blob(chunks);
        var compSize=0;for(var i=0;i<chunks.length;i++)compSize+=chunks[i].length;
        document.getElementById('extractInfo').textContent=(outLen/SR).toFixed(1)+'s vocal | '+nf+'/'+stftStore.length+' voice frames | '+(rawSize/1024).toFixed(0)+'KB -> '+(compSize/1024).toFixed(0)+'KB gz';
        fetch('/api/extracts',{method:'POST',headers:{'Content-Type':'application/octet-stream'},body:compressed}).then(function(){loadWaveforms();}).catch(function(){});
        return;
      }
      chunks.push(result.value);
      return reader.read().then(pump);
    });
    writer.write(rawBytes);
    writer.close();
  } else {
    document.getElementById('extractInfo').textContent=(outLen/SR).toFixed(1)+'s vocal | '+nf+'/'+stftStore.length+' voice frames | '+(rawSize/1024).toFixed(0)+'KB raw';
  }
  btn.className='btn';btn.textContent='EXTRACT';
}
/* Screenshot capture: POST spectrogram canvas as PNG every 10 seconds */
setInterval(function(){
  if(typeof sgC!=='undefined'&&sgC.toBlob){
    sgC.toBlob(function(blob){
      if(blob) fetch('/api/screenshot',{method:'POST',body:blob}).catch(function(){});
    },'image/png');
  }
},10000);

/* ── Transcript log ── */
function renderTranscriptLog(){
  var el=document.getElementById('transcriptLog');
  if(!el) return;
  el.innerHTML='';
  for(var i=0;i<transcriptHistory.length;i++){
    var e=transcriptHistory[i];
    var div=document.createElement('div');
    div.className='activity-entry';
    div.style.cssText='font-size:10px;line-height:1.4;padding:3px 4px;';
    var dt=e.t;
    var ts=(dt.getMonth()+1)+'/'+dt.getDate()+' '+dt.getHours().toString().padStart(2,'0')+':'+dt.getMinutes().toString().padStart(2,'0');
    var ch=e.ch?'CH'+e.ch:'--';
    var txt=e.clean||e.text;
    div.innerHTML='<span class="time">'+ts+'</span> <span class="ch">'+ch+'</span> '+txt;
    el.appendChild(div);
  }
}

function getChanLabel(ch){
  var l={1:'PORT-VTS',5:'PORT-VTS',6:'SAFETY',7:'COMM',8:'COMM-IS',9:'CALL',10:'COMM',11:'COMM-VTS',12:'PORT-VTS',13:'BRIDGE',14:'PORT-VTS',15:'ENV-RX',16:'SAFETY-CALL',17:'STATE',18:'COMM',19:'COMM',20:'PORT',21:'USCG',22:'USCG-LINK',23:'USCG',24:'MARINE-OP',25:'MARINE-OP',26:'MARINE-OP',27:'MARINE-OP',28:'MARINE-OP',63:'PORT-VTS',65:'PORT',66:'PORT',67:'BRIDGE-MS',68:'NON-COMM',69:'NON-COMM',70:'DSC',71:'NON-COMM',72:'NON-COMM-IS',73:'PORT',74:'PORT',77:'PORT-PILOT',78:'NON-COMM',79:'COMM',80:'COMM',81:'GOV-ENV',82:'GOV',83:'USCG',84:'MARINE-OP',85:'MARINE-OP',86:'MARINE-OP',87:'MARINE-OP',88:'COMM-IS'};
  return l[ch]||'UNKNOWN';
}

/* ── Load past recordings into event log on startup ── */
(function(){
  fetch('/api/recordings').then(function(r){return r.json();}).then(function(recs){
    var log=document.getElementById('activityLog');
    for(var i=0;i<Math.min(recs.length,30);i++){
      var r=recs[i];
      var div=document.createElement('div');
      div.className='activity-entry';
      div.style.cssText='display:flex;align-items:center;justify-content:space-between;';
      var ts='';
      var m=r.filename.match(/(\d{4})(\d{2})(\d{2})_(\d{2})(\d{2})(\d{2})/);
      if(m) ts=m[4]+':'+m[5]+':'+m[6];
      var ch='';
      var cm=r.filename.match(/Ch(\d+)/);
      if(cm) ch='CH'+cm[1];
      var kb=Math.round(r.size/1024);
      div.innerHTML='<span><span class="time">'+ts+'</span> <span class="ch">'+ch+'</span></span><span class="play-badge ready" data-url="'+r.url+'" title="Play">&#9654;<span class="sz">'+kb+'K</span></span>';
      log.appendChild(div);
    }
  }).catch(function(){});
})();

/* ── Waveforms (extracts) list ── */
function loadWaveforms(){
  fetch('/api/extracts').then(function(r){return r.json();}).then(function(exts){
    var el=document.getElementById('waveformsList');
    el.innerHTML='';
    /* Group by timestamp: extract_YYYYMMDD_HHMMSS.{wav,png,stft.gz} */
    var groups={};
    for(var i=0;i<exts.length;i++){
      var e=exts[i];
      var tm=e.filename.match(/extract_(\d{8}_\d{6})/);
      var key=tm?tm[1]:e.filename;
      if(!groups[key]) groups[key]={wav:null,png:null,stft:null};
      if(e.filename.endsWith('.wav')) groups[key].wav=e;
      else if(e.filename.endsWith('.png')) groups[key].png=e;
      else if(e.filename.endsWith('.stft.gz')) groups[key].stft=e;
    }
    var keys=Object.keys(groups).sort().reverse();
    for(var k=0;k<keys.length;k++){
      var g=groups[keys[k]];
      if(!g.wav&&!g.png) continue;
      var div=document.createElement('div');
      div.className='activity-entry';
      div.style.cssText='display:flex;flex-direction:column;gap:2px;padding:3px 4px;';
      /* Timestamp from key */
      var tm=keys[k].match(/(\d{4})(\d{2})(\d{2})_(\d{2})(\d{2})(\d{2})/);
      var ts=tm?tm[4]+':'+tm[5]+':'+tm[6]:'';
      /* Sizes */
      var sizes=[];
      if(g.wav) sizes.push((g.wav.size/1024).toFixed(0)+'KB wav');
      if(g.stft) sizes.push((g.stft.size/1024).toFixed(0)+'KB stft');
      if(g.png) sizes.push((g.png.size/1024).toFixed(0)+'KB png');
      var topRow=document.createElement('div');
      topRow.style.cssText='display:flex;justify-content:space-between;align-items:center;';
      topRow.innerHTML='<span class="time">'+ts+'</span><span style="font-size:9px;color:#555;">'+sizes.join(' | ')+'</span>';
      div.appendChild(topRow);
      /* Spectrogram thumbnail if PNG available */
      if(g.png){
        var img=document.createElement('img');
        img.src=g.png.url;
        img.style.cssText='width:100%;height:28px;object-fit:cover;border-radius:2px;opacity:0.8;margin-top:2px;';
        div.appendChild(img);
      }
      /* Play link */
      if(g.wav){
        var playLink=document.createElement('a');
        playLink.href='#';
        playLink.style.cssText='font-size:9px;color:#00cc55;text-decoration:none;cursor:pointer;';
        playLink.textContent='[play extract]';
        playLink.setAttribute('data-url',g.wav.url);
        playLink.addEventListener('click',function(ev){
          ev.preventDefault();
          var url=this.getAttribute('data-url');
          var player=document.getElementById('extractPlayer');
          if(player.src) URL.revokeObjectURL(player.src);
          player.src=url;
          document.getElementById('extractPanel').style.display='block';
          document.getElementById('extractInfo').textContent='Extract: '+this.parentElement.parentElement.querySelector('.time').textContent;
          player.play();
        });
        div.appendChild(playLink);
      }
      el.appendChild(div);
    }
  }).catch(function(){});
}
loadWaveforms();
setInterval(loadWaveforms,15000);

/* ── GMT Clock + World Time ── */
function updateClocks(){
  var now=new Date();
  var gmt=now.toUTCString().match(/(\d{2}:\d{2}:\d{2})/);
  var gmtEl=document.getElementById('gmtClock');
  if(gmtEl) gmtEl.innerHTML=(gmt?gmt[1]:'--:--:--')+' <span style="font-size:10px;color:#00cc55;">GMT</span>';
  var wt=document.getElementById('worldTime');
  if(wt){
    var zones=[
      {label:'AST',tz:'America/Puerto_Rico',off:'-4'},
      {label:'EST',tz:'America/New_York',off:'-5'},
      {label:'CST',tz:'America/Chicago',off:'-6'},
      {label:'MST',tz:'America/Denver',off:'-7'},
      {label:'PST',tz:'America/Los_Angeles',off:'-8'},
      {label:'GMT',tz:'Europe/London',off:'+0'},
      {label:'CET',tz:'Europe/Berlin',off:'+1'},
      {label:'GST',tz:'Asia/Dubai',off:'+4'},
      {label:'IST',tz:'Asia/Kolkata',off:'+5.5'},
      {label:'HKT',tz:'Asia/Hong_Kong',off:'+8'},
      {label:'JST',tz:'Asia/Tokyo',off:'+9'},
      {label:'AEST',tz:'Australia/Sydney',off:'+11'}
    ];
    var html='';
    for(var i=0;i<zones.length;i++){
      var z=zones[i];
      var t=now.toLocaleTimeString('en-US',{timeZone:z.tz,hour:'2-digit',minute:'2-digit',hour12:false});
      var h=parseInt(now.toLocaleTimeString('en-US',{timeZone:z.tz,hour:'2-digit',hour12:false}));
      var icon='';
      if(h>=6&&h<8) icon='\uD83C\uDF05';        /* sunrise */
      else if(h>=17&&h<19) icon='\uD83C\uDF07';  /* sunset */
      else if(h>=8&&h<17) icon='\u2600\uFE0F';   /* day */
      else icon='\uD83C\uDF19';                   /* night */
      html+='<div style="display:flex;gap:6px;"><span style="flex:1;color:#00cc55;">'+z.label+'</span><span style="flex:1;">'+t+'</span><span style="width:28px;text-align:right;color:#556;">'+z.off+'</span><span style="width:16px;text-align:center;">'+icon+'</span></div>';
    }
    wt.innerHTML=html;
  }
}
updateClocks();
setInterval(updateClocks,1000);

document.addEventListener('click',function(){_dbg('first click -> initAudio');initAudio();},{once:true});
_dbg('calling connectWS...');
connectWS();
_dbg('render loop starting — canvas dims: wave='+waveC.width+'x'+waveC.height+' spec='+specC.width+'x'+specC.height+' sg='+sgC.width+'x'+sgC.height);
render();
console.timeEnd('[DBG] total-init');
_dbg('=== BOOT COMPLETE ===');
</script>
</body>
</html>
"##;
