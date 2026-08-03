//! Setup glue for recognizer selection and local model download.

use std::path::{Path, PathBuf};

use gpui::{App, AsyncApp, Entity};
use wisp_audiokit::{
    LocalModelId, RecognizerBackend, download_local_model_for_with_progress, local_model_status_for,
};

use crate::app::{AppModel, ModelDownloadState};

enum DownloadUpdate {
    Progress(u64, u64),
    Finished(wisp_audiokit::SetupResult<wisp_audiokit::LocalModelStatus>),
}

fn download_is_current(
    setup: &crate::app::Setup,
    id: LocalModelId,
    generation: u64,
) -> bool {
    setup.local_model_id == id && setup.download_generation == generation
}

pub fn refresh(
    model: &Entity<AppModel>,
    data_dir: &Path,
    cx: &mut App,
) {
    let id = model.read(cx).setup.local_model_id;
    let status = local_model_status_for(data_dir, id);
    model.update(cx, |m, cx| {
        if m.setup.local_model != status {
            m.setup.local_model = status;
            cx.notify();
        }
    });
}

pub fn select_model(
    id: LocalModelId,
    model: &Entity<AppModel>,
    data_dir: &Path,
    cx: &mut App,
) {
    let status = local_model_status_for(data_dir, id);
    model.update(cx, |m, cx| {
        m.setup.download_generation = m.setup.download_generation.wrapping_add(1);
        m.setup.local_model_id = id;
        m.setup.local_model = status;
        m.setup.model_download = ModelDownloadState::Idle;
        m.setup.model_error = None;
        cx.notify();
    });
    let mut settings = crate::settings::load(data_dir);
    settings.transcription.model = id.into();
    if let Err(error) = crate::settings::save(data_dir, &settings) {
        eprintln!("wisp: failed to save Whisper model: {error}");
    }
}

pub fn select_recognizer(
    recognizer: RecognizerBackend,
    model: &Entity<AppModel>,
    data_dir: &Path,
    cx: &mut App,
) {
    model.update(cx, |m, cx| {
        m.setup.download_generation = m.setup.download_generation.wrapping_add(1);
        m.setup.recognizer = recognizer;
        m.setup.model_error = None;
        cx.notify();
    });
    let mut settings = crate::settings::load(data_dir);
    settings.transcription.provider = recognizer.into();
    if let Err(error) = crate::settings::save(data_dir, &settings) {
        eprintln!("wisp: failed to save transcription provider: {error}");
    }
}

#[allow(clippy::too_many_lines)]
pub fn download_model(
    model: Entity<AppModel>,
    data_dir: PathBuf,
    cx: &mut App,
) {
    if matches!(
        model.read(cx).setup.model_download,
        ModelDownloadState::Downloading { .. }
    ) {
        return;
    }
    let id = model.read(cx).setup.local_model_id;
    let generation = model.read(cx).setup.download_generation.wrapping_add(1);
    let total = wisp_audiokit::local_model_spec_for(id).bytes;
    model.update(cx, |m, cx| {
        m.setup.model_download = ModelDownloadState::Downloading {
            model: id,
            generation,
            downloaded: 0,
            total,
        };
        m.setup.download_generation = generation;
        m.setup.model_error = None;
        cx.notify();
    });
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn({
        let data_dir = data_dir.clone();
        move || {
            let progress_sender = sender.clone();
            let result =
                download_local_model_for_with_progress(&data_dir, id, move |downloaded, total| {
                    let _ = progress_sender.send(DownloadUpdate::Progress(downloaded, total));
                });
            let _ = sender.send(DownloadUpdate::Finished(result));
        }
    });
    cx.spawn(async move |cx: &mut AsyncApp| {
        loop {
            while let Ok(update) = receiver.try_recv() {
                match update {
                    DownloadUpdate::Progress(downloaded, total) => {
                        let _ = model.update(cx, |m, cx| {
                            if !download_is_current(&m.setup, id, generation) {
                                return;
                            }
                            m.setup.model_download = ModelDownloadState::Downloading {
                                model: id,
                                generation,
                                downloaded,
                                total,
                            };
                            cx.notify();
                        });
                    },
                    DownloadUpdate::Finished(result) => {
                        let _ = model.update(cx, |m, cx| {
                            if !download_is_current(&m.setup, id, generation) {
                                if matches!(
                                    m.setup.model_download,
                                    ModelDownloadState::Downloading {
                                        generation: active,
                                        ..
                                    } if active == generation
                                ) {
                                    m.setup.model_download = ModelDownloadState::Idle;
                                    if m.setup.local_model_id == id {
                                        m.setup.local_model =
                                            local_model_status_for(&data_dir, id);
                                    }
                                    cx.notify();
                                }
                                return;
                            }
                            m.setup.model_download = ModelDownloadState::Idle;
                            match result {
                                Ok(status) => {
                                    m.setup.local_model = status;
                                    if m.setup.local_model_id != id {
                                        return;
                                    }
                                    m.setup.recognizer = RecognizerBackend::LocalModel;
                                    let mut settings = crate::settings::load(&data_dir);
                                    settings.transcription.provider =
                                        RecognizerBackend::LocalModel.into();
                                    if let Err(error) =
                                        crate::settings::save(&data_dir, &settings)
                                    {
                                        m.setup.model_error = Some(format!(
                                            "model installed, but settings could not be saved: {error}"
                                        ));
                                    }
                                },
                                Err(err) => {
                                    m.setup.local_model = local_model_status_for(data_dir, id);
                                    m.setup.model_error = Some(err.to_string());
                                },
                            }
                            cx.notify();
                        });
                        return;
                    },
                }
            }
            cx.background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use wisp_audiokit::LocalModelId;

    use super::download_is_current;

    #[test]
    fn model_or_generation_change_invalidates_download_completion() {
        let data_dir = tempfile::tempdir().expect("temp dir");
        let mut setup = crate::app::Setup::new(data_dir.path());
        setup.local_model_id = LocalModelId::Base;
        setup.download_generation = 7;
        assert!(download_is_current(&setup, LocalModelId::Base, 7));
        assert!(!download_is_current(&setup, LocalModelId::Tiny, 7));
        assert!(!download_is_current(&setup, LocalModelId::Base, 6));
    }
}
