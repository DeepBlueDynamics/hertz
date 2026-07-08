use hertz_sdr::{enumerate, open_by_index, open_by_serial, Gain};
use num_complex::Complex;
use rustfft::FftPlanner;
use std::env;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut serial: Option<String> = None;
    let mut index: u32 = 0;
    let mut freq: u32 = 162_550_000; // default NOAA WX1
    let mut rate: u32 = 240_000; // default 240 kS/s

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--serial" if i + 1 < args.len() => {
                serial = Some(args[i + 1].clone());
                i += 2;
            }
            "--index" if i + 1 < args.len() => {
                index = args[i + 1].parse().unwrap_or(0);
                i += 2;
            }
            "--freq" if i + 1 < args.len() => {
                freq = args[i + 1].parse().unwrap_or(162_550_000);
                i += 2;
            }
            "--rate" if i + 1 < args.len() => {
                rate = args[i + 1].parse().unwrap_or(240_000);
                i += 2;
            }
            _ => {
                eprintln!("Unknown argument: {}", args[i]);
                eprintln!("Usage: dump_fft [--serial <serial>] [--index <index>] [--freq <hz>] [--rate <hz>]");
                std::process::exit(1);
            }
        }
    }

    println!("Enumerating devices...");
    match enumerate() {
        Ok(devs) => {
            println!("Found {} devices:", devs.len());
            for dev in devs {
                println!(
                    "  Index {}: Serial={}, Product={}",
                    dev.index, dev.serial, dev.product
                );
            }
        }
        Err(e) => {
            eprintln!("Failed to enumerate devices: {}", e);
        }
    }

    println!("Opening device (serial={:?}, index={})...", serial, index);
    let mut dev = if let Some(ref s) = serial {
        match open_by_serial(s) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Failed to open by serial {}: {}", s, e);
                return;
            }
        }
    } else {
        match open_by_index(index) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("Failed to open by index {}: {}", index, e);
                return;
            }
        }
    };

    println!("Configuring device: freq={} Hz, rate={} Hz...", freq, rate);
    dev.set_sample_rate(rate).expect("set_sample_rate failed");
    dev.set_center_freq(freq).expect("set_center_freq failed");
    dev.set_gain(Gain::Auto).expect("set_gain failed");
    dev.set_bandwidth(150_000).expect("set_bandwidth failed");
    dev.reset_buffer().expect("reset_buffer failed");

    println!("Starting spectrum dump. Press Ctrl+C to exit.");

    const N: usize = 1024;
    let mut buf = vec![0u8; N * 2];

    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(N);
    let mut fft_buf = vec![Complex::new(0.0, 0.0); N];

    let blocks = [" ", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

    loop {
        let start = Instant::now();

        match dev.read_sync(&mut buf) {
            Ok(bytes_read) => {
                if bytes_read < buf.len() {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }

                for (chunk, val) in buf.chunks_exact(2).zip(fft_buf.iter_mut()) {
                    let i_val = ((chunk[0] as f32) - 127.5) / 128.0;
                    let q_val = ((chunk[1] as f32) - 127.5) / 128.0;
                    *val = Complex::new(i_val, q_val);
                }

                fft.process(&mut fft_buf);

                let mut shifted_powers = vec![0.0; N];
                for (idx, val) in fft_buf.iter().enumerate() {
                    let shifted_idx = (idx + N / 2) % N;
                    let norm_val = val / (N as f32);
                    let power = 10.0 * (norm_val.norm_sqr() + 1e-10).log10();
                    shifted_powers[shifted_idx] = power;
                }

                let mut cols = [0.0f32; 60];
                for (c, col_val) in cols.iter_mut().enumerate() {
                    let start_bin = c * N / 60;
                    let end_bin = (c + 1) * N / 60;
                    let mut max_p = -200.0f32;
                    for &power in shifted_powers.iter().take(end_bin).skip(start_bin) {
                        if power > max_p {
                            max_p = power;
                        }
                    }
                    *col_val = max_p;
                }

                let min_db = -100.0f32;
                let max_db = 0.0f32;

                let mut line = String::new();
                for &val in &cols {
                    let val_norm = ((val - min_db) / (max_db - min_db)).clamp(0.0, 1.0);
                    let block_idx = (val_norm * 7.9) as usize;
                    line.push_str(blocks[block_idx]);
                }

                println!("{}", line);
            }
            Err(e) => {
                eprintln!("Read error: {}", e);
                std::thread::sleep(Duration::from_millis(500));
            }
        }

        let elapsed = start.elapsed();
        if elapsed < Duration::from_millis(500) {
            std::thread::sleep(Duration::from_millis(500) - elapsed);
        }
    }
}
