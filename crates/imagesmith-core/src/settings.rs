//! User settings (settings.toml).

use crate::recipe::{Compress, SizeMode};
use crate::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_AI_MODEL: &str = "anthropic/claude-sonnet-4.5";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Path to the `container` binary; auto-detected when unset.
    pub container_bin: Option<PathBuf>,
    pub start_runtime_at_launch: bool,
    pub cpus: u32,
    pub memory_gb: u32,
    pub cache_cap_gb: u64,
    pub default_output_dir: Option<PathBuf>,
    pub compress: Compress,
    pub level: Option<i32>,
    pub bmap: bool,
    pub checksum: bool,
    pub size_mode: SizeMode,
    pub theme: Theme,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub openrouter_api_key: Option<String>,
    /// OpenRouter model ID.
    pub ai_model: String,
    pub ai_provider: AiProvider,
    /// LM Studio server, without /v1.
    pub lmstudio_url: String,
    pub lmstudio_model: String,
    /// Only needed when the LM Studio server requires authentication.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lmstudio_api_key: Option<String>,
    /// Lets the assistant search the web, whichever model it runs on.
    pub web_search: WebSearch,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub langsearch_api_key: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WebSearch {
    #[default]
    Off,
    LangSearch,
    /// OpenRouter's web search server tool, run with the OpenRouter key and model.
    OpenRouter,
}

pub const DEFAULT_LMSTUDIO_URL: &str = "http://localhost:1234";

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum AiProvider {
    #[default]
    OpenRouter,
    LmStudio,
}

impl Settings {
    /// Model ID for the selected provider.
    pub fn active_model(&self) -> &str {
        match self.ai_provider {
            AiProvider::OpenRouter => &self.ai_model,
            AiProvider::LmStudio => &self.lmstudio_model,
        }
    }

    pub fn set_active_model(&mut self, m: String) {
        match self.ai_provider {
            AiProvider::OpenRouter => self.ai_model = m,
            AiProvider::LmStudio => self.lmstudio_model = m,
        }
    }

    pub fn lmstudio_base(&self) -> String {
        let u = self.lmstudio_url.trim().trim_end_matches('/');
        let u = u.strip_suffix("/v1").unwrap_or(u);
        if u.is_empty() { DEFAULT_LMSTUDIO_URL.into() } else { u.to_string() }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

impl Default for Settings {
    fn default() -> Self {
        let host_cpus = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4);
        Settings {
            container_bin: None,
            start_runtime_at_launch: true,
            cpus: (host_cpus / 2).max(1),
            memory_gb: 4,
            cache_cap_gb: 20,
            default_output_dir: None,
            compress: Compress::Zstd,
            level: None,
            bmap: true,
            checksum: true,
            size_mode: SizeMode::Shrink,
            theme: Theme::System,
            openrouter_api_key: None,
            ai_model: DEFAULT_AI_MODEL.into(),
            ai_provider: AiProvider::OpenRouter,
            lmstudio_url: DEFAULT_LMSTUDIO_URL.into(),
            lmstudio_model: String::new(),
            lmstudio_api_key: None,
            web_search: WebSearch::Off,
            langsearch_api_key: None,
        }
    }
}

impl Settings {
    pub fn load() -> Settings {
        std::fs::read_to_string(crate::app_dirs::settings_file())
            .ok()
            .and_then(|t| toml_edit::de::from_str::<Settings>(&t).ok())
            .map(Settings::clamped)
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = crate::app_dirs::settings_file();
        std::fs::create_dir_all(path.parent().unwrap())?;
        let text = toml_edit::ser::to_string_pretty(self).map_err(|e| crate::Error::Other(e.to_string()))?;
        // Holds the OpenRouter API key.
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(text.as_bytes())?;
        Ok(())
    }

    fn clamped(mut self) -> Settings {
        let host_cpus = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4);
        self.cpus = self.cpus.clamp(1, host_cpus);
        self.memory_gb = self.memory_gb.clamp(2, 16);
        if self.ai_model.trim().is_empty() {
            self.ai_model = DEFAULT_AI_MODEL.into();
        }
        self
    }

    pub fn container_bin(&self) -> PathBuf {
        if let Some(p) = &self.container_bin {
            return p.clone();
        }
        ["/usr/local/bin/container", "/opt/homebrew/bin/container"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.exists())
            .unwrap_or_else(|| PathBuf::from("container"))
    }
}
