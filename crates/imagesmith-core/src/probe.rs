//! Base image probe results (produced by `build.sh --probe` in the store).

use crate::recipe::PartitionRef;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Probe {
    pub sha256: String,
    /// "gpt" or "dos"
    pub table: String,
    pub sector_size: u64,
    pub image_bytes: u64,
    pub partitions: Vec<Partition>,
    /// Detected rootfs (last ext4 partition), if any.
    #[serde(default)]
    pub rootfs: Option<RootfsInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Partition {
    pub number: u32,
    pub start_bytes: u64,
    pub size_bytes: u64,
    #[serde(default)]
    pub fstype: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub uuid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RootfsInfo {
    pub partition: u32,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub distro: Option<String>,
    #[serde(default)]
    pub distro_id: Option<String>,
    #[serde(default)]
    pub codename: Option<String>,
    #[serde(default)]
    pub used_bytes: u64,
    /// Existing first-boot resize service, if any.
    #[serde(default)]
    pub resize_service: Option<String>,
    /// Binaries with file capabilities, which `debugfs rdump` drops.
    #[serde(default)]
    pub caps: Vec<FileCap>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileCap {
    pub path: String,
    pub caps: String,
}

impl Probe {
    pub fn partition(&self, n: u32) -> Option<&Partition> {
        self.partitions.iter().find(|p| p.number == n)
    }

    pub fn last_partition(&self) -> Option<&Partition> {
        self.partitions.iter().max_by_key(|p| p.start_bytes)
    }

    pub fn is_last(&self, n: u32) -> bool {
        self.last_partition().is_some_and(|p| p.number == n)
    }

    pub fn resolve(&self, r: PartitionRef) -> Option<&Partition> {
        match r {
            PartitionRef::Number(n) => self.partition(n),
            PartitionRef::Auto => self
                .partitions
                .iter()
                .filter(|p| p.fstype.as_deref() == Some("ext4"))
                .max_by_key(|p| p.start_bytes),
        }
    }

    /// "GPT · 3 partitions · arm64 · Debian 12"
    pub fn summary(&self) -> String {
        let mut parts = vec![
            self.table.to_uppercase(),
            format!("{} partition{}", self.partitions.len(), if self.partitions.len() == 1 { "" } else { "s" }),
        ];
        if let Some(r) = &self.rootfs {
            parts.extend(r.arch.clone());
            parts.extend(r.distro.clone());
        }
        parts.join(" · ")
    }

    pub fn cache_path(sha: &str) -> std::path::PathBuf {
        crate::app_dirs::cache_dir().join("probes").join(format!("{sha}.json"))
    }

    pub fn load_cached(sha: &str) -> Option<Probe> {
        serde_json::from_slice(&std::fs::read(Self::cache_path(sha)).ok()?).ok()
    }

    pub fn save_cached(&self) -> crate::Result<()> {
        let p = Self::cache_path(&self.sha256);
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(p, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}
