//! First-use download of the engine and weights from Hugging Face.
//!
//! The engine library is pulled out of the `cactus-needle` wheel for this
//! platform (`Cactus-Compute/needle3/python/...`), since only the wheels carry
//! a shared library; the weights come from `Cactus-Compute/whistle`.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::{lib_file_name, Error, Result, ENGINE_VERSION};

const HF: &str = "https://huggingface.co";

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

fn get(url: &str) -> Result<Vec<u8>> {
    let resp = ureq::get(url)
        .call()
        .map_err(|e| Error::Download(format!("{url}: {e}")))?;
    let mut body = Vec::new();
    resp.into_reader().read_to_end(&mut body)?;
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
    let url = format!("{HF}/Cactus-Compute/needle3/resolve/main/python/cactus_needle-{ENGINE_VERSION}-py3-none-{tag}.whl");
    let wheel = get(&url)?;
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
            &get(&format!(
                "{HF}/Cactus-Compute/whistle/resolve/main/whistle.cact"
            ))?,
        )?;
    }
    Ok(dest)
}
