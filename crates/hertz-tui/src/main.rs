use clap::{Parser, Subcommand};
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Terminal,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

use hertz_tui::{
    bins_to_columns, colormap_from_str, colormap_to_str, decode_spectrum_frame,
    make_swept_peak_frame, next_colormap, tuned_col, Colormap, Config, History, Palette, Row,
    SpectrumFrame, WaterfallWidget,
};
use hertz_types::{
    wire::{DongleSummary, StatusResponse, TuneRequest},
    DoctorReport, Event as HertzEvent, RecordingFileEntry, WsServerMsg,
};

// ---------------------------------------------------------------------------
// CLI Arg Parsing
// ---------------------------------------------------------------------------

#[derive(Clone, Parser, Debug)]
#[command(name = "hertz", about = "Hertz SDR Operator Console & CLI tool")]
struct Cli {
    #[arg(long, default_value = "http://localhost:9080")]
    connect: String,

    #[arg(long, env = "HERTZ_TOKEN")]
    token: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Clone, Subcommand, Debug)]
enum Commands {
    /// Launch the interactive operator TUI console (default)
    Tui {
        /// Run with a synthetic sweeping peak (no hardware needed)
        #[arg(long)]
        demo: bool,

        /// Disable audio playback locally
        #[arg(long)]
        no_audio: bool,
    },
    /// Print daemon status
    Status,
    /// Run daemon diagnostic checks
    Doctor,
    /// List recordings
    Records,
    /// Stream live daemon events to stdout
    Tail,
    /// Tune a dongle to a frequency or channel
    Tune {
        /// Frequency in Hz or Channel ID
        target: String,

        /// Target Dongle ID (if omitted, tunes first available)
        #[arg(long)]
        dongle: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// TUI Connection State and Event definitions
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectionState {
    Offline,
    Connecting,
    Connected,
}

enum TuiEvent {
    ConnectionState(ConnectionState),
    ServerMsg(WsServerMsg),
    WsError(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaneType {
    Dongles,
    Waterfall,
    ChannelGrid,
    Activity,
    Transcripts,
}

// ---------------------------------------------------------------------------
// Audio Player Interface (CPAL)
// ---------------------------------------------------------------------------

struct AudioPlayer {
    _stream: cpal::Stream,
}

impl AudioPlayer {
    pub fn new(no_audio: bool) -> Option<(Self, rtrb::Producer<f32>)> {
        if no_audio {
            return None;
        }
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        let host = cpal::default_host();
        let device = host.default_output_device()?;
        let config = device.default_output_config().ok()?;

        let sample_rate = config.sample_rate().0;
        let channels = config.channels();

        // 2 seconds buffer at 48000 Hz
        let (producer, mut consumer) = rtrb::RingBuffer::new(96000);

        let stream_config = cpal::StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let ratio = 48000.0f64 / sample_rate as f64;
        let mut input_sample_idx = 0.0f64;
        let mut last_samples = [0.0f32; 2];

        let stream = device
            .build_output_stream(
                &stream_config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    for frame in data.chunks_mut(channels as usize) {
                        let sample = if consumer.is_empty() {
                            0.0f32
                        } else if sample_rate == 48000 {
                            consumer.pop().unwrap_or(0.0)
                        } else {
                            while input_sample_idx >= 1.0 {
                                if let Ok(s) = consumer.pop() {
                                    last_samples[0] = last_samples[1];
                                    last_samples[1] = s;
                                } else {
                                    last_samples[0] = last_samples[1];
                                }
                                input_sample_idx -= 1.0;
                            }
                            let val = last_samples[0]
                                + (last_samples[1] - last_samples[0]) * input_sample_idx as f32;
                            input_sample_idx += ratio;
                            val
                        };

                        // Scale volume to 0.035 as required
                        let val = sample * 0.035;
                        for ch in frame.iter_mut() {
                            *ch = val;
                        }
                    }
                },
                |err| log::error!("cpal stream error: {:?}", err),
                None,
            )
            .ok()?;

        stream.play().ok()?;

        Some((Self { _stream: stream }, producer))
    }
}

// ---------------------------------------------------------------------------
// TUI App State
// ---------------------------------------------------------------------------

struct AppState {
    conn_state: ConnectionState,
    focused_pane: PaneType,
    dongles: Vec<DongleSummary>,
    selected_dongle_idx: usize,

    // Waterfall state
    waterfall_hist: History,
    center_hz: f64,
    span_hz: f64,
    tuned_hz: f64,
    squelch_open: bool,
    paused: bool,
    db_floor: f32,
    db_ceil: f32,
    colormap: Colormap,
    newest_on_top: bool,

    // Logs & Grid
    activity_log: Vec<String>,
    transcripts: Vec<String>,
    channel_activity: Vec<hertz_types::ChannelActivityInfo>,

    // Keyboard Dialogs
    tune_dialog: Option<String>,
    squelch_dialog: Option<String>,
    tx_dialog: Option<TxState>,

    // Status / Msg
    status_message: String,
    status_time: Instant,
    tx_enabled: bool,
}

#[derive(Clone)]
enum TxState {
    Composing(String),
    Confirming(String),
    Transmitting { text: String, start: Instant },
}

impl AppState {
    fn new(cfg: &Config) -> Self {
        Self {
            conn_state: ConnectionState::Offline,
            focused_pane: PaneType::Waterfall,
            dongles: Vec::new(),
            selected_dongle_idx: 0,
            waterfall_hist: History::new(cfg.max_rows),
            center_hz: 150_000_000.0,
            span_hz: 2_000_000.0,
            tuned_hz: cfg.tuned_hz,
            squelch_open: false,
            paused: false,
            db_floor: cfg.db_floor,
            db_ceil: cfg.db_ceil,
            colormap: colormap_from_str(&cfg.colormap),
            newest_on_top: cfg.newest_on_top,
            activity_log: Vec::new(),
            transcripts: Vec::new(),
            channel_activity: Vec::new(),
            tune_dialog: None,
            squelch_dialog: None,
            tx_dialog: None,
            status_message: "Press '?' for help".to_string(),
            status_time: Instant::now(),
            tx_enabled: false,
        }
    }

    fn active_dongle(&self) -> Option<&DongleSummary> {
        self.dongles.get(self.selected_dongle_idx)
    }

    fn push_activity(&mut self, s: String) {
        self.activity_log.push(s);
        if self.activity_log.len() > 100 {
            self.activity_log.remove(0);
        }
    }

    fn push_transcript(&mut self, s: String) {
        self.transcripts.push(s);
        if self.transcripts.len() > 100 {
            self.transcripts.remove(0);
        }
    }

    fn set_status(&mut self, s: &str) {
        self.status_message = s.to_string();
        self.status_time = Instant::now();
    }
}

// ---------------------------------------------------------------------------
// HTTP/REST Daemon Client
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct DaemonClient {
    client: reqwest::Client,
    base_url: String,
    token: Option<String>,
}

impl DaemonClient {
    fn new(base_url: String, token: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            token,
        }
    }

    fn auth_req(&self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(ref t) = self.token {
            req = req.bearer_auth(t);
        }
        req
    }

    async fn get_status(&self) -> anyhow::Result<StatusResponse> {
        let res = self
            .auth_req(self.client.get(format!("{}/api/status", self.base_url)))
            .send()
            .await?;
        if !res.status().is_success() {
            return Err(anyhow::anyhow!("status endpoint error: {}", res.status()));
        }
        Ok(res.json::<StatusResponse>().await?)
    }

    async fn get_doctor(&self) -> anyhow::Result<DoctorReport> {
        let res = self
            .auth_req(self.client.get(format!("{}/api/doctor", self.base_url)))
            .send()
            .await?;
        if !res.status().is_success() {
            return Err(anyhow::anyhow!("doctor endpoint error: {}", res.status()));
        }
        Ok(res.json::<DoctorReport>().await?)
    }

    async fn get_recordings(&self) -> anyhow::Result<Vec<RecordingFileEntry>> {
        let res = self
            .auth_req(self.client.get(format!("{}/api/recordings", self.base_url)))
            .send()
            .await?;
        if !res.status().is_success() {
            return Err(anyhow::anyhow!(
                "recordings endpoint error: {}",
                res.status()
            ));
        }
        Ok(res.json::<Vec<RecordingFileEntry>>().await?)
    }

    async fn tune(&self, dongle_id: &str, target: &str) -> anyhow::Result<()> {
        let freq_hz = target.parse::<u64>().ok();
        let channel_id = if freq_hz.is_none() {
            Some(target.to_string())
        } else {
            None
        };
        let req = TuneRequest {
            channel_id,
            freq_hz,
        };
        let res = self
            .auth_req(
                self.client
                    .post(format!("{}/api/dongles/{}/tune", self.base_url, dongle_id))
                    .json(&req),
            )
            .send()
            .await?;
        if !res.status().is_success() {
            return Err(anyhow::anyhow!("tune failed: {}", res.status()));
        }
        Ok(())
    }

    async fn squelch(&self, dongle_id: &str, squelch_db: f32) -> anyhow::Result<()> {
        let req = hertz_types::SquelchRequest { squelch_db };
        let res = self
            .auth_req(
                self.client
                    .post(format!(
                        "{}/api/dongles/{}/squelch",
                        self.base_url, dongle_id
                    ))
                    .json(&req),
            )
            .send()
            .await?;
        if !res.status().is_success() {
            return Err(anyhow::anyhow!("squelch failed: {}", res.status()));
        }
        Ok(())
    }

    async fn set_recording(&self, dongle_id: &str, record: bool) -> anyhow::Result<()> {
        let req = hertz_types::RecordingRequest { record };
        let res = self
            .auth_req(
                self.client
                    .post(format!(
                        "{}/api/dongles/{}/recording",
                        self.base_url, dongle_id
                    ))
                    .json(&req),
            )
            .send()
            .await?;
        if !res.status().is_success() {
            return Err(anyhow::anyhow!("recording failed: {}", res.status()));
        }
        Ok(())
    }

    async fn set_listening(&self, dongle_id: &str, listen: bool) -> anyhow::Result<()> {
        let req = hertz_types::ListenRequest { listen };
        let res = self
            .auth_req(
                self.client
                    .post(format!(
                        "{}/api/dongles/{}/listen",
                        self.base_url, dongle_id
                    ))
                    .json(&req),
            )
            .send()
            .await?;
        if !res.status().is_success() {
            return Err(anyhow::anyhow!("listen failed: {}", res.status()));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Main Entrypoint
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Cli::parse();

    let client = DaemonClient::new(args.connect.clone(), args.token.clone());

    match args.command.unwrap_or(Commands::Tui {
        demo: false,
        no_audio: false,
    }) {
        Commands::Tui { demo, no_audio } => {
            run_tui(demo, no_audio, client, args.connect, args.token).await?;
        }
        Commands::Status => {
            let status = client.get_status().await?;
            println!("Hertz Backend Status:");
            println!("Version: {}", status.version);
            println!("Uptime: {:.1}s", status.uptime_sec);
            println!("Channels Loaded: {}", status.channels_loaded);
            println!("Auth Required: {}", status.auth_required);
            println!("Dongles ({}):", status.dongles.len());
            for d in status.dongles {
                println!(
                    "  - {} [{}]: online={}, freq={} Hz, squelch={:.1} dB, recording={}, listening={}",
                    d.id,
                    colormap_to_str(colormap_from_str(&d.bandplan.clone().unwrap_or_default())),
                    d.online,
                    d.freq_hz,
                    d.squelch_db,
                    d.recording,
                    d.listening
                );
            }
        }
        Commands::Doctor => {
            let doc = client.get_doctor().await?;
            println!("Hertz Doctor Diagnostic Report:");
            println!("Platform: {}", doc.platform);
            println!("Channels Loaded: {}", doc.channels_loaded);
            println!("Dongles ({}):", doc.dongles.len());
            for d in doc.dongles {
                println!(
                    "  - {} [{}]: online={}, dropped={}, read_errors={}",
                    d.id, d.serial, d.online, d.dropped_samples, d.read_errors
                );
            }
            if !doc.messages.is_empty() {
                println!("System Messages:");
                for m in doc.messages {
                    println!("  * {}", m);
                }
            }
        }
        Commands::Records => {
            let recs = client.get_recordings().await?;
            println!(
                "{:<35} {:<15} {:<8} {:<10} {:<10}",
                "Filename", "Frequency", "Channel", "Duration", "Size"
            );
            println!("{:-<85}", "");
            for r in recs {
                let ch = r.channel.unwrap_or_else(|| "N/A".to_string());
                println!(
                    "{:<35} {:<15} {:<8} {:<10.1} {:<10}",
                    r.filename,
                    format!("{:.4} MHz", r.freq_hz as f64 / 1_000_000.0),
                    ch,
                    r.duration_sec,
                    r.size_bytes
                );
            }
        }
        Commands::Tail => {
            let mut ws_url = args
                .connect
                .replace("http://", "ws://")
                .replace("https://", "wss://");
            ws_url = format!("{}/stream?events=all&audio=none", ws_url);
            let mut request = ws_url.as_str().into_client_request()?;
            if let Some(ref t) = args.token {
                request
                    .headers_mut()
                    .insert("Authorization", format!("Bearer {}", t).parse().unwrap());
            }
            println!("Connecting to event stream at {}...", ws_url);
            let (ws_stream, _) = tokio_tungstenite::connect_async(request).await?;
            println!("Connected. Streaming events...");
            let (_, mut read) = ws_stream.split();
            while let Some(msg_res) = read.next().await {
                match msg_res {
                    Ok(Message::Text(t)) => {
                        println!("{}", t);
                    }
                    Ok(Message::Close(_)) => {
                        println!("Connection closed by server.");
                        break;
                    }
                    Err(e) => {
                        eprintln!("WS Error: {:?}", e);
                        break;
                    }
                    _ => {}
                }
            }
        }
        Commands::Tune { target, dongle } => {
            let dongle_id = match dongle {
                Some(id) => id,
                None => {
                    let st = client.get_status().await?;
                    if st.dongles.is_empty() {
                        return Err(anyhow::anyhow!("No dongles online"));
                    }
                    st.dongles[0].id.clone()
                }
            };
            client.tune(&dongle_id, &target).await?;
            println!(
                "Successfully tuned dongle {} to target {}",
                dongle_id, target
            );
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Interactive TUI Runner
// ---------------------------------------------------------------------------

async fn run_tui(
    demo: bool,
    no_audio: bool,
    client: DaemonClient,
    connect_url: String,
    token: Option<String>,
) -> anyhow::Result<()> {
    let config = Config::load();

    // Setup terminal
    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Safe restore on panic
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::event::DisableMouseCapture
        );
        default_hook(info);
    }));

    let mut state = AppState::new(&config);
    let palette = Palette::new();

    // Channels for async communication
    let (frame_tx, frame_rx) = crossbeam_channel::bounded::<SpectrumFrame>(8);
    let (event_tx, event_rx) = crossbeam_channel::unbounded::<TuiEvent>();

    // Shared flags
    let keyed = Arc::new(AtomicBool::new(false));
    let (audio_player, audio_tx) = if let Some((ap, tx)) = AudioPlayer::new(no_audio) {
        (Some(ap), Some(tx))
    } else {
        (None, None)
    };
    let _player = audio_player; // keep stream alive on the main thread
    let audio_tx_shared = Arc::new(Mutex::new(audio_tx));
    let selected_dongle_id = Arc::new(Mutex::new(None));
    if demo {
        state.conn_state = ConnectionState::Connected;
        state.tx_enabled = true;
        // Mock daemon info
        state.dongles = vec![
            DongleSummary {
                id: "DEMO01".to_string(),
                serial: "DEMO01".to_string(),
                role: hertz_types::DongleRole::Monitor,
                online: true,
                bandplan: Some("marine-vhf-us".to_string()),
                tap_channel: None,
                groups: vec![],
                freq_hz: 150_000_000,
                squelch_db: 12.0,
                recording: false,
                listening: true,
                message: None,
            },
            DongleSummary {
                id: "DEMO02".to_string(),
                serial: "DEMO02".to_string(),
                role: hertz_types::DongleRole::Channelized,
                online: true,
                bandplan: Some("frs-gmrs".to_string()),
                tap_channel: Some("16".to_string()),
                groups: vec![],
                freq_hz: 462_562_500,
                squelch_db: 9.0,
                recording: true,
                listening: false,
                message: None,
            },
        ];
        *selected_dongle_id.lock().await = Some("DEMO01".to_string());

        // Spawn demo frame generator thread
        let frame_tx_clone = frame_tx.clone();
        let keyed_clone = keyed.clone();
        tokio::spawn(async move {
            let start = Instant::now();
            let mut last_key_toggle = Instant::now();
            let mut key_state = false;
            loop {
                let elapsed = start.elapsed().as_secs_f64();

                // Toggle PTT key state every 4 seconds
                if last_key_toggle.elapsed() >= Duration::from_secs(4) {
                    key_state = !key_state;
                    keyed_clone.store(key_state, Ordering::Relaxed);
                    last_key_toggle = Instant::now();
                }

                let f = make_swept_peak_frame(
                    elapsed,
                    config.fft_size,
                    150_000_000.0,
                    2_000_000.0,
                    150_000_000.0,
                );
                let _ = frame_tx_clone.try_send(f);
                tokio::time::sleep(Duration::from_millis(1000 / config.target_fps as u64)).await;
            }
        });
    } else {
        // Run real network loops
        let event_tx_clone = event_tx.clone();
        let frame_tx_clone = frame_tx.clone();
        let audio_tx_clone = audio_tx_shared.clone();
        let _selected_dongle_id_clone = selected_dongle_id.clone();

        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);
            loop {
                event_tx_clone
                    .send(TuiEvent::ConnectionState(ConnectionState::Connecting))
                    .unwrap_or(());

                let ws_url = if connect_url.starts_with("http://") {
                    connect_url.replace("http://", "ws://")
                } else if connect_url.starts_with("https://") {
                    connect_url.replace("https://", "wss://")
                } else {
                    connect_url.clone()
                };

                let ws_url_full = format!("{}/stream?events=all&audio=all&spectrum=all", ws_url);

                let mut request = match ws_url_full.as_str().into_client_request() {
                    Ok(req) => req,
                    Err(e) => {
                        let _ =
                            event_tx_clone.send(TuiEvent::WsError(format!("Invalid URL: {}", e)));
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                };

                if let Some(ref t) = token {
                    request
                        .headers_mut()
                        .insert("Authorization", format!("Bearer {}", t).parse().unwrap());
                }

                match tokio_tungstenite::connect_async(request).await {
                    Ok((ws_stream, _)) => {
                        backoff = Duration::from_secs(1);
                        let _ = event_tx_clone
                            .send(TuiEvent::ConnectionState(ConnectionState::Connected));

                        let (_, mut read) = ws_stream.split();
                        while let Some(msg_res) = read.next().await {
                            match msg_res {
                                Ok(Message::Text(t)) => {
                                    if let Ok(parsed) = serde_json::from_str::<WsServerMsg>(&t) {
                                        let _ = event_tx_clone.send(TuiEvent::ServerMsg(parsed));
                                    }
                                }
                                Ok(Message::Binary(bin)) => {
                                    if bin.starts_with(&[0x02]) {
                                        if let Some(frame) = decode_spectrum_frame(&bin) {
                                            let _ = frame_tx_clone.try_send(frame);
                                        }
                                    } else {
                                        if let Ok(audio) =
                                            hertz_types::wire::decode_audio_frame(&bin)
                                        {
                                            // Extract PCM data if we are listening to this channel
                                            // Check if we have audio player
                                            let mut tx = audio_tx_clone.lock().await;
                                            if let Some(ref mut p) = *tx {
                                                for &s in &audio.pcm {
                                                    let _ = p.push(s);
                                                }
                                            }
                                        }
                                    }
                                }
                                Ok(Message::Close(_)) => break,
                                Err(_) => break,
                                _ => {}
                            }
                        }
                    }
                    Err(e) => {
                        let _ = event_tx_clone
                            .send(TuiEvent::WsError(format!("Connection failed: {}", e)));
                    }
                }

                let _ = event_tx_clone.send(TuiEvent::ConnectionState(ConnectionState::Offline));
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
            }
        });

        // Query initial API info
        let client_clone = client.clone();
        tokio::spawn(async move {
            if let Ok(_st) = client_clone.get_status().await {
                // If auth settings require it, we might be blocked, but check if we succeeded
                // Send updates or trigger initial loading.
                // We'll update state from live WS frames mostly.
            }
        });
    }

    // Main TUI Draw/Event loop
    let budget = Duration::from_secs_f64(1.0 / config.target_fps as f64);
    let mut last_draw = Instant::now();

    loop {
        // --- Process Crossterm Keyboard/Mouse events ---
        if event::poll(Duration::from_millis(5))? {
            if let Event::Key(k) = event::read()? {
                // Quit globally
                if k.code == KeyCode::Char('q') {
                    break;
                }

                // Check if dialog is open
                if let Some(ref mut val) = state.tune_dialog {
                    match k.code {
                        KeyCode::Esc => state.tune_dialog = None,
                        KeyCode::Enter => {
                            let target = val.clone();
                            state.tune_dialog = None;
                            if let Some(active) = state.active_dongle() {
                                let id = active.id.clone();
                                let client_clone = client.clone();
                                let is_demo = demo;
                                tokio::spawn(async move {
                                    if !is_demo {
                                        let _ = client_clone.tune(&id, &target).await;
                                    }
                                });
                            }
                        }
                        KeyCode::Char(c) => val.push(c),
                        KeyCode::Backspace => {
                            val.pop();
                        }
                        _ => {}
                    }
                } else if let Some(ref mut val) = state.squelch_dialog {
                    match k.code {
                        KeyCode::Esc => state.squelch_dialog = None,
                        KeyCode::Enter => {
                            if let Ok(db) = val.parse::<f32>() {
                                state.squelch_dialog = None;
                                if let Some(active) = state.active_dongle() {
                                    let id = active.id.clone();
                                    let client_clone = client.clone();
                                    let is_demo = demo;
                                    tokio::spawn(async move {
                                        if !is_demo {
                                            let _ = client_clone.squelch(&id, db).await;
                                        }
                                    });
                                }
                            }
                        }
                        KeyCode::Char(c) => {
                            if c.is_ascii_digit() || c == '.' || c == '-' {
                                val.push(c);
                            }
                        }
                        KeyCode::Backspace => {
                            val.pop();
                        }
                        _ => {}
                    }
                } else if let Some(ref mut tx_state) = state.tx_dialog {
                    match tx_state {
                        TxState::Composing(ref mut val) => match k.code {
                            KeyCode::Esc => state.tx_dialog = None,
                            KeyCode::Enter => {
                                if !val.trim().is_empty() {
                                    *tx_state = TxState::Confirming(val.clone());
                                }
                            }
                            KeyCode::Char(c) => val.push(c),
                            KeyCode::Backspace => {
                                val.pop();
                            }
                            _ => {}
                        },
                        TxState::Confirming(ref text) => match k.code {
                            KeyCode::Esc => state.tx_dialog = None,
                            KeyCode::Char('y') | KeyCode::Char('Y') => {
                                // Confirm send
                                let text_clone = text.clone();
                                if demo {
                                    // In demo mode, start a mock transmission
                                    *tx_state = TxState::Transmitting {
                                        text: text_clone,
                                        start: Instant::now(),
                                    };
                                } else {
                                    state.tx_dialog = None;
                                    // REST endpoint for TX not in daemon yet (Phase 8), but we can watch events
                                }
                            }
                            KeyCode::Char('n') | KeyCode::Char('N') => {
                                *tx_state = TxState::Composing(text.clone());
                            }
                            _ => {}
                        },
                        TxState::Transmitting { .. } => {
                            // Can't compose while transmitting, wait
                        }
                    }
                } else {
                    // Global Hotkeys
                    match k.code {
                        KeyCode::Tab => {
                            // Cycle focus
                            state.focused_pane = match state.focused_pane {
                                PaneType::Dongles => PaneType::Waterfall,
                                PaneType::Waterfall => PaneType::ChannelGrid,
                                PaneType::ChannelGrid => PaneType::Activity,
                                PaneType::Activity => PaneType::Transcripts,
                                PaneType::Transcripts => PaneType::Dongles,
                            };
                        }
                        KeyCode::Char('t') => {
                            state.tune_dialog = Some(String::new());
                        }
                        KeyCode::Char('s') => {
                            state.squelch_dialog = Some(String::new());
                        }
                        KeyCode::Char('r') => {
                            if let Some(active) = state.active_dongle() {
                                let id = active.id.clone();
                                let record = !active.recording;
                                let client_clone = client.clone();
                                let is_demo = demo;
                                tokio::spawn(async move {
                                    if !is_demo {
                                        let _ = client_clone.set_recording(&id, record).await;
                                    }
                                });
                            }
                        }
                        KeyCode::Char('x') => {
                            state.tx_dialog = Some(TxState::Composing(String::new()));
                        }
                        KeyCode::Char(' ') => {
                            if state.focused_pane == PaneType::Waterfall {
                                state.paused = !state.paused;
                            } else {
                                if let Some(active) = state.active_dongle() {
                                    let id = active.id.clone();
                                    let listen = !active.listening;
                                    let client_clone = client.clone();
                                    let is_demo = demo;
                                    tokio::spawn(async move {
                                        if !is_demo {
                                            let _ = client_clone.set_listening(&id, listen).await;
                                        }
                                    });
                                }
                            }
                        }
                        // Arrow keys & pane navigation
                        KeyCode::Up => {
                            if state.focused_pane == PaneType::Dongles
                                && state.selected_dongle_idx > 0
                            {
                                state.selected_dongle_idx -= 1;
                                let selected_id =
                                    state.dongles[state.selected_dongle_idx].id.clone();
                                *selected_dongle_id.lock().await = Some(selected_id);
                            }
                        }
                        KeyCode::Down => {
                            if state.focused_pane == PaneType::Dongles
                                && state.selected_dongle_idx + 1 < state.dongles.len()
                            {
                                state.selected_dongle_idx += 1;
                                let selected_id =
                                    state.dongles[state.selected_dongle_idx].id.clone();
                                *selected_dongle_id.lock().await = Some(selected_id);
                            }
                        }
                        // Waterfall navigation (Arrow keys)
                        KeyCode::Left => {
                            if state.focused_pane == PaneType::Waterfall {
                                let step = if k.modifiers.contains(KeyModifiers::SHIFT) {
                                    -config.tune_step_coarse_hz
                                } else {
                                    -config.tune_step_hz
                                };
                                state.tuned_hz = (state.tuned_hz + step).max(0.0);
                                let id = state.active_dongle().map(|d| d.id.clone());
                                let client_clone = client.clone();
                                let target_freq = state.tuned_hz as u64;
                                let is_demo = demo;
                                tokio::spawn(async move {
                                    if !is_demo {
                                        if let Some(ref d_id) = id {
                                            let _ = client_clone
                                                .tune(d_id, &target_freq.to_string())
                                                .await;
                                        }
                                    }
                                });
                            }
                        }
                        KeyCode::Right => {
                            if state.focused_pane == PaneType::Waterfall {
                                let step = if k.modifiers.contains(KeyModifiers::SHIFT) {
                                    config.tune_step_coarse_hz
                                } else {
                                    config.tune_step_hz
                                };
                                state.tuned_hz = (state.tuned_hz + step).max(0.0);
                                let id = state.active_dongle().map(|d| d.id.clone());
                                let client_clone = client.clone();
                                let target_freq = state.tuned_hz as u64;
                                let is_demo = demo;
                                tokio::spawn(async move {
                                    if !is_demo {
                                        if let Some(ref d_id) = id {
                                            let _ = client_clone
                                                .tune(d_id, &target_freq.to_string())
                                                .await;
                                        }
                                    }
                                });
                            }
                        }
                        // DB floor/ceil controls
                        KeyCode::Char('[') => {
                            state.db_floor -= 5.0;
                        }
                        KeyCode::Char(']') => {
                            state.db_floor += 5.0;
                        }
                        KeyCode::Char('{') => {
                            state.db_ceil -= 5.0;
                        }
                        KeyCode::Char('}') => {
                            state.db_ceil += 5.0;
                        }
                        KeyCode::Char('c') => {
                            state.colormap = next_colormap(state.colormap);
                        }
                        _ => {}
                    }
                }
            }
        }

        // --- Process WS / SSE / Demo events from network thread ---
        while let Ok(evt) = event_rx.try_recv() {
            match evt {
                TuiEvent::ConnectionState(cs) => {
                    state.conn_state = cs;
                    match cs {
                        ConnectionState::Offline => state.set_status("Daemon connection: OFFLINE"),
                        ConnectionState::Connecting => state.set_status("Connecting to daemon..."),
                        ConnectionState::Connected => state.set_status("Connected to daemon."),
                    }
                }
                TuiEvent::WsError(err) => {
                    state.set_status(&format!("WS Error: {}", err));
                }
                TuiEvent::ServerMsg(msg) => match msg {
                    WsServerMsg::Hello { version, dongles } => {
                        state.dongles = dongles;
                        state.set_status(&format!("Daemon Version: {}", version));
                        if state.selected_dongle_idx >= state.dongles.len() {
                            state.selected_dongle_idx = 0;
                        }
                        if let Some(d) = state.active_dongle() {
                            *selected_dongle_id.lock().await = Some(d.id.clone());
                        }
                    }
                    WsServerMsg::Event(ev) => match ev {
                        HertzEvent::DongleStatus {
                            dongle_id,
                            online,
                            role,
                            serial,
                            message,
                        } => {
                            if let Some(d) = state.dongles.iter_mut().find(|d| d.id == dongle_id) {
                                d.online = online;
                                d.role = role;
                                d.serial = serial;
                                d.message = message;
                            } else {
                                // Add new dongle
                                state.dongles.push(DongleSummary {
                                    id: dongle_id.clone(),
                                    serial,
                                    role,
                                    online,
                                    bandplan: None,
                                    tap_channel: None,
                                    groups: vec![],
                                    freq_hz: 0,
                                    squelch_db: 0.0,
                                    recording: false,
                                    listening: false,
                                    message,
                                });
                            }
                        }
                        HertzEvent::SquelchEvent {
                            dongle_id,
                            channel,
                            freq_hz,
                            open,
                            signal_db,
                            classification,
                        } => {
                            let ch_str = channel.unwrap_or_default();
                            state.push_activity(format!(
                                "SQL {} on {} ({:.4} MHz) [{:.1} dB] {}",
                                if open { "OPEN" } else { "CLOSE" },
                                ch_str,
                                freq_hz as f64 / 1_000_000.0,
                                signal_db,
                                classification
                            ));
                            if let Some(active) = state.active_dongle() {
                                if active.id == dongle_id {
                                    state.squelch_open = open;
                                }
                            }
                        }
                        HertzEvent::ChannelActivity { active, .. } => {
                            state.channel_activity = active;
                        }
                        HertzEvent::Transcription {
                            dongle_id,
                            channel,
                            text,
                            ..
                        } => {
                            let ch = channel.unwrap_or_else(|| "VFO".to_string());
                            state.push_transcript(format!("[{}/{}] -> {}", dongle_id, ch, text));
                        }
                        HertzEvent::Translation {
                            dongle_id,
                            channel,
                            text,
                            language,
                            ..
                        } => {
                            let ch = channel.unwrap_or_else(|| "VFO".to_string());
                            state.push_transcript(format!(
                                "[{}/{}] ({} to en) -> {}",
                                dongle_id, ch, language, text
                            ));
                        }
                        HertzEvent::RecordingSaved {
                            dongle_id,
                            channel,
                            filename,
                            duration_sec,
                            ..
                        } => {
                            let ch = channel.unwrap_or_else(|| "VFO".to_string());
                            state.push_activity(format!(
                                "REC Saved: {}/{} [{}] ({:.1}s)",
                                dongle_id, ch, filename, duration_sec
                            ));
                        }
                        HertzEvent::TxEvent {
                            status,
                            text,
                            reason,
                            ..
                        } => {
                            state.push_activity(format!(
                                "TX Event: status={}, text={:?}, reason={:?}",
                                status, text, reason
                            ));
                            if status.to_lowercase() == "transmitting" {
                                keyed.store(true, Ordering::Relaxed);
                            } else {
                                keyed.store(false, Ordering::Relaxed);
                            }
                        }
                        _ => {}
                    },
                },
            }
        }

        // --- Drain spectrum frames ---
        if !state.paused {
            while let Ok(frame) = frame_rx.try_recv() {
                state.center_hz = frame.center_hz;
                state.span_hz = frame.span_hz;
                state.squelch_open = frame.squelch_open;
                state.waterfall_hist.push(Row {
                    bins_db: frame.bins_db,
                    squelch_open: frame.squelch_open,
                    tx: keyed.load(Ordering::Relaxed),
                });
            }
        }

        // --- Mock demo TxState processing ---
        if let Some(TxState::Transmitting { ref text, start }) = state.tx_dialog {
            if start.elapsed() >= Duration::from_secs(3) {
                // Done transmitting mock text
                state.push_activity(format!("TX finished: {}", text));
                state.tx_dialog = None;
                keyed.store(false, Ordering::Relaxed);
            } else {
                keyed.store(true, Ordering::Relaxed);
            }
        }

        // --- Draw UI ---
        if last_draw.elapsed() >= budget {
            terminal.draw(|f| {
                // Root partitions: Main + Status
                let root_layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(1), Constraint::Length(1)])
                    .split(f.area());

                // Main partitions: Top (Dongles + Waterfall), Middle (Channel Grid), Bottom (Activity + Transcript)
                let main_layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Min(6),
                        Constraint::Length(6),
                        Constraint::Length(8),
                    ])
                    .split(root_layout[0]);

                let top_layout = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Length(25), Constraint::Min(1)])
                    .split(main_layout[0]);

                let bottom_layout = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                    .split(main_layout[2]);

                // Render Dongles Pane
                let dongle_border = if state.focused_pane == PaneType::Dongles {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let dongles_list: Vec<ListItem> = state
                    .dongles
                    .iter()
                    .enumerate()
                    .map(|(i, d)| {
                        let active_indicator = if i == state.selected_dongle_idx {
                            "▸ "
                        } else {
                            "  "
                        };
                        let style = if d.online {
                            Style::default().fg(Color::Green)
                        } else {
                            Style::default().fg(Color::Red)
                        };
                        let role_str = match d.role {
                            hertz_types::DongleRole::Channelized => "chan",
                            hertz_types::DongleRole::Hopscan => "hop",
                            hertz_types::DongleRole::Monitor => "mon",
                        };
                        ListItem::new(Line::from(vec![
                            Span::styled(active_indicator, Style::default().fg(Color::Yellow)),
                            Span::styled(format!("{} ", d.id), style),
                            Span::styled(
                                format!("[{}]", role_str),
                                Style::default().fg(Color::DarkGray),
                            ),
                        ]))
                    })
                    .collect();
                let dongles_block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(dongle_border)
                    .title(" Dongles [F1] ");
                let dongles_widget = List::new(dongles_list).block(dongles_block);
                f.render_widget(dongles_widget, top_layout[0]);

                // Render Band Scope / Waterfall Pane
                let wf_border = if state.focused_pane == PaneType::Waterfall {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let waterfall_block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(wf_border)
                    .title(" Band Scope [F2] ");
                let wf_inner = waterfall_block.inner(top_layout[1]);
                f.render_widget(waterfall_block, top_layout[1]);

                if wf_inner.width > 2 && wf_inner.height > 2 {
                    // Split vertically into:
                    // 1. Spectrum Trace (if height is sufficient, e.g. >= 8)
                    // 2. Waterfall (remaining height)
                    // 3. Ruler (2 rows at bottom)
                    let show_spectrum = wf_inner.height >= 8;
                    let inner_layout = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([
                            if show_spectrum {
                                Constraint::Length(4)
                            } else {
                                Constraint::Length(0)
                            },
                            Constraint::Min(1),
                            Constraint::Length(2), // Ruler
                        ])
                        .split(wf_inner);

                    // Render Spectrum Trace
                    if show_spectrum && !state.waterfall_hist.is_empty() {
                        let newest = state.waterfall_hist.rows().front().unwrap();
                        draw_spectrum_trace(
                            f,
                            inner_layout[0],
                            &newest.bins_db,
                            state.db_floor,
                            state.db_ceil,
                        );
                    }

                    // Render Waterfall
                    let wf_row_layout = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Length(1), // Gutter
                            Constraint::Min(1),    // Waterfall
                        ])
                        .split(inner_layout[1]);

                    // Gutter
                    let gutter_area = wf_row_layout[0];
                    let rows_count = (gutter_area.height as usize) * 2;
                    let hist_rows: Vec<&Row> = state
                        .waterfall_hist
                        .rows()
                        .iter()
                        .take(rows_count)
                        .collect();

                    for cy in 0..gutter_area.height {
                        let ti = cy as usize * 2;
                        let bi = cy as usize * 2 + 1;
                        let top_tx = hist_rows.get(ti).map(|r| r.tx).unwrap_or(false);
                        let bot_tx = hist_rows.get(bi).map(|r| r.tx).unwrap_or(false);
                        let top_sql = hist_rows.get(ti).map(|r| r.squelch_open).unwrap_or(false);
                        let bot_sql = hist_rows.get(bi).map(|r| r.squelch_open).unwrap_or(false);

                        let y = gutter_area.y + cy;
                        let x = gutter_area.x;
                        if let Some(cell) = f.buffer_mut().cell_mut((x, y)) {
                            if top_tx || bot_tx {
                                cell.set_char('▐').set_fg(Color::Red).set_bg(Color::Reset);
                            } else if top_sql || bot_sql {
                                cell.set_char('▐').set_fg(Color::Green).set_bg(Color::Reset);
                            } else {
                                cell.set_char(' ');
                            }
                        }
                    }

                    // Waterfall widget
                    let t_col = tuned_col(
                        state.tuned_hz,
                        state.center_hz,
                        state.span_hz,
                        wf_row_layout[1].width,
                    );
                    let wf_widget = WaterfallWidget {
                        hist: &state.waterfall_hist,
                        floor: state.db_floor,
                        ceil: state.db_ceil,
                        cm: state.colormap,
                        newest_on_top: state.newest_on_top,
                        tuned_col: t_col,
                        palette: &palette,
                    };
                    f.render_widget(&wf_widget, wf_row_layout[1]);

                    // Render Ruler
                    draw_ruler(
                        f,
                        inner_layout[2],
                        state.center_hz,
                        state.span_hz,
                        state.tuned_hz,
                    );
                }

                // Render Channel Grid Pane
                let cg_border = if state.focused_pane == PaneType::ChannelGrid {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let grid_block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(cg_border)
                    .title(" Channel Grid [F3] ");
                let grid_inner = grid_block.inner(main_layout[1]);
                f.render_widget(grid_block, main_layout[1]);

                let mut grid_text = Vec::new();
                let max_cols = 5;
                let col_width = grid_inner.width as usize / max_cols;
                if col_width > 10 {
                    let mut current_row = Vec::new();
                    for ch in &state.channel_activity {
                        let style = Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD);
                        let name_span = Span::styled(
                            format!("{:<col_width$}", format!("{} ▆", ch.channel)),
                            style,
                        );
                        current_row.push(name_span);
                        if current_row.len() == max_cols {
                            grid_text.push(Line::from(current_row.clone()));
                            current_row.clear();
                        }
                    }
                    if !current_row.is_empty() {
                        grid_text.push(Line::from(current_row));
                    }
                }
                if grid_text.is_empty() {
                    grid_text.push(Line::from("No active channel detections."));
                }
                let grid_paragraph = Paragraph::new(grid_text).wrap(Wrap { trim: true });
                f.render_widget(grid_paragraph, grid_inner);

                // Render Activity Pane
                let act_border = if state.focused_pane == PaneType::Activity {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let act_block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(act_border)
                    .title(" Activity Log [F4] ");
                let act_inner = act_block.inner(bottom_layout[0]);
                f.render_widget(act_block, bottom_layout[0]);

                let act_lines: Vec<Line> = state
                    .activity_log
                    .iter()
                    .rev()
                    .take(act_inner.height as usize)
                    .map(|s| Line::from(s.as_str()))
                    .collect();
                let act_paragraph = Paragraph::new(act_lines);
                f.render_widget(act_paragraph, act_inner);

                // Render Transcript Pane
                let ts_border = if state.focused_pane == PaneType::Transcripts {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let ts_block = Block::default()
                    .borders(Borders::ALL)
                    .border_style(ts_border)
                    .title(" Transcript [F5] ");
                let ts_inner = ts_block.inner(bottom_layout[1]);
                f.render_widget(ts_block, bottom_layout[1]);

                let ts_lines: Vec<Line> = state
                    .transcripts
                    .iter()
                    .rev()
                    .take(ts_inner.height as usize)
                    .map(|s| Line::from(s.as_str()))
                    .collect();
                let ts_paragraph = Paragraph::new(ts_lines);
                f.render_widget(ts_paragraph, ts_inner);

                // Render Status Bar
                let sql_db = state.active_dongle().map(|d| d.squelch_db).unwrap_or(0.0);
                let rec_active = state.dongles.iter().any(|d| d.recording);
                let listen_ch = state
                    .dongles
                    .iter()
                    .find(|d| d.listening)
                    .map(|d| d.tap_channel.clone().unwrap_or_else(|| "VFO".to_string()))
                    .unwrap_or_else(|| "NONE".to_string());

                let rec_span = if rec_active {
                    Span::styled(
                        "● REC",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::styled("● REC", Style::default().fg(Color::DarkGray))
                };
                let listen_span = Span::styled(
                    format!("♪ LISTEN {}", listen_ch),
                    Style::default().fg(Color::Cyan),
                );
                let sql_span = Span::raw(format!("SQL {:.1}dB", sql_db));
                let tx_span = if state.tx_enabled {
                    Span::styled("TX:ready", Style::default().fg(Color::LightGreen))
                } else {
                    Span::styled("TX:disabled", Style::default().fg(Color::DarkGray))
                };
                let conn_span = match state.conn_state {
                    ConnectionState::Connected => {
                        Span::styled("9080 ok", Style::default().fg(Color::Green))
                    }
                    ConnectionState::Connecting => {
                        Span::styled("connecting", Style::default().fg(Color::Yellow))
                    }
                    ConnectionState::Offline => {
                        Span::styled("offline", Style::default().fg(Color::Red))
                    }
                };
                let utc_time = chrono::Utc::now().format("%H:%M").to_string();
                let time_span = Span::raw(format!("UTC {}", utc_time));

                let bar_line = Line::from(vec![
                    rec_span,
                    Span::raw(" | "),
                    listen_span,
                    Span::raw(" | "),
                    sql_span,
                    Span::raw(" | "),
                    tx_span,
                    Span::raw(" | "),
                    conn_span,
                    Span::raw(" | "),
                    time_span,
                    Span::raw(" | "),
                    Span::styled(
                        &state.status_message,
                        Style::default().fg(Color::LightYellow),
                    ),
                ]);
                let bar_paragraph = Paragraph::new(bar_line);
                f.render_widget(bar_paragraph, root_layout[1]);

                // Render Dialogs
                if let Some(ref text) = state.tune_dialog {
                    let area = centered_rect(60, 20, f.area());
                    f.render_widget(Clear, area);
                    let block = Block::default()
                        .borders(Borders::ALL)
                        .title(" Manual Tune ");
                    let p = Paragraph::new(vec![
                        Line::from("Enter center frequency (Hz) or Channel ID:"),
                        Line::from(format!("> {}", text)),
                        Line::from("Press Enter to tune, Esc to close"),
                    ])
                    .block(block);
                    f.render_widget(p, area);
                } else if let Some(ref text) = state.squelch_dialog {
                    let area = centered_rect(60, 20, f.area());
                    f.render_widget(Clear, area);
                    let block = Block::default()
                        .borders(Borders::ALL)
                        .title(" Set Squelch ");
                    let p = Paragraph::new(vec![
                        Line::from("Enter squelch threshold in dB (0.0 to 30.0):"),
                        Line::from(format!("> {}", text)),
                        Line::from("Press Enter to set, Esc to close"),
                    ])
                    .block(block);
                    f.render_widget(p, area);
                } else if let Some(ref tx_state) = state.tx_dialog {
                    let area = centered_rect(60, 25, f.area());
                    f.render_widget(Clear, area);
                    let block = Block::default()
                        .borders(Borders::ALL)
                        .title(" Compose Voice Transmission ");
                    let p = match tx_state {
                        TxState::Composing(text) => Paragraph::new(vec![
                            Line::from("Enter voice relay text to speak:"),
                            Line::from(format!("> {}", text)),
                            Line::from("Press Enter to confirm, Esc to close"),
                        ]),
                        TxState::Confirming(text) => Paragraph::new(vec![
                            Line::from("Confirm voice transmission?"),
                            Line::from(format!("Text: \"{}\"", text)),
                            Line::from("Press Y to transmit, N to edit, Esc to close"),
                        ]),
                        TxState::Transmitting { text, .. } => Paragraph::new(vec![
                            Line::from("TRANSMITTING VOICE RELAY..."),
                            Line::from(format!("Text: \"{}\"", text)),
                        ]),
                    }
                    .block(block);
                    f.render_widget(p, area);
                }
            })?;
            last_draw = Instant::now();
        }

        // Clean up status message after 4 seconds
        if state.status_time.elapsed() >= Duration::from_secs(4)
            && state.status_message != "Press '?' for help"
        {
            state.status_message = "Press '?' for help".to_string();
        }

        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Restore terminal on normal exit
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}

