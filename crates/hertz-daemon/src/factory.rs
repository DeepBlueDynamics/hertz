//! SDR factory seam: lets the daemon open a real RTL-SDR by serial in production and
//! a [`MockSdr`] in tests, without the dongle manager knowing which.

use std::sync::Arc;

use hertz_sdr::{MockSdr, SdrConfig, SdrDevice, SdrError, SdrWorker, WorkerHandle};

/// Constructs the [`hertz_sdr::SdrDevice`] + spawns the [`SdrWorker`] for a dongle.
/// Implemented by [`RealFactory`] (production) and [`MockFactory`] (tests).
pub trait SdrFactory: Send + Sync {
    /// Open the device for `serial` and spawn its worker thread with `config`.
    fn spawn_worker(&self, serial: &str, config: SdrConfig) -> Result<WorkerHandle, SdrError>;
}

/// Production factory: opens a real RTL-SDR by serial via librtlsdr-rs.
pub struct RealFactory;

impl SdrFactory for RealFactory {
    fn spawn_worker(&self, serial: &str, config: SdrConfig) -> Result<WorkerHandle, SdrError> {
        let device: Box<dyn SdrDevice> = hertz_sdr::open_by_serial(serial)?;
        Ok(SdrWorker::spawn(device, serial.to_string(), config))
    }
}

/// Test factory: ignores `serial` and spawns a worker over a [`MockSdr`] built from a
/// closure. The closure produces raw u8 IQ byte pairs from a sample index — tests use
/// it to synthesize NFM voice bursts.
pub struct MockFactory {
    builder: Box<dyn Fn() -> Box<dyn SdrDevice> + Send + Sync>,
}

impl MockFactory {
    pub fn new<F>(builder: F) -> Self
    where
        F: Fn() -> Box<dyn SdrDevice> + Send + Sync + 'static,
    {
        Self {
            builder: Box::new(builder),
        }
    }

    /// Build from a sample-producing closure `(sample_index) -> (i_u8, q_u8)`. The
    /// closure is shared (`Arc`) so the `Fn` builder can spawn many workers from it.
    pub fn from_closure<F>(f: F) -> Self
    where
        F: Fn(u64) -> (u8, u8) + Send + Sync + 'static,
    {
        let f = Arc::new(f);
        Self::new(move || {
            let f = Arc::clone(&f);
            Box::new(MockSdr::new_with_closure(move |i| f(i)))
        })
    }
}

impl SdrFactory for MockFactory {
    fn spawn_worker(&self, _serial: &str, config: SdrConfig) -> Result<WorkerHandle, SdrError> {
        let device = (self.builder)();
        Ok(SdrWorker::spawn(device, "MOCK".to_string(), config))
    }
}
