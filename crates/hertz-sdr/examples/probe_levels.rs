//! Diagnostic: raw IQ level stats on one frequency under several tuner setups.
use hertz_sdr::{open_by_index, Gain, SdrDevice};

fn stats(dev: &mut Box<dyn SdrDevice>) -> String {
    let mut buf = vec![0u8; 240_000 * 2];
    let _ = dev.reset_buffer();
    // discard first read (settling)
    let _ = dev.read_sync(&mut buf);
    let mut pw = Vec::new();
    for _ in 0..5 {
        let n = dev.read_sync(&mut buf).unwrap_or(0);
        let mut p = 0f64;
        for c in buf[..n].chunks_exact(2) {
            let i = (c[0] as f64 - 127.5) / 127.5;
            let q = (c[1] as f64 - 127.5) / 127.5;
            p += i * i + q * q;
        }
        pw.push(10.0 * (p / (n as f64 / 2.0) + 1e-12).log10());
    }
    let max = buf.iter().map(|&b| (b as i32 - 127).abs()).max().unwrap();
    format!(
        "power dB per 1s: {:?}  peak |dev| {}",
        pw.iter()
            .map(|x| (x * 10.0).round() / 10.0)
            .collect::<Vec<_>>(),
        max
    )
}

fn main() {
    let f = 156_625_000;
    let mut dev = open_by_index(0).expect("open");
    let cases: Vec<(&str, Gain, Option<u32>, bool)> = vec![
        (
            "vhf_monitor order: rate,freq,gain49.6 (no bw)",
            Gain::Db(49.6),
            None,
            false,
        ),
        (
            "hertz order: rate,freq,gain49.6,bw150k",
            Gain::Db(49.6),
            Some(150_000),
            false,
        ),
        (
            "bw150k then re-set freq",
            Gain::Db(49.6),
            Some(150_000),
            true,
        ),
        ("auto gain, bw150k", Gain::Auto, Some(150_000), false),
        ("auto gain, no bw", Gain::Auto, None, false),
    ];
    for (name, gain, bw, refreq) in cases {
        dev.set_sample_rate(240_000).unwrap();
        dev.set_center_freq(f).unwrap();
        let g = dev.set_gain(gain);
        let b = bw.map(|b| dev.set_bandwidth(b));
        if refreq {
            dev.set_center_freq(f + 1000).unwrap();
            dev.set_center_freq(f).unwrap();
        }
        println!(
            "{name}: gain={:?} bw={:?}\n   {}",
            g.is_ok(),
            b.map(|r| r.is_ok()),
            stats(&mut dev)
        );
    }
}
