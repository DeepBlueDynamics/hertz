//! Per-dongle control state shared between REST handlers (writers) and the DSP
//! thread (reader). Mutations are infrequent (human / API driven); the DSP thread
//! snapshots the locked state each frame.

use std::sync::{Arc, Mutex};

/// Tunable/runtime state for one dongle. Cloned cheaply (it's an `Arc<Mutex<…>>`).
#[derive(Clone)]
pub struct DongleControl(Arc<Mutex<DongleControlInner>>);

#[derive(Debug)]
struct DongleControlInner {
    pub dongle_id: String,
    pub serial: String,
    pub role: hertz_types::DongleRole,
    pub bandplan: Option<String>,
    pub tap_channel: Option<String>,
    pub groups: Vec<String>,
    pub freq_hz: u64,
    pub squelch_db: f32,
    pub recording: bool,
    pub listening: bool,
    /// Monotonically increases when `freq_hz` changes; DSP thread uses it to detect
    /// retunes without holding the lock while rebuilding its pipeline.
    pub freq_epoch: u64,
    pub squelch_epoch: u64,
    pub online: bool,
    pub message: Option<String>,
}

impl DongleControl {
    pub fn new(
        dongle_id: String,
        serial: String,
        role: hertz_types::DongleRole,
        bandplan: Option<String>,
        tap_channel: Option<String>,
        groups: Vec<String>,
        freq_hz: u64,
        squelch_db: f32,
        recording: bool,
        listening: bool,
    ) -> Self {
        Self(Arc::new(Mutex::new(DongleControlInner {
            dongle_id,
            serial,
            role,
            bandplan,
            tap_channel,
            groups,
            freq_hz,
            squelch_db,
            recording,
            listening,
            freq_epoch: 0,
            squelch_epoch: 0,
            online: true,
            message: None,
        })))
    }

    pub fn snapshot(&self) -> DongleControlSnapshot {
        let g = self.0.lock().unwrap();
        DongleControlSnapshot {
            dongle_id: g.dongle_id.clone(),
            serial: g.serial.clone(),
            role: g.role,
            bandplan: g.bandplan.clone(),
            tap_channel: g.tap_channel.clone(),
            groups: g.groups.clone(),
            freq_hz: g.freq_hz,
            squelch_db: g.squelch_db,
            recording: g.recording,
            listening: g.listening,
            freq_epoch: g.freq_epoch,
            squelch_epoch: g.squelch_epoch,
            online: g.online,
            message: g.message.clone(),
        }
    }

    pub fn set_freq(&self, hz: u64) {
        let mut g = self.0.lock().unwrap();
        if g.freq_hz != hz {
            g.freq_hz = hz;
            g.freq_epoch = g.freq_epoch.wrapping_add(1);
        }
    }

    pub fn set_squelch(&self, db: f32) {
        let mut g = self.0.lock().unwrap();
        g.squelch_db = db.clamp(0.0, 30.0);
        g.squelch_epoch = g.squelch_epoch.wrapping_add(1);
    }

    pub fn set_recording(&self, on: bool) {
        self.0.lock().unwrap().recording = on;
    }

    pub fn set_listening(&self, on: bool) {
        self.0.lock().unwrap().listening = on;
    }

    pub fn set_online(&self, online: bool, message: Option<String>) {
        let mut g = self.0.lock().unwrap();
        g.online = online;
        g.message = message;
    }

    pub fn recording(&self) -> bool {
        self.0.lock().unwrap().recording
    }
}

/// An owned snapshot of [`DongleControl`] — cheap to hold without keeping the lock.
#[derive(Clone, Debug)]
pub struct DongleControlSnapshot {
    pub dongle_id: String,
    pub serial: String,
    pub role: hertz_types::DongleRole,
    pub bandplan: Option<String>,
    pub tap_channel: Option<String>,
    pub groups: Vec<String>,
    pub freq_hz: u64,
    pub squelch_db: f32,
    pub recording: bool,
    pub listening: bool,
    pub freq_epoch: u64,
    pub squelch_epoch: u64,
    pub online: bool,
    pub message: Option<String>,
}

impl DongleControlSnapshot {
    pub fn to_summary(&self) -> hertz_types::DongleSummary {
        hertz_types::DongleSummary {
            id: self.dongle_id.clone(),
            serial: self.serial.clone(),
            role: self.role,
            online: self.online,
            bandplan: self.bandplan.clone(),
            tap_channel: self.tap_channel.clone(),
            groups: self.groups.clone(),
            freq_hz: self.freq_hz,
            squelch_db: self.squelch_db,
            recording: self.recording,
            listening: self.listening,
            message: self.message.clone(),
        }
    }
}

/// Handle held by the runtime for each dongle: its control state plus a handle to
/// the SDR worker (for retune/gain/shutdown) and a join handle for the DSP thread.
pub struct DongleRuntimeHandle {
    pub control: DongleControl,
    pub worker: std::sync::Arc<hertz_sdr::WorkerHandle>,
    /// Optional: shutdown signal the DSP thread watches (set on graceful stop).
    pub shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
