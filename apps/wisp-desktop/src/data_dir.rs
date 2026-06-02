//! Wisp application data directory (matches `wisp-audiokit-win` on Windows).

use std::path::PathBuf;

/// Root folder for sessions, SQLite, and downloaded Vosk models.
pub fn wisp_data_root() -> PathBuf {
    if let Ok(dir) = std::env::var("WISP_DATA_DIR") {
        return PathBuf::from(dir);
    }

    #[cfg(target_os = "windows")]
    {
        return std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("dev.mokmok.wisp");
    }

    #[cfg(target_os = "macos")]
    {
        let mut path = std::env::var_os("HOME").map_or_else(std::env::temp_dir, PathBuf::from);
        path.push("Library");
        path.push("Application Support");
        path.push("dev.mokmok.wisp");
        return path;
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::env::var_os("HOME")
            .map_or_else(std::env::temp_dir, PathBuf::from)
            .join("dev.mokmok.wisp")
    }
}
