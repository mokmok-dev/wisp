//! Download and extract official Vosk language models into the Wisp data folder.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use zip::ZipArchive;

use crate::speech;

/// Download the recommended small model for `locale` if it is not already present.
pub fn ensure_model(
    locale: &str,
    data_root: &Path,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf, String> {
    if let Some(path) = speech::resolve_model_path(locale, data_root) {
        return Ok(path);
    }

    let (dir_name, url) = model_artifact(locale).ok_or_else(|| {
        format!("no Vosk model mapping for locale {locale:?}; set WISP_VOSK_MODEL")
    })?;

    let models_dir = data_root.join("models");
    fs::create_dir_all(&models_dir).map_err(|e| format!("create models dir: {e}"))?;

    let dest = models_dir.join(dir_name);
    if dest.is_dir() {
        return Ok(dest);
    }

    let zip_path = models_dir.join(format!("{dir_name}.zip"));
    let partial = models_dir.join(format!("{dir_name}.zip.partial"));

    if let Err(err) = download_file(url, &partial, &mut on_progress) {
        let _ = fs::remove_file(&partial);
        return Err(err);
    }

    fs::rename(&partial, &zip_path).map_err(|e| format!("rename download: {e}"))?;

    if let Err(err) = extract_zip(&zip_path, &models_dir) {
        let _ = fs::remove_file(&zip_path);
        return Err(err);
    }

    let _ = fs::remove_file(&zip_path);

    if dest.is_dir() {
        Ok(dest)
    } else {
        Err(format!(
            "extracted archive but {} is missing; try deleting {} and retrying",
            dest.display(),
            models_dir.display()
        ))
    }
}

/// Official small-model zip for a locale (`dir_name`, download URL).
pub fn model_artifact(locale: &str) -> Option<(&'static str, &'static str)> {
    let dir_name = speech::model_candidates_for_locale(locale).first()?;
    let url = match *dir_name {
        "vosk-model-small-ja-0.22" => {
            "https://alphacephei.com/vosk/models/vosk-model-small-ja-0.22.zip"
        },
        "vosk-model-small-en-us-0.15" => {
            "https://alphacephei.com/vosk/models/vosk-model-small-en-us-0.15.zip"
        },
        _ => return None,
    };
    Some((dir_name, url))
}

fn download_file(
    url: &str,
    dest: &Path,
    on_progress: &mut impl FnMut(u64, Option<u64>),
) -> Result<(), String> {
    let response = ureq::get(url)
        .call()
        .map_err(|e| format!("download {url}: {e}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "download {url}: HTTP {}",
            response.status().as_str()
        ));
    }

    let total = response
        .header("Content-Length")
        .and_then(|value| value.parse::<u64>().ok());

    let mut reader = response.into_reader();
    let mut file = File::create(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;

    let mut received = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    on_progress(0, total);

    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|e| format!("read download stream: {e}"))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .map_err(|e| format!("write {}: {e}", dest.display()))?;
        received = received.saturating_add(read as u64);
        on_progress(received, total);
    }

    file.sync_all()
        .map_err(|e| format!("flush {}: {e}", dest.display()))?;

    Ok(())
}

fn extract_zip(
    archive_path: &Path,
    dest_dir: &Path,
) -> Result<(), String> {
    let file =
        File::open(archive_path).map_err(|e| format!("open {}: {e}", archive_path.display()))?;
    let mut archive =
        ZipArchive::new(file).map_err(|e| format!("read zip {}: {e}", archive_path.display()))?;

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| format!("zip entry {index}: {e}"))?;
        let Some(name) = entry.enclosed_name().map(Path::to_path_buf) else {
            continue;
        };
        let out_path = dest_dir.join(name);
        if entry.is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| io_error("create dir", &out_path, e))?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent).map_err(|e| io_error("create parent", parent, e))?;
            }
            let mut out_file =
                File::create(&out_path).map_err(|e| io_error("create file", &out_path, e))?;
            io::copy(&mut entry, &mut out_file)
                .map_err(|e| io_error("extract file", &out_path, e))?;
        }
    }
    Ok(())
}

fn io_error(
    action: &str,
    path: &Path,
    err: io::Error,
) -> String {
    format!("{action} {}: {err}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ja_locale_maps_to_small_model_zip() {
        let (dir, url) = model_artifact("ja-JP").expect("ja-JP mapping");
        assert_eq!(dir, "vosk-model-small-ja-0.22");
        assert!(url.contains("vosk-model-small-ja-0.22.zip"));
    }
}
