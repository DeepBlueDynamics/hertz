use crossbeam_channel::{Receiver, Sender};
use rtrb::RingBuffer;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::SystemTime;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SdrError {
    #[error("Device not found: {0}")]
    NotFound(String),
    #[error("Device busy or already in use: {0}")]
    Busy(String),
    #[error("USB error: {0}")]
    Usb(String),
    #[error("Unsupported operation: {0}")]
    Unsupported(String),
    #[error("Other error: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, SdrError>;

impl From<rtl_sdr_rs::error::RtlsdrError> for SdrError {
    fn from(err: rtl_sdr_rs::error::RtlsdrError) -> Self {
        match err {
            rtl_sdr_rs::error::RtlsdrError::Usb(usb_err) => match usb_err {
                rusb::Error::NoDevice | rusb::Error::NotFound => {
                    SdrError::NotFound(usb_err.to_string())
                }
                rusb::Error::Busy => SdrError::Busy(usb_err.to_string()),
                rusb::Error::Access => SdrError::Busy(format!("Permission denied: {}", usb_err)),
                _ => SdrError::Usb(usb_err.to_string()),
            },
            rtl_sdr_rs::error::RtlsdrError::RtlsdrErr(msg) => {
                if msg.contains("not found") || msg.contains("No RTL-SDR devices") {
                    SdrError::NotFound(msg)
                } else if msg.contains("busy") || msg.contains("already in use") {
                    SdrError::Busy(msg)
                } else {
                    SdrError::Other(msg)
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Gain {
    Auto,
    Db(f32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerEvent {
    DeviceLost,
    DeviceReconnected,
}

#[derive(Clone, Debug)]
pub struct WorkerStats {
    pub dropped_samples: usize,
    pub read_errors: usize,
    pub last_read_time: Option<SystemTime>,
    pub epoch: u64,
}

#[derive(Clone, Debug)]
pub struct SdrConfig {
    pub sample_rate: u32,
    pub center_freq: u32,
    pub gain: Gain,
    pub bandwidth: u32,
    pub buffer_size_iq_pairs: usize,
    pub ring_capacity_bytes: usize,
}

impl Default for SdrConfig {
    fn default() -> Self {
        Self {
            sample_rate: 240_000,
            center_freq: 156_800_000,
            gain: Gain::Auto,
            bandwidth: 150_000,
            buffer_size_iq_pairs: 48_000,
            ring_capacity_bytes: 2 * 240_000 * 2, // ~2 seconds of samples
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DongleInfo {
    pub index: u32,
    pub serial: String,
    pub product: String,
}

pub trait SdrDevice: Send {
    fn set_sample_rate(&mut self, hz: u32) -> Result<()>;
    fn set_center_freq(&mut self, hz: u32) -> Result<()>;
    fn set_gain(&mut self, gain: Gain) -> Result<()>;
    fn set_bandwidth(&mut self, hz: u32) -> Result<()>;
    fn reset_buffer(&mut self) -> Result<()>;
    fn read_sync(&mut self, buf: &mut [u8]) -> Result<usize>;
}

pub struct RealSdrDevice {
    device: rtl_sdr_rs::RtlSdr,
}

impl SdrDevice for RealSdrDevice {
    fn set_sample_rate(&mut self, hz: u32) -> Result<()> {
        self.device.set_sample_rate(hz).map_err(SdrError::from)
    }

    fn set_center_freq(&mut self, hz: u32) -> Result<()> {
        self.device.set_center_freq(hz).map_err(SdrError::from)
    }

    fn set_gain(&mut self, gain: Gain) -> Result<()> {
        let tuner_gain = match gain {
            Gain::Auto => rtl_sdr_rs::TunerGain::Auto,
            Gain::Db(db) => rtl_sdr_rs::TunerGain::Manual((db * 10.0) as i32),
        };
        self.device
            .set_tuner_gain(tuner_gain)
            .map_err(SdrError::from)
    }

    fn set_bandwidth(&mut self, hz: u32) -> Result<()> {
        self.device.set_tuner_bandwidth(hz).map_err(SdrError::from)
    }

    fn reset_buffer(&mut self) -> Result<()> {
        self.device.reset_buffer().map_err(SdrError::from)
    }

    fn read_sync(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.device.read_sync(buf).map_err(SdrError::from)
    }
}

pub fn enumerate() -> Result<Vec<DongleInfo>> {
    let devices = rtl_sdr_rs::RtlSdr::list_devices().map_err(SdrError::from)?;
    let mut info_list = Vec::new();
    for dev in devices {
        info_list.push(DongleInfo {
            index: dev.index as u32,
            serial: dev.serial,
            product: dev.product,
        });
    }
    Ok(info_list)
}

pub fn open_by_serial(serial: &str) -> Result<Box<dyn SdrDevice>> {
    let sdr = rtl_sdr_rs::RtlSdr::open_with_serial(serial).map_err(SdrError::from)?;
    Ok(Box::new(RealSdrDevice { device: sdr }))
}

pub fn open_by_index(index: u32) -> Result<Box<dyn SdrDevice>> {
    let sdr = rtl_sdr_rs::RtlSdr::open_with_index(index as usize).map_err(SdrError::from)?;
    Ok(Box::new(RealSdrDevice { device: sdr }))
}

// --- MockSdr implementation ---

pub enum MockSource {
    Closure(Box<dyn Fn(u64) -> (u8, u8) + Send + Sync>),
    File {
        path: std::path::PathBuf,
        file: Option<std::fs::File>,
    },
    Constant(u8, u8),
}

pub struct MockSdr {
    pub source: MockSource,
    pub sample_index: u64,
    pub error_injector: Option<Box<dyn Fn(u64) -> Option<SdrError> + Send + Sync>>,
    pub sample_rate: u32,
    pub center_freq: u32,
    pub gain: Gain,
    pub bandwidth: u32,
}

impl MockSdr {
    pub fn new_with_closure<F>(f: F) -> Self
    where
        F: Fn(u64) -> (u8, u8) + Send + Sync + 'static,
    {
        Self {
            source: MockSource::Closure(Box::new(f)),
            sample_index: 0,
            error_injector: None,
            sample_rate: 240_000,
            center_freq: 156_800_000,
            gain: Gain::Auto,
            bandwidth: 150_000,
        }
    }

    pub fn new_with_file<P: Into<std::path::PathBuf>>(path: P) -> Self {
        Self {
            source: MockSource::File {
                path: path.into(),
                file: None,
            },
            sample_index: 0,
            error_injector: None,
            sample_rate: 240_000,
            center_freq: 156_800_000,
            gain: Gain::Auto,
            bandwidth: 150_000,
        }
    }

    pub fn new_constant(i: u8, q: u8) -> Self {
        Self {
            source: MockSource::Constant(i, q),
            sample_index: 0,
            error_injector: None,
            sample_rate: 240_000,
            center_freq: 156_800_000,
            gain: Gain::Auto,
            bandwidth: 150_000,
        }
    }

    pub fn set_error_injector<F>(&mut self, f: F)
    where
        F: Fn(u64) -> Option<SdrError> + Send + Sync + 'static,
    {
        self.error_injector = Some(Box::new(f));
    }
}

impl SdrDevice for MockSdr {
    fn set_sample_rate(&mut self, hz: u32) -> Result<()> {
        self.sample_rate = hz;
        Ok(())
    }

    fn set_center_freq(&mut self, hz: u32) -> Result<()> {
        self.center_freq = hz;
        Ok(())
    }

    fn set_gain(&mut self, gain: Gain) -> Result<()> {
        self.gain = gain;
        Ok(())
    }

    fn set_bandwidth(&mut self, hz: u32) -> Result<()> {
        self.bandwidth = hz;
        Ok(())
    }

    fn reset_buffer(&mut self) -> Result<()> {
        Ok(())
    }

    fn read_sync(&mut self, buf: &mut [u8]) -> Result<usize> {
        if let Some(ref injector) = self.error_injector {
            if let Some(err) = injector(self.sample_index) {
                return Err(err);
            }
        }

        let num_pairs = buf.len() / 2;
        for i in 0..num_pairs {
            let (i_val, q_val) = match &mut self.source {
                MockSource::Closure(f) => f(self.sample_index),
                MockSource::Constant(i_val, q_val) => (*i_val, *q_val),
                MockSource::File { path, file } => {
                    let f = match file {
                        Some(f) => f,
                        None => {
                            let open_file = std::fs::File::open(path).map_err(|e| {
                                SdrError::Other(format!("Failed to open raw IQ file: {}", e))
                            })?;
                            *file = Some(open_file);
                            file.as_mut().unwrap()
                        }
                    };
                    use std::io::Read;
                    let mut iq_bytes = [0u8; 2];
                    match f.read_exact(&mut iq_bytes) {
                        Ok(()) => (iq_bytes[0], iq_bytes[1]),
                        Err(ref e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                            use std::io::Seek;
                            if f.seek(std::io::SeekFrom::Start(0)).is_ok()
                                && f.read_exact(&mut iq_bytes).is_ok()
                            {
                                (iq_bytes[0], iq_bytes[1])
                            } else {
                                return Err(SdrError::Other(
                                    "EOF reached and failed to loop raw IQ file".to_string(),
                                ));
                            }
                        }
                        Err(e) => {
                            return Err(SdrError::Other(format!(
                                "Failed to read raw IQ file: {}",
                                e
                            )))
                        }
                    }
                }
            };
            buf[2 * i] = i_val;
            buf[2 * i + 1] = q_val;
            self.sample_index += 1;
        }

        Ok(num_pairs * 2)
    }
}

// --- Worker & Handle ---

enum WorkerCmd {
    Retune(u32),
    SetGain(Gain),
    Shutdown,
}

pub struct SdrWorker;

struct WorkerStatsShared {
    dropped_samples: AtomicUsize,
    read_errors: AtomicUsize,
    last_read_nanos: AtomicU64,
    epoch: AtomicU64,
}

pub struct WorkerHandle {
    cmd_tx: Sender<WorkerCmd>,
    stats: Arc<WorkerStatsShared>,
    consumer: std::sync::Mutex<Option<rtrb::Consumer<u8>>>,
    event_rx: Receiver<WorkerEvent>,
}

impl WorkerHandle {
    pub fn stats(&self) -> WorkerStats {
        let nanos = self.stats.last_read_nanos.load(Ordering::Relaxed);
        let last_read_time = if nanos == 0 {
            None
        } else {
            Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_nanos(nanos))
        };
        WorkerStats {
            dropped_samples: self.stats.dropped_samples.load(Ordering::Relaxed),
            read_errors: self.stats.read_errors.load(Ordering::Relaxed),
            last_read_time,
            epoch: self.stats.epoch.load(Ordering::Relaxed),
        }
    }

    pub fn retune(&self, hz: u32) -> Result<()> {
        self.cmd_tx
            .send(WorkerCmd::Retune(hz))
            .map_err(|_| SdrError::Other("Worker thread is dead".to_string()))
    }

    pub fn set_gain(&self, gain: Gain) -> Result<()> {
        self.cmd_tx
            .send(WorkerCmd::SetGain(gain))
            .map_err(|_| SdrError::Other("Worker thread is dead".to_string()))
    }

    pub fn shutdown(&self) -> Result<()> {
        self.cmd_tx
            .send(WorkerCmd::Shutdown)
            .map_err(|_| SdrError::Other("Worker thread is dead".to_string()))
    }

    pub fn reader(&self) -> rtrb::Consumer<u8> {
        self.consumer
            .lock()
            .unwrap()
            .take()
            .expect("Reader already taken")
    }

    pub fn event_receiver(&self) -> &Receiver<WorkerEvent> {
        &self.event_rx
    }
}

impl SdrWorker {
    pub fn spawn(
        mut device: Box<dyn SdrDevice>,
        serial: String,
        config: SdrConfig,
    ) -> WorkerHandle {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<WorkerCmd>();
        let (event_tx, event_rx) = crossbeam_channel::unbounded::<WorkerEvent>();

        let (producer, consumer) = RingBuffer::<u8>::new(config.ring_capacity_bytes);

        let stats = Arc::new(WorkerStatsShared {
            dropped_samples: AtomicUsize::new(0),
            read_errors: AtomicUsize::new(0),
            last_read_nanos: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
        });

        let stats_clone = Arc::clone(&stats);

        thread::spawn(move || {
            let mut producer = producer;
            let mut current_freq = config.center_freq;
            let current_sample_rate = config.sample_rate;
            let mut current_gain = config.gain;
            let current_bandwidth = config.bandwidth;

            // Set initial parameters. Failures are logged, not fatal: a dongle that
            // rejects one setting can still stream, but silently ignoring it hid
            // mis-tuning and gain problems.
            let warn_err = |what: &str, r: Result<()>| {
                if let Err(e) = r {
                    log::warn!("sdr init: {what} failed: {e}");
                }
            };
            warn_err(
                "set_sample_rate",
                device.set_sample_rate(current_sample_rate),
            );
            warn_err("set_center_freq", device.set_center_freq(current_freq));
            warn_err("set_gain", device.set_gain(current_gain));
            warn_err("set_bandwidth", device.set_bandwidth(current_bandwidth));
            warn_err("reset_buffer", device.reset_buffer());

            let buffer_size = config.buffer_size_iq_pairs * 2;
            let mut read_buf = vec![0u8; buffer_size];
            let mut consecutive_errors = 0;

            loop {
                // Drain command channel
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        WorkerCmd::Shutdown => {
                            return;
                        }
                        WorkerCmd::Retune(hz) => {
                            current_freq = hz;
                            if let Err(e) = device.set_center_freq(hz) {
                                log::error!("Failed to retune to {} Hz: {}", hz, e);
                            }
                            let _ = device.reset_buffer();
                            stats_clone.epoch.fetch_add(1, Ordering::Relaxed);
                        }
                        WorkerCmd::SetGain(gain) => {
                            current_gain = gain;
                            if let Err(e) = device.set_gain(gain) {
                                log::error!("Failed to set gain: {}", e);
                            }
                        }
                    }
                }

                // Read from device
                match device.read_sync(&mut read_buf) {
                    Ok(n) => {
                        consecutive_errors = 0;
                        let now = SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos() as u64;
                        stats_clone.last_read_nanos.store(now, Ordering::Relaxed);

                        let (_pushed, remainder) = producer.push_partial_slice(&read_buf[..n]);
                        if !remainder.is_empty() {
                            let dropped = remainder.len() / 2;
                            stats_clone
                                .dropped_samples
                                .fetch_add(dropped, Ordering::Relaxed);
                        }
                    }
                    Err(_e) => {
                        stats_clone.read_errors.fetch_add(1, Ordering::Relaxed);
                        consecutive_errors += 1;

                        if consecutive_errors >= 3 {
                            let _ = event_tx.send(WorkerEvent::DeviceLost);
                            stats_clone.epoch.fetch_add(1, Ordering::Relaxed);

                            // Reconnection loop
                            loop {
                                thread::sleep(std::time::Duration::from_millis(500));

                                let mut shut_down = false;
                                while let Ok(cmd) = cmd_rx.try_recv() {
                                    if let WorkerCmd::Shutdown = cmd {
                                        shut_down = true;
                                        break;
                                    }
                                }
                                if shut_down {
                                    return;
                                }

                                match open_by_serial(&serial) {
                                    Ok(mut new_device) => {
                                        let _ = new_device.set_sample_rate(current_sample_rate);
                                        let _ = new_device.set_center_freq(current_freq);
                                        let _ = new_device.set_gain(current_gain);
                                        let _ = new_device.set_bandwidth(current_bandwidth);
                                        let _ = new_device.reset_buffer();

                                        device = new_device;
                                        consecutive_errors = 0;
                                        let _ = event_tx.send(WorkerEvent::DeviceReconnected);
                                        break;
                                    }
                                    Err(_) => {
                                        // Keep retrying
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });

        WorkerHandle {
            cmd_tx,
            stats,
            consumer: std::sync::Mutex::new(Some(consumer)),
            event_rx,
        }
    }
}

// --- Doctor functionality ---

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DeviceDoctorResult {
    pub index: u32,
    pub serial: String,
    pub product: String,
    pub open_ok: bool,
    pub configure_ok: bool,
    pub read_ok: bool,
    pub dc_offset_i: f32,
    pub dc_offset_q: f32,
    pub samples_read: usize,
    pub error_message: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DoctorReport {
    pub devices: Vec<DeviceDoctorResult>,
    pub serial_warnings: Vec<String>,
    pub total_devices_found: usize,
}

pub fn doctor() -> DoctorReport {
    doctor_impl(enumerate, open_by_index)
}

pub fn doctor_impl<E, O>(enumerate_fn: E, open_fn: O) -> DoctorReport
where
    E: Fn() -> Result<Vec<DongleInfo>>,
    O: Fn(u32) -> Result<Box<dyn SdrDevice>>,
{
    let sdr_list = match enumerate_fn() {
        Ok(l) => l,
        Err(e) => {
            return DoctorReport {
                devices: Vec::new(),
                serial_warnings: vec![format!("Failed to enumerate devices: {}", e)],
                total_devices_found: 0,
            };
        }
    };

    let mut devices_report = Vec::new();
    let mut serial_counts = std::collections::HashMap::new();

    for info in &sdr_list {
        if !info.serial.is_empty() {
            *serial_counts.entry(info.serial.clone()).or_insert(0) += 1;
        }

        let mut result = DeviceDoctorResult {
            index: info.index,
            serial: info.serial.clone(),
            product: info.product.clone(),
            open_ok: false,
            configure_ok: false,
            read_ok: false,
            dc_offset_i: 0.0,
            dc_offset_q: 0.0,
            samples_read: 0,
            error_message: None,
        };

        match open_fn(info.index) {
            Ok(mut device) => {
                result.open_ok = true;
                if device.set_sample_rate(240_000).is_ok()
                    && device.set_gain(Gain::Auto).is_ok()
                    && device.set_bandwidth(150_000).is_ok()
                {
                    result.configure_ok = true;

                    let mut buf = vec![0u8; 240_000];
                    let mut total_read = 0;
                    let mut read_failed = false;
                    let start = std::time::Instant::now();

                    while total_read < buf.len() && start.elapsed().as_secs_f32() < 1.0 {
                        match device.read_sync(&mut buf[total_read..]) {
                            Ok(n) => {
                                if n == 0 {
                                    break;
                                }
                                total_read += n;
                            }
                            Err(e) => {
                                result.error_message = Some(format!("Read error: {}", e));
                                read_failed = true;
                                break;
                            }
                        }
                    }

                    if !read_failed && total_read > 0 {
                        result.read_ok = true;
                        let num_samples = total_read / 2;
                        result.samples_read = num_samples;

                        let mut sum_i = 0.0;
                        let mut sum_q = 0.0;
                        for i in 0..num_samples {
                            sum_i += (buf[2 * i] as f32) - 127.5;
                            sum_q += (buf[2 * i + 1] as f32) - 127.5;
                        }
                        result.dc_offset_i = sum_i / (num_samples as f32);
                        result.dc_offset_q = sum_q / (num_samples as f32);
                    } else if total_read == 0 && !read_failed {
                        result.error_message = Some("No samples read".to_string());
                    }
                } else {
                    result.error_message = Some("Failed to configure device settings".to_string());
                }
            }
            Err(e) => {
                result.error_message = Some(format!("Failed to open device: {}", e));
            }
        }

        devices_report.push(result);
    }

    let mut serial_warnings = Vec::new();
    for (serial, count) in serial_counts {
        if count > 1 {
            serial_warnings.push(format!(
                "Duplicate serial '{}' detected ({} devices share it)",
                serial, count
            ));
        }
    }

    DoctorReport {
        devices: devices_report,
        serial_warnings,
        total_devices_found: sdr_list.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_worker_delivers_bytes() {
        let mock = Box::new(MockSdr::new_constant(42, 84));
        let config = SdrConfig {
            sample_rate: 1000,
            center_freq: 100_000_000,
            gain: Gain::Auto,
            bandwidth: 100_000,
            buffer_size_iq_pairs: 8,
            ring_capacity_bytes: 1024,
        };
        let handle = SdrWorker::spawn(mock, "MOCK01".to_string(), config);

        thread::sleep(Duration::from_millis(50));
        let mut reader = handle.reader();
        let mut read_buf = [0u8; 16];
        let (popped, _remainder) = reader.pop_partial_slice(&mut read_buf);
        assert_eq!(popped.len(), 16);
        for i in 0..8 {
            assert_eq!(read_buf[2 * i], 42);
            assert_eq!(read_buf[2 * i + 1], 84);
        }

        handle.shutdown().unwrap();
    }

    #[test]
    fn test_worker_retune_discontinuity() {
        let mock = Box::new(MockSdr::new_constant(0, 0));
        let config = SdrConfig {
            sample_rate: 1000,
            center_freq: 100_000_000,
            gain: Gain::Auto,
            bandwidth: 100_000,
            buffer_size_iq_pairs: 8,
            ring_capacity_bytes: 1024,
        };
        let handle = SdrWorker::spawn(mock, "MOCK01".to_string(), config);

        thread::sleep(Duration::from_millis(50));
        let stats_before = handle.stats();
        assert_eq!(stats_before.epoch, 0);

        handle.retune(156_900_000).unwrap();
        thread::sleep(Duration::from_millis(50));

        let stats_after = handle.stats();
        assert_eq!(stats_after.epoch, 1);

        handle.shutdown().unwrap();
    }

    #[test]
    fn test_worker_ring_full_drops() {
        let mock = Box::new(MockSdr::new_constant(0, 0));
        let config = SdrConfig {
            sample_rate: 10000,
            center_freq: 100_000_000,
            gain: Gain::Auto,
            bandwidth: 100_000,
            buffer_size_iq_pairs: 256,
            ring_capacity_bytes: 128,
        };
        let handle = SdrWorker::spawn(mock, "MOCK01".to_string(), config);

        thread::sleep(Duration::from_millis(150));

        let stats = handle.stats();
        assert!(
            stats.dropped_samples > 0,
            "Expected drops, got {}",
            stats.dropped_samples
        );

        handle.shutdown().unwrap();
    }

    #[test]
    fn test_worker_device_lost() {
        let mut mock = MockSdr::new_constant(0, 0);
        mock.set_error_injector(|_| Some(SdrError::Usb("USB connection reset".to_string())));

        let config = SdrConfig {
            sample_rate: 1000,
            center_freq: 100_000_000,
            gain: Gain::Auto,
            bandwidth: 100_000,
            buffer_size_iq_pairs: 8,
            ring_capacity_bytes: 1024,
        };
        let handle = SdrWorker::spawn(Box::new(mock), "MOCK01".to_string(), config);

        let event_rx = handle.event_receiver();
        let ev = event_rx
            .recv_timeout(Duration::from_millis(200))
            .expect("Expected DeviceLost event");
        assert_eq!(ev, WorkerEvent::DeviceLost);

        handle.shutdown().unwrap();
    }

    #[test]
    fn test_doctor_report_on_mock() {
        let mock_enumerate = || {
            Ok(vec![
                DongleInfo {
                    index: 0,
                    serial: "DUP01".to_string(),
                    product: "Mock1".to_string(),
                },
                DongleInfo {
                    index: 1,
                    serial: "DUP01".to_string(),
                    product: "Mock2".to_string(),
                },
            ])
        };

        let mock_open = |index: u32| -> Result<Box<dyn SdrDevice>> {
            if index == 0 {
                Ok(Box::new(MockSdr::new_constant(128 + 10, 128 - 20)))
            } else {
                Ok(Box::new(MockSdr::new_constant(128, 128)))
            }
        };

        let report = doctor_impl(mock_enumerate, mock_open);
        assert_eq!(report.total_devices_found, 2);
        assert_eq!(report.devices.len(), 2);

        let dev0 = &report.devices[0];
        assert!(dev0.open_ok);
        assert!(dev0.configure_ok);
        assert!(dev0.read_ok);
        assert!((dev0.dc_offset_i - 10.5).abs() < 0.01);
        assert!((dev0.dc_offset_q - (-19.5)).abs() < 0.01);

        assert_eq!(report.serial_warnings.len(), 1);
        assert!(report.serial_warnings[0].contains("Duplicate serial 'DUP01'"));
    }

    #[test]
    #[ignore]
    fn hw_enumerate() {
        let list = enumerate().unwrap();
        println!("Hardware devices found: {:?}", list);
    }

    #[test]
    #[ignore]
    fn hw_read() {
        let list = enumerate().unwrap();
        if list.is_empty() {
            println!("No hardware devices connected, skipping");
            return;
        }
        let mut dev = open_by_index(0).unwrap();
        dev.set_sample_rate(240_000).unwrap();
        dev.set_center_freq(162_550_000).unwrap();
        dev.set_gain(Gain::Auto).unwrap();
        dev.set_bandwidth(150_000).unwrap();

        let mut buf = vec![0u8; 1024];
        let n = dev.read_sync(&mut buf).unwrap();
        assert!(n > 0);
        println!("Successfully read {} bytes from real hardware!", n);
    }
}
