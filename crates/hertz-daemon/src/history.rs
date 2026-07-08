//! History accumulator (PLAN §5): ring buffers of recent activity + transcripts,
//! with JSONL append persistence under `data_dir/history/` so restarts don't amnesia
//! the day's log. Audio is never persisted here (only its recording files).

use std::path::PathBuf;
use std::sync::Mutex;

use hertz_types::wire::{ActivityEntry, TranscriptEntry};

const ACTIVITY_CAP: usize = 500;
const TRANSCRIPT_CAP: usize = 200;

pub struct History {
    activity: Mutex<Vec<ActivityEntry>>,
    transcripts: Mutex<Vec<TranscriptEntry>>,
    activity_log: Mutex<Option<std::fs::File>>,
    transcript_log: Mutex<Option<std::fs::File>>,
    dir: PathBuf,
}

impl History {
    /// Open (or create) the history store rooted at `data_dir/history/`. Existing
    /// JSONL files are reloaded into the rings on boot.
    pub fn open(data_dir: &std::path::Path) -> std::io::Result<Self> {
        let dir = data_dir.join("history");
        std::fs::create_dir_all(&dir)?;
        let activity_path = dir.join("activity.jsonl");
        let transcript_path = dir.join("transcripts.jsonl");

        let activity = load_jsonl::<ActivityEntry>(&activity_path);
        let transcripts = load_jsonl::<TranscriptEntry>(&transcript_path);

        let activity_log = Mutex::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&activity_path)
                .ok(),
        );
        let transcript_log = Mutex::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&transcript_path)
                .ok(),
        );

        Ok(Self {
            activity: Mutex::new(activity),
            transcripts: Mutex::new(transcripts),
            activity_log,
            transcript_log,
            dir,
        })
    }

    pub fn record_activity(&self, entry: ActivityEntry) {
        if let Ok(mut g) = self.activity.lock() {
            g.push(entry.clone());
            if g.len() > ACTIVITY_CAP {
                g.remove(0);
            }
        }
        if let Ok(mut f) = self.activity_log.lock() {
            use std::io::Write;
            if let Some(file) = f.as_mut() {
                let _ = writeln!(
                    file,
                    "{}",
                    serde_json::to_string(&entry).unwrap_or_default()
                );
                let _ = file.flush();
            }
        }
    }

    pub fn record_transcript(&self, entry: TranscriptEntry) {
        if let Ok(mut g) = self.transcripts.lock() {
            g.push(entry.clone());
            if g.len() > TRANSCRIPT_CAP {
                g.remove(0);
            }
        }
        if let Ok(mut f) = self.transcript_log.lock() {
            use std::io::Write;
            if let Some(file) = f.as_mut() {
                let _ = writeln!(
                    file,
                    "{}",
                    serde_json::to_string(&entry).unwrap_or_default()
                );
                let _ = file.flush();
            }
        }
    }

    pub fn activity(&self) -> Vec<ActivityEntry> {
        self.activity.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn transcripts(&self) -> Vec<TranscriptEntry> {
        self.transcripts
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }
}

fn load_jsonl<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Vec<T> {
    let mut out = Vec::new();
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            if let Ok(v) = serde_json::from_str::<T>(line) {
                out.push(v);
            }
        }
    }
    out
}
