/// Terminal User Interface Module
/// Provides real-time spectrum display with interactive controls
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Sparkline},
    Frame, Terminal,
};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// UI State shared between monitor thread and TUI
#[allow(dead_code)]
pub struct UiState {
    pub spectrum_freqs: Vec<f32>,
    pub spectrum_mags: Vec<f32>,
    pub signal_db: f32,
    pub noise_floor: f32,
    pub squelch_threshold: f32,
    pub squelch_open: bool,
    pub hang_counter: u64,
    pub channel: Option<u8>,
    pub frequency_hz: u32,
    pub recording: bool,
    pub listen: bool,
    pub log_messages: Vec<String>,
}

impl UiState {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            spectrum_freqs: Vec::new(),
            spectrum_mags: Vec::new(),
            signal_db: 0.0,
            noise_floor: -100.0,
            squelch_threshold: 0.0,
            squelch_open: false,
            hang_counter: 0,
            channel: None,
            frequency_hz: 0,
            recording: false,
            listen: true,
            log_messages: Vec::new(),
        }
    }

    #[allow(dead_code)]
    pub fn add_log(&mut self, message: String) {
        self.log_messages.push(message);
        // Keep only last 100 messages
        if self.log_messages.len() > 100 {
            self.log_messages.remove(0);
        }
    }
}

/// Interactive controls
#[allow(dead_code)]
pub struct TuiControls {
    pub squelch_adjust: f32, // User adjustment to squelch
    pub zoom_level: usize,   // Spectrum zoom (1-10)
    pub update_rate_ms: u64, // FFT update interval
    pub quit: bool,
}

impl TuiControls {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            squelch_adjust: 0.0,
            zoom_level: 1,
            update_rate_ms: 200, // 5 Hz default
            quit: false,
        }
    }
}

/// Run the TUI
#[allow(dead_code)]
pub fn run_tui(ui_state: Arc<Mutex<UiState>>, controls: Arc<Mutex<TuiControls>>) -> io::Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = run_app(&mut terminal, ui_state, controls);

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        println!("TUI Error: {:?}", err);
    }

    Ok(())
}

#[allow(dead_code)]
fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ui_state: Arc<Mutex<UiState>>,
    controls: Arc<Mutex<TuiControls>>,
) -> io::Result<()> {
    let mut last_tick = Instant::now();
    let tick_rate = Duration::from_millis(100); // 10 Hz UI refresh

    loop {
        // Check if quit requested
        {
            let ctrl = controls.lock().unwrap();
            if ctrl.quit {
                break;
            }
        }

        terminal.draw(|f| ui(f, &ui_state, &controls))?;

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    handle_key(key.code, &controls, &ui_state);
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            last_tick = Instant::now();
        }
    }

    Ok(())
}

#[allow(dead_code)]
fn handle_key(key: KeyCode, controls: &Arc<Mutex<TuiControls>>, _ui_state: &Arc<Mutex<UiState>>) {
    let mut ctrl = controls.lock().unwrap();

    match key {
        KeyCode::Char('q') | KeyCode::Esc => {
            ctrl.quit = true;
        }
        KeyCode::Up => {
            ctrl.squelch_adjust += 0.5;
        }
        KeyCode::Down => {
            ctrl.squelch_adjust -= 0.5;
        }
        KeyCode::Char('+') | KeyCode::Char('=') => {
            ctrl.zoom_level = (ctrl.zoom_level + 1).min(10);
        }
        KeyCode::Char('-') | KeyCode::Char('_') => {
            ctrl.zoom_level = (ctrl.zoom_level.saturating_sub(1)).max(1);
        }
        KeyCode::Left => {
            ctrl.update_rate_ms = (ctrl.update_rate_ms + 50).min(2000);
        }
        KeyCode::Right => {
            ctrl.update_rate_ms = (ctrl.update_rate_ms.saturating_sub(50)).max(50);
        }
        _ => {}
    }
}

#[allow(dead_code)]
fn ui(f: &mut Frame, ui_state: &Arc<Mutex<UiState>>, controls: &Arc<Mutex<TuiControls>>) {
    let state = ui_state.lock().unwrap();
    let ctrl = controls.lock().unwrap();

    // Main layout: [Spectrum][Status][Logs]
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(12), // Spectrum
            Constraint::Length(3),  // Status
            Constraint::Min(5),     // Logs
        ])
        .split(f.area());

    // Spectrum display
    render_spectrum(f, chunks[0], &state);

    // Status bar
    render_status(f, chunks[1], &state, &ctrl);

    // Log panel
    render_logs(f, chunks[2], &state);
}

#[allow(dead_code)]
fn render_spectrum(f: &mut Frame, area: Rect, state: &UiState) {
    let title = if let Some(ch) = state.channel {
        format!(
            " RF Spectrum - Ch {} ({:.3} MHz) ",
            ch,
            state.frequency_hz as f64 / 1e6
        )
    } else {
        format!(" RF Spectrum ({:.3} MHz) ", state.frequency_hz as f64 / 1e6)
    };

    // Convert magnitudes to sparkline data (u64)
    let sparkline_data: Vec<u64> = if state.spectrum_mags.is_empty() {
        vec![0; 80]
    } else {
        // Downsample to fit width
        let width = area.width.saturating_sub(4) as usize;
        let step = (state.spectrum_mags.len() as f32 / width as f32).ceil() as usize;

        state
            .spectrum_mags
            .chunks(step.max(1))
            .map(|chunk| {
                let avg = chunk.iter().sum::<f32>() / chunk.len() as f32;
                // Convert dB to positive value (shift by 100)
                ((avg + 100.0).max(0.0) * 10.0) as u64
            })
            .collect()
    };

    let sparkline = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .data(&sparkline_data)
        .style(Style::default().fg(Color::Yellow))
        .max(1000); // Max value for scaling

    f.render_widget(sparkline, area);
}

#[allow(dead_code)]
fn render_status(f: &mut Frame, area: Rect, state: &UiState, ctrl: &TuiControls) {
    let squelch_status = if state.squelch_open {
        Span::styled(
            "OPEN",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("CLOSED", Style::default().fg(Color::Red))
    };

    let rec_status = if state.recording {
        Span::styled(
            "REC",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("---", Style::default().fg(Color::DarkGray))
    };

    let status_text = Line::from(vec![
        Span::raw(format!("Signal: {:>5.1}dB | ", state.signal_db)),
        Span::raw(format!("Noise: {:>5.1}dB | ", state.noise_floor)),
        Span::raw(format!("Thresh: {:>5.1}dB (", state.squelch_threshold)),
        Span::styled(
            format!("{:+.1}", ctrl.squelch_adjust),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw(") | Squelch: "),
        squelch_status,
        Span::raw(format!(" [{}] | ", state.hang_counter)),
        rec_status,
        Span::raw(format!(" | Zoom: {}x", ctrl.zoom_level)),
    ]);

    let paragraph = Paragraph::new(status_text).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Status ")
            .border_style(Style::default().fg(Color::White)),
    );

    f.render_widget(paragraph, area);
}

#[allow(dead_code)]
fn render_logs(f: &mut Frame, area: Rect, state: &UiState) {
    let log_items: Vec<ListItem> = state
        .log_messages
        .iter()
        .rev()
        .take(area.height.saturating_sub(2) as usize)
        .rev()
        .map(|msg| ListItem::new(msg.clone()))
        .collect();

    let logs = List::new(log_items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Event Log (↑/↓: Squelch | ←/→: Update Rate | +/-: Zoom | q: Quit) ")
            .border_style(Style::default().fg(Color::White)),
    );

    f.render_widget(logs, area);
}
