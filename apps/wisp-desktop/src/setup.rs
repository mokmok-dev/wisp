//! Setup glue for recognizer selection and local model download.

use std::path::{Path, PathBuf};

use gpui::{App, AsyncApp, Entity};
use wisp_audiokit::{RecognizerBackend, download_local_model, local_model_status};

use crate::app::{AppModel, ModelDownloadState};

pub fn refresh(
    model: &Entity<AppModel>,
    data_dir: &Path,
    cx: &mut App,
) {
    let status = local_model_status(data_dir);
    model.update(cx, |m, cx| {
        if m.setup.local_model != status {
            m.setup.local_model = status;
            cx.notify();
        }
    });
}

pub fn select_recognizer(
    recognizer: RecognizerBackend,
    model: &Entity<AppModel>,
    cx: &mut App,
) {
    model.update(cx, |m, cx| {
        m.setup.recognizer = recognizer;
        m.setup.model_error = None;
        cx.notify();
    });
}

pub fn download_model(
    model: Entity<AppModel>,
    data_dir: PathBuf,
    cx: &mut App,
) {
    if model.read(cx).setup.model_download == ModelDownloadState::Downloading {
        return;
    }
    model.update(cx, |m, cx| {
        m.setup.model_download = ModelDownloadState::Downloading;
        m.setup.model_error = None;
        cx.notify();
    });

    cx.spawn(async move |cx: &mut AsyncApp| {
        let data_dir_for_download = data_dir.clone();
        let result = cx
            .background_executor()
            .spawn(async move { download_local_model(data_dir_for_download) })
            .await;
        let _ = model.update(cx, |m, cx| {
            m.setup.model_download = ModelDownloadState::Idle;
            match result {
                Ok(status) => {
                    m.setup.local_model = status;
                    m.setup.recognizer = RecognizerBackend::LocalModel;
                },
                Err(err) => {
                    m.setup.local_model = local_model_status(data_dir);
                    m.setup.model_error = Some(err.to_string());
                },
            }
            cx.notify();
        });
    })
    .detach();
}