// ---------------------------------------------------------------------------
// UI Helper renderers
// ---------------------------------------------------------------------------

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn draw_spectrum_trace(f: &mut ratatui::Frame, area: Rect, bins: &[f32], floor: f32, ceil: f32) {
    let w = area.width as usize;
    if w == 0 || area.height == 0 || bins.is_empty() {
        return;
    }

    let h = area.height as usize;
    let mut cols = Vec::with_capacity(w);
    bins_to_columns(bins, w, &mut cols);

    for r in 0..h {
        let row_idx = h - 1 - r;
        for c in 0..w {
            let val = cols.get(c).copied().unwrap_or(floor);
            let t = ((val - floor) / (ceil - floor)).clamp(0.0, 1.0);
            let col_height = t * h as f32;

            let cell_height = col_height - r as f32;
            let ch = if cell_height <= 0.0 {
                ' '
            } else if cell_height >= 1.0 {
                '█'
            } else {
                let idx = (cell_height * 8.0).round() as usize;
                let glyphs = [' ', ' ', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
                glyphs[idx.min(8)]
            };

            let y = area.y + row_idx as u16;
            let x = area.x + c as u16;
            if let Some(cell) = f.buffer_mut().cell_mut((x, y)) {
                cell.set_char(ch).set_fg(Color::LightGreen);
            }
        }
    }
}

fn draw_ruler(f: &mut ratatui::Frame, area: Rect, center_hz: f64, span_hz: f64, tuned_hz: f64) {
    let w = area.width;
    if w < 10 {
        return;
    }

    // Row 0: Carets and tick marks
    let mut ticks_chars = vec![' '; w as usize];
    for i in 0..5 {
        let frac = i as f64 / 4.0;
        let col = (frac * (w as f64 - 1.0)).round() as usize;
        if col < ticks_chars.len() {
            ticks_chars[col] = '┬';
        }
    }

    if let Some(col) = tuned_col(tuned_hz, center_hz, span_hz, w) {
        let col = col as usize;
        if col < ticks_chars.len() {
            ticks_chars[col] = '▲';
        }
    }

    let ticks_str: String = ticks_chars.into_iter().collect();
    let ticks_line = Line::from(Span::styled(ticks_str, Style::default().fg(Color::Cyan)));

    // Row 1: Labels
    let lo = center_hz - span_hz / 2.0;
    let left_f = lo / 1_000_000.0;
    let mid_f = center_hz / 1_000_000.0;
    let right_f = (center_hz + span_hz / 2.0) / 1_000_000.0;

    let left_s = format!("{:.3}M", left_f);
    let mid_s = format!("{:.3}M", mid_f);
    let right_s = format!("{:.3}M", right_f);

    let mut labels_chars = vec![' '; w as usize];
    // Left label at 0
    for (idx, c) in left_s.chars().enumerate() {
        if idx < labels_chars.len() {
            labels_chars[idx] = c;
        }
    }
    // Mid label at center
    let mid_start = (w as usize / 2).saturating_sub(mid_s.len() / 2);
    for (idx, c) in mid_s.chars().enumerate() {
        let col = mid_start + idx;
        if col < labels_chars.len() {
            labels_chars[col] = c;
        }
    }
    // Right label at end
    let right_start = w as usize - right_s.len();
    for (idx, c) in right_s.chars().enumerate() {
        let col = right_start + idx;
        if col < labels_chars.len() {
            labels_chars[col] = c;
        }
    }

    let labels_str: String = labels_chars.into_iter().collect();
    let labels_line = Line::from(Span::styled(
        labels_str,
        Style::default().fg(Color::LightCyan),
    ));

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(area);

    f.render_widget(Paragraph::new(ticks_line), layout[0]);
    f.render_widget(Paragraph::new(labels_line), layout[1]);
}
