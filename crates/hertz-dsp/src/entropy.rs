//! RF-noise entropy pool — ported from `create_entropy_pool` / `harvest_entropy` in
//! `plan/reference/gnosis-radio/src/pipeline/state.rs`.
//!
//! ADC quantization noise makes the least significant bits of demodulated audio
//! genuinely random. The pool is filled every frame and drained elsewhere (in gnosis
//! this was an HTTP `/api/entropy` endpoint; here it's an owned struct the daemon
//! wraps as it sees fit). Capped at 4 KiB.

/// Pool capacity (bytes). Carried from gnosis.
pub const POOL_CAPACITY: usize = 4096;

/// Harvest stride and per-frame budget. Carried from gnosis.
const HARVEST_STRIDE: usize = 37;
const HARVEST_BYTES_PER_FRAME: u32 = 32;

pub struct EntropyPool {
    buf: Vec<u8>,
}

impl Default for EntropyPool {
    fn default() -> Self {
        Self::new()
    }
}

impl EntropyPool {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(POOL_CAPACITY),
        }
    }

    /// Current number of bytes available.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether the pool is empty.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Harvest entropy from audio samples: take the LSB of each float's bit pattern,
    /// packing 8 at a time, sampling every 37rd sample (gnosis's stride), up to 32
    /// bytes per frame. When full, the oldest bytes rotate out to keep it fresh.
    /// [`POOL_CAPACITY`] is enforced as a hard cap after every call (gnosis relied on
    /// frequent HTTP draining; this library guarantees the cap regardless).
    pub fn harvest(&mut self, audio: &[f32]) {
        if self.buf.len() >= POOL_CAPACITY {
            let n = HARVEST_BYTES_PER_FRAME.min(self.buf.len() as u32) as usize;
            self.buf.drain(..n);
        }
        let mut byte: u8 = 0;
        let mut bit_count: u32 = 0;
        let mut collected: u32 = 0;
        for &sample in audio.iter().step_by(HARVEST_STRIDE) {
            let bits = sample.to_bits();
            byte = (byte << 1) | (bits as u8 & 1);
            bit_count += 1;
            if bit_count == 8 {
                self.buf.push(byte);
                byte = 0;
                bit_count = 0;
                collected += 1;
                if collected >= HARVEST_BYTES_PER_FRAME {
                    break;
                }
            }
        }
        if self.buf.len() > POOL_CAPACITY {
            let excess = self.buf.len() - POOL_CAPACITY;
            self.buf.drain(..excess);
        }
    }

    /// Drain up to `max_bytes` bytes from the pool (oldest first).
    pub fn drain(&mut self, max_bytes: usize) -> Vec<u8> {
        let n = max_bytes.min(self.buf.len());
        self.buf.drain(..n).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_and_drains() {
        let mut pool = EntropyPool::new();
        // White-noise-ish audio: lots of LSB entropy.
        let audio: Vec<f32> = (0..4096)
            .map(|i| ((i as u64 * 2654435761_u64) as i32 as f32) / 1e9)
            .collect();
        for _ in 0..5 {
            pool.harvest(&audio);
        }
        assert!(!pool.is_empty(), "pool should have filled");
        let drained = pool.drain(64);
        assert_eq!(drained.len(), 64.min(pool.len() + drained.len()));
        assert!(drained.iter().any(|&b| b != 0));
    }
}
