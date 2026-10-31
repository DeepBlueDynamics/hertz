//! First-use download of the engine and weights from Hugging Face.
//!
//! The engine library is pulled out of the `cactus-needle` wheel for this
//! platform (`Cactus-Compute/needle3/python/...`), since only the wheels carry
//! a shared library; the weights come from `Cactus-Compute/whistle`.
//!
//! Both are fetched from a pinned commit and checked against a pinned SHA-256
//! before anything is written to the cache: the engine is native code hertzd
//! loads, so a changed upstream file must fail, not run.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{lib_file_name, Error, Result, ENGINE_VERSION};

const HF: &str = "https://huggingface.co";

/// `Cactus-Compute/needle3` commit the engine wheels are fetched from.
const NEEDLE3_REVISION: &str = "d6c7d6a2e9655505e0f8e23339356dd1705900eb";
/// `Cactus-Compute/whistle` commit the weights are fetched from.
const WHISTLE_REVISION: &str = "741118687e1f008f06d1351a402a731274789790";
/// SHA-256 of `whistle.cact` at `WHISTLE_REVISION`.
const WEIGHTS_SHA256: &str = "b6e02f048568ac5d01a2042556c658061e699acbc0aa2a1439f52f3d461dffeb";

/// SHA-256 of the `cactus_needle-<ENGINE_VERSION>` wheel for each platform tag.
/// Bumping `ENGINE_VERSION` means updating these and `NEEDLE3_REVISION`.
fn wheel_sha256(tag: &str) -> Option<&'static str> {
    Some(match tag {
        "macosx_11_0_arm64" => "19534f411ad71312031d6df5418d0e55b8d16075c28a217aef4b921349bdfb4a",
        "macosx_11_0_x86_64" => "e74ac1204ffe29d75db1215d889ad46c8add39e52042df22cd99c2e37855c273",
        "manylinux2014_aarch64" => {
            "3128efab8a8cf9f8381a6ea38eee3db66d85746260489563513160940dcbaf63"
        }
        "manylinux2014_x86_64" => {
            "3de03a183f56e28f8f1945fbbc5048efca0699a7f3ed0d66a244ca7e30442c22"
        }
        "musllinux_1_2_aarch64" => {
            "c5c73a77cdcdde4b614d8e618c0338d1c8c7be6e32f8d5b6c06f27ffa467ce66"
        }
        "musllinux_1_2_x86_64" => {
            "8d40a1253993426227095e5886294bf0baccace8ec3fce677bc3cc1deacc058a"
        }
        "win_amd64" => "4e50c5fa983c8737de200d9e96af937dd8135371ca557fe8c7c9eb1adba39f5a",
        "win_arm64" => "4387a5fbca30b4bd7da6b8bb7674a888c26d507f0da2723885f64e9a39f3f437",
        _ => return None,
    })
}

/// The wheel platform tag for the target this crate was compiled for.
pub fn wheel_tag() -> Option<&'static str> {
    let arm = cfg!(target_arch = "aarch64");
    let x64 = cfg!(target_arch = "x86_64");
    let musl = cfg!(target_env = "musl");
    let tag = if cfg!(windows) {
        if x64 {
            "win_amd64"
        } else if arm {
            "win_arm64"
        } else {
            return None;
        }
    } else if cfg!(target_os = "macos") {
        if arm {
            "macosx_11_0_arm64"
        } else if x64 {
            "macosx_11_0_x86_64"
        } else {
            return None;
        }
    } else if cfg!(target_os = "linux") {
        match (musl, x64, arm) {
            (true, true, _) => "musllinux_1_2_x86_64",
            (true, _, true) => "musllinux_1_2_aarch64",
            (false, true, _) => "manylinux2014_x86_64",
            (false, _, true) => "manylinux2014_aarch64",
            _ => return None,
        }
    } else {
        return None;
    };
    Some(tag)
}

/// Attempts per download. huggingface.co resets some TLS handshakes outright
/// ("forcibly closed by the remote host"; 5 of 8 direct tries in one run), in
/// bursts that outlasted 5 tries over 15 s, so this waits up to about a minute.
const ATTEMPTS: u32 = 8;

fn get(url: &str) -> Result<Vec<u8>> {
    let mut last = String::new();
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(
                (500u64 << attempt).min(15_000),
            ));
        }
        match ureq::get(url).call() {
            Ok(resp) => {
                let mut body = Vec::new();
                match resp.into_reader().read_to_end(&mut body) {
                    Ok(_) => return Ok(body),
                    Err(e) => last = e.to_string(),
                }
            }
            // A status answer (404, 403) won't change on a retry.
            Err(e @ ureq::Error::Status(..)) => return Err(Error::Download(format!("{url}: {e}"))),
            Err(e) => last = e.to_string(),
        }
    }
    Err(Error::Download(format!(
        "{url}: {last} (after {ATTEMPTS} attempts)"
    )))
}

/// Download `url` and refuse it unless its SHA-256 is `expected`.
fn get_verified(url: &str, expected: &str) -> Result<Vec<u8>> {
    let body = get(url)?;
    let actual: String = Sha256::digest(&body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if actual != expected {
        return Err(Error::Download(format!(
            "{url}: SHA-256 {actual} does not match the pinned {expected}"
        )));
    }
    Ok(body)
}

/// Write `bytes` to `dest` via a temp file so a half-written file is never
/// mistaken for a cached one.
fn write_atomic(dest: &Path, bytes: &[u8]) -> Result<()> {
    fs::create_dir_all(dest.parent().unwrap_or(Path::new(".")))?;
    let tmp = dest.with_extension("part");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, dest)?;
    Ok(())
}

/// Download (once) the engine shared library into `<cache>/<version>/`.
pub fn engine(cache: &Path) -> Result<PathBuf> {
    let dest = cache.join(ENGINE_VERSION).join(lib_file_name());
    if dest.exists() {
        return Ok(dest);
    }
    let tag =
        wheel_tag().ok_or_else(|| Error::Download("no prebuilt engine for this target".into()))?;
    let sha256 = wheel_sha256(tag)
        .ok_or_else(|| Error::Download(format!("no pinned engine hash for {tag}")))?;
    let url = format!("{HF}/Cactus-Compute/needle3/resolve/{NEEDLE3_REVISION}/python/cactus_needle-{ENGINE_VERSION}-py3-none-{tag}.whl");
    let wheel = get_verified(&url, sha256)?;
    let mut zip =
        zip::ZipArchive::new(io::Cursor::new(wheel)).map_err(|e| Error::Download(e.to_string()))?;
    let member = format!("needle/{}", lib_file_name());
    let mut file = zip
        .by_name(&member)
        .map_err(|e| Error::Download(format!("{member}: {e}")))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    write_atomic(&dest, &bytes)?;
    Ok(dest)
}

/// Download (once) `whistle.cact` into `<cache>/`.
pub fn weights(cache: &Path) -> Result<PathBuf> {
    let dest = cache.join("whistle.cact");
    if !dest.exists() {
        write_atomic(
            &dest,
            &get_verified(
                &format!("{HF}/Cactus-Compute/whistle/resolve/{WHISTLE_REVISION}/whistle.cact"),
                WEIGHTS_SHA256,
            )?,
        )?;
    }
    Ok(dest)
}
