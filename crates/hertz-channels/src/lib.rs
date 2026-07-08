use anyhow::{anyhow, Result};
use hertz_types::{Channel, TxPolicy};
use std::collections::HashMap;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct GroupMetadata {
    pub group_id: String,
    pub center_hz: Option<u64>,
    pub sample_rate: Option<u32>,
    pub channel_ids: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ChannelDb {
    pub channels: HashMap<String, Channel>,
    pub groups: HashMap<String, GroupMetadata>,
}

#[derive(Clone, Debug, serde::Deserialize)]
struct BandplanFile {
    pub group: String,
    pub center_hz: Option<u64>,
    pub sample_rate: Option<u32>,
    #[serde(rename = "channel")]
    pub channels: Vec<Channel>,
}

impl Default for ChannelDb {
    fn default() -> Self {
        Self::new()
    }
}

impl ChannelDb {
    pub fn new() -> Self {
        Self {
            channels: HashMap::new(),
            groups: HashMap::new(),
        }
    }

    pub fn get_by_id(&self, id: &str) -> Option<&Channel> {
        self.channels.get(id)
    }

    pub fn get_by_group(&self, group: &str) -> Vec<&Channel> {
        if let Some(group_meta) = self.groups.get(group) {
            group_meta
                .channel_ids
                .iter()
                .filter_map(|id| self.channels.get(id))
                .collect()
        } else {
            Vec::new()
        }
    }

    pub fn get_by_freq(&self, freq_hz: u64, tolerance_hz: u64) -> Vec<&Channel> {
        let mut results = Vec::new();
        for channel in self.channels.values() {
            let diff = (channel.freq_hz as i64 - freq_hz as i64).abs();
            if diff <= tolerance_hz as i64 {
                results.push(channel);
            }
        }
        results
    }
}

pub fn is_never_tx_group(group: &str) -> bool {
    let g = group.to_lowercase();
    g.contains("marine") || g.contains("air") || g.contains("rail") || g == "noaa-wx"
}

pub fn load_channels<P: AsRef<Path>>(bandplans_dir: P, user_dir: Option<P>) -> Result<ChannelDb> {
    let mut db = ChannelDb::new();

    let mut load_dir = |path: &Path| -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let file_path = entry.path();
            if file_path.is_file() && file_path.extension().is_some_and(|ext| ext == "toml") {
                let content = std::fs::read_to_string(&file_path)?;
                let file_data: BandplanFile = toml::from_str(&content)
                    .map_err(|e| anyhow!("Failed to parse file {:?}: {}", file_path, e))?;

                let group_id = file_data.group.clone();
                let group_meta =
                    db.groups
                        .entry(group_id.clone())
                        .or_insert_with(|| GroupMetadata {
                            group_id: group_id.clone(),
                            center_hz: file_data.center_hz,
                            sample_rate: file_data.sample_rate,
                            channel_ids: Vec::new(),
                        });

                if file_data.center_hz.is_some() && group_meta.center_hz.is_none() {
                    group_meta.center_hz = file_data.center_hz;
                }
                if file_data.sample_rate.is_some() && group_meta.sample_rate.is_none() {
                    group_meta.sample_rate = file_data.sample_rate;
                }

                for channel in file_data.channels {
                    if db.channels.contains_key(&channel.id) {
                        return Err(anyhow!("Duplicate channel ID: {}", channel.id));
                    }

                    if channel.group != group_id {
                        return Err(anyhow!(
                            "Channel {} group '{}' does not match file group '{}'",
                            channel.id,
                            channel.group,
                            group_id
                        ));
                    }

                    if is_never_tx_group(&group_id) && channel.tx_policy != TxPolicy::Never {
                        return Err(anyhow!(
                            "Channel {} is in restricted group '{}' but has non-Never TX policy",
                            channel.id,
                            group_id
                        ));
                    }

                    if let (Some(center), Some(rate)) =
                        (group_meta.center_hz, group_meta.sample_rate)
                    {
                        let half_span = (rate / 2) as i64;
                        let min_freq = center as i64 - half_span;
                        let max_freq = center as i64 + half_span;
                        let f = channel.freq_hz as i64;
                        if f < min_freq || f > max_freq {
                            return Err(anyhow!(
                                "Channel {} frequency {} Hz is outside capture range [{}, {}] Hz for group '{}'",
                                channel.id,
                                channel.freq_hz,
                                min_freq,
                                max_freq,
                                group_id
                            ));
                        }
                    }

                    group_meta.channel_ids.push(channel.id.clone());
                    db.channels.insert(channel.id.clone(), channel);
                }
            }
        }
        Ok(())
    };

    load_dir(bandplans_dir.as_ref())?;
    if let Some(user_path) = user_dir {
        load_dir(user_path.as_ref())?;
    }

    Ok(db)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bandplans_load_and_validate() {
        let bandplans_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bandplans");
        let db = load_channels(&bandplans_dir, None).unwrap();
        println!("Total channel count loaded: {}", db.channels.len());
        assert!(
            db.channels.len() >= 400,
            "Expected at least 400 channels, got {}",
            db.channels.len()
        );
    }

    #[test]
    fn test_marine_roundtrip() {
        let bandplans_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bandplans");
        let db = load_channels(&bandplans_dir, None).unwrap();

        // Find channel "16"
        let ch16 = db.get_by_id("16").expect("Channel 16 not found");
        assert_eq!(ch16.freq_hz, 156_800_000);
        assert_eq!(ch16.group, "marine-vhf-us");

        // Lookup by frequency
        let matches = db.get_by_freq(156_800_000, 1_000);
        let has_ch16 = matches.iter().any(|c| c.id == "16");
        assert!(
            has_ch16,
            "Expected to find channel 16 when querying 156.800 MHz"
        );
    }

    #[test]
    fn test_tx_legality() {
        let temp_dir = std::env::temp_dir().join(format!("hertz_test_{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let bad_toml = r#"
group = "marine-vhf-us"

[[channel]]
id = "16"
name = "Channel 16"
freq_hz = 156800000
mode = "nfm"
bandwidth_hz = 25000
group = "marine-vhf-us"
label = "SAFETY-CALL"
rx = true
tx_policy = "certified-radio+none"
"#;

        let bad_toml_path = temp_dir.join("marine_bad.toml");
        std::fs::write(&bad_toml_path, bad_toml).unwrap();

        // Load bad bandplan, should error due to TX policy on marine channel
        let res = load_channels(&temp_dir, None);
        assert!(
            res.is_err(),
            "Expected load_channels to fail for marine channel with non-Never TX policy"
        );
        let err_msg = res.unwrap_err().to_string();
        assert!(
            err_msg.contains("non-Never TX policy"),
            "Expected error to mention non-Never TX policy"
        );

        // Clean up
        let _ = std::fs::remove_file(&bad_toml_path);
        let _ = std::fs::remove_dir(&temp_dir);
    }
}
