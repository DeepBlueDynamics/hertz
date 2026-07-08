use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::Receiver;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const MONITOR_VOLUME: f32 = 0.035;

pub fn spawn_audio_thread(
    rx: Receiver<Vec<f32>>,
    running: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let host = cpal::default_host();
        let device = match host.default_output_device() {
            Some(device) => device,
            None => return,
        };
        let config_range = match device.default_output_config() {
            Ok(cfg) => cfg,
            Err(_) => return,
        };
        let config = cpal::StreamConfig {
            channels: config_range.channels(),
            sample_rate: config_range.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };

        let channels = config.channels as usize;
        let mut audio_buf = Vec::new();

        let stream = match device.build_output_stream(
            &config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let frames = data.len() / channels;
                let mut written = 0;

                while written < frames {
                    if audio_buf.is_empty() {
                        match rx.try_recv() {
                            Ok(chunk) => audio_buf = chunk,
                            Err(_) => {
                                for sample in &mut data[written * channels..] {
                                    *sample = 0.0;
                                }
                                return;
                            }
                        }
                    }

                    let to_write = (frames - written).min(audio_buf.len());
                    for i in 0..to_write {
                        let sample = audio_buf[i] * MONITOR_VOLUME;
                        for ch in 0..channels {
                            data[(written + i) * channels + ch] = sample;
                        }
                    }
                    written += to_write;
                    audio_buf.drain(..to_write);
                }
            },
            |err| eprintln!("Audio error: {}", err),
            None,
        ) {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("Failed to build audio stream: {}", err);
                return;
            }
        };

        if let Err(err) = stream.play() {
            eprintln!("Failed to start audio stream: {}", err);
            return;
        }

        while running.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(100));
        }

        drop(stream);
    })
}
