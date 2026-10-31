//! App-owned locations on the host. Overridable with IMAGESMITH_HOME (tests).

use std::path::PathBuf;

fn home_override() -> Option<PathBuf> {
    std::env::var_os("IMAGESMITH_HOME").map(PathBuf::from)
}

/// ~/Library/Application Support/ImageSmith
pub fn support_dir() -> PathBuf {
    home_override()
        .map(|h| h.join("support"))
        .or_else(|| dirs::data_dir().map(|d| d.join("ImageSmith")))
        .unwrap_or_else(|| PathBuf::from(".imagesmith"))
}

/// ~/Library/Caches/ImageSmith
pub fn cache_dir() -> PathBuf {
    home_override()
        .map(|h| h.join("cache"))
        .or_else(|| dirs::cache_dir().map(|d| d.join("ImageSmith")))
        .unwrap_or_else(|| PathBuf::from(".imagesmith-cache"))
}

pub fn projects_index() -> PathBuf {
    support_dir().join("projects.json")
}

pub fn settings_file() -> PathBuf {
    support_dir().join("settings.toml")
}

/// Assistant conversations, one JSON file per project ID.
pub fn chats_dir() -> PathBuf {
    support_dir().join("chats")
}

pub fn expand_tilde(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().map(|h| h.join(rest)).unwrap_or_else(|| PathBuf::from(p)),
        None if p == "~" => dirs::home_dir().unwrap_or_else(|| PathBuf::from(p)),
        None => PathBuf::from(p),
    }
}
