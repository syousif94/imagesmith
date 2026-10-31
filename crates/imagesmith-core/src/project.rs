//! Project folders (user-owned, in git) and the app's project index.

use crate::recipe::{self, Arch, PartitionRef, RecipeFile};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const RECIPE: &str = "recipe.toml";
pub const GITIGNORE: &str = "bases/\nbuilds/\n";
pub const AGENTS_MD: &str = r#"# ImageSmith project

This folder is an ImageSmith project: `recipe.toml` customizes a Linux disk image
(Debian-family rootfs) and ImageSmith builds it on a Mac. Read `recipe.toml` first;
`[image].arch` (`arm64` | `amd64`) is the CPU every binary you add must target.

## Layout

| Path | Purpose |
|---|---|
| `recipe.toml` | The recipe. Edit it to add files, packages, services, steps. |
| `overlay/` | Mirror of the image's `/`. Everything here is copied into the rootfs. |
| `files/` | Single files placed via `[[files]]` entries with explicit dst/mode/owner. |
| `bases/` | Base images (git-ignored). Do not modify. |
| `builds/` | Build outputs (git-ignored). Do not write here. |

## Getting things into the image

**overlay/** (preferred for trees of files): `overlay/usr/local/bin/foo` lands at
`/usr/local/bin/foo`. Ownership becomes `root:root`; file modes are kept, so
`chmod 0755` executables and `chmod 0600` secrets before building. Symlinks are
copied as symlinks and resolve against the image.

**files/** + `[[files]]` (when you need a specific owner or mode):

```toml
[[files]]
src = "files/myapp"
dst = "/opt/myapp/bin/myapp"
mode = "0755"           # default 0644
owner = "myuser:myuser"  # default root:root; user must exist in the image
```

Order during a build: `when = "first"` steps -> packages -> `[[files]]` ->
`overlay/` -> `[services]` -> `[[steps]]`. So default steps can use the files,
and services can reference units you added.

## Binaries

- Must be Linux ELF for the recipe arch, never macOS Mach-O. Check with `file`.
- Prefer fully static builds so they don't depend on the image's libc:
  - Rust: `cargo zigbuild --release --target aarch64-unknown-linux-musl`
    (or `x86_64-unknown-linux-musl` for amd64).
  - Go: `CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build`.
  - C: `zig cc -target aarch64-linux-musl -static`.
- If a binary must link dynamically, install its runtime libs via
  `[packages].install` and build against the same distro release.
- If it's packaged for the distro, `[packages].install` beats shipping a binary.

## Scripts

- Start with a shebang (`#!/bin/sh` or `#!/bin/bash`), use LF line endings,
  and make them executable (`chmod 0755` in overlay/, or `mode = "0755"`).
- Only use interpreters present in the image (add them to `[packages].install`).
- systemd units go in `overlay/etc/systemd/system/<name>.service`; add
  `<name>` to `[services].enable`.

## Running commands

- `[[steps]]` run as root inside the image (chroot, emulated if needed) during
  the build. Use them for configuration that must happen in the target, not
  for cross-compiling:

  ```toml
  [[steps]]
  name = "Enable foo"
  run = "/usr/local/bin/foo --init"
  when = "last"  # or "first"; default last
  ```

- `[[host_scripts]]` run bash on the Mac in this folder (same fields as steps).
  `when = "first"` runs before the image is touched: use it to build binaries
  into `overlay/` or `files/`. `when = "last"` runs after export with
  `$IMAGESMITH_IMAGE` and `$IMAGESMITH_OUTPUT_DIR`. Also available:
  `$IMAGESMITH_PROJECT_DIR`, `$IMAGESMITH_BUILD_ID`, `$IMAGESMITH_IMAGE_NAME`,
  `$IMAGESMITH_ARCH`. The user must approve each host script before it runs.

## Don'ts

- No private keys, tokens or passwords in this folder unless the user asks; it
  is usually committed to git.
- Don't write outside this folder or into `bases/` or `builds/`.
"#;

pub struct Project {
    pub path: PathBuf,
    pub id: String,
}

impl Project {
    /// Opens a project and assigns it a stable ID in projects.json.
    pub fn open(path: &Path) -> Result<Project> {
        let path = path.canonicalize()?;
        if !path.join(RECIPE).is_file() {
            return Err(Error::Other(format!("{} has no {RECIPE}", path.display())));
        }
        let agents = path.join("AGENTS.md");
        if !agents.exists() {
            // Best effort: the folder may be read-only.
            let _ = std::fs::write(&agents, AGENTS_MD);
        }
        let id = ProjectIndex::load().id_for(&path)?;
        Ok(Project { path, id })
    }

    pub fn recipe_path(&self) -> PathBuf {
        self.path.join(RECIPE)
    }

    pub fn load_recipe(&self) -> Result<RecipeFile> {
        RecipeFile::load(&self.recipe_path())
    }

    pub fn builds_dir(&self) -> PathBuf {
        self.path.join("builds")
    }

    pub fn builds(&self) -> Vec<BuildSummary> {
        let mut out: Vec<BuildSummary> = std::fs::read_dir(self.builds_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let rec: BuildRecord = serde_json::from_slice(&std::fs::read(p.join("build.json")).ok()?).ok()?;
                Some(BuildSummary { dir: p, record: rec })
            })
            .collect();
        out.sort_by(|a, b| b.record.id.cmp(&a.record.id));
        out
    }

    /// Writes the folder skeleton and a starter recipe. Fails if a recipe exists.
    pub fn create(dir: &Path, name: &str, base: &str, arch: Arch, partition: PartitionRef) -> Result<Project> {
        if dir.join(RECIPE).exists() {
            return Err(Error::Other(format!("{} already has a {RECIPE}", dir.display())));
        }
        for sub in ["files", "overlay", "bases", "builds"] {
            std::fs::create_dir_all(dir.join(sub))?;
        }
        let gi = dir.join(".gitignore");
        if !gi.exists() {
            std::fs::write(&gi, GITIGNORE)?;
        }
        let text = recipe::starter(name, base, arch, partition);
        recipe::parse(&text)?;
        std::fs::write(dir.join(RECIPE), text)?;
        Project::open(dir)
    }
}

/// Written next to the artifacts so the sidebar can list builds without
/// replaying their event streams.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildRecord {
    pub id: String,
    pub started_at: String,
    pub duration_ms: u64,
    pub result: crate::tracker::BuildState,
}

pub struct BuildSummary {
    pub dir: PathBuf,
    pub record: BuildRecord,
}

/// "2026-10-30T1412-a1b2": sortable, filesystem-safe, unique enough.
pub fn new_build_id() -> String {
    let now = chrono::Local::now();
    let mut h = Sha256::new();
    h.update(now.timestamp_nanos_opt().unwrap_or_default().to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    let tag: String = h.finalize().iter().take(2).map(|b| format!("{b:02x}")).collect();
    format!("{}-{tag}", now.format("%Y-%m-%dT%H%M"))
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ProjectIndex {
    #[serde(default)]
    pub projects: Vec<IndexEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub path: PathBuf,
    pub id: String,
    #[serde(default)]
    pub last_build: Option<String>,
    #[serde(default)]
    pub window: Option<serde_json::Value>,
    /// Removed from the app's list; kept so the ID (and stage) survives a re-open.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

impl ProjectIndex {
    pub fn load() -> ProjectIndex {
        std::fs::read(crate::app_dirs::projects_index())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let p = crate::app_dirs::projects_index();
        std::fs::create_dir_all(p.parent().unwrap())?;
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, p)?;
        Ok(())
    }

    pub fn id_for(&mut self, path: &Path) -> Result<String> {
        if let Some(e) = self.projects.iter_mut().find(|e| e.path == path) {
            let id = e.id.clone();
            if e.hidden {
                e.hidden = false;
                self.save()?;
            }
            return Ok(id);
        }
        let mut h = Sha256::new();
        h.update(path.as_os_str().as_encoded_bytes());
        h.update(chrono::Local::now().timestamp_nanos_opt().unwrap_or_default().to_le_bytes());
        let id: String = h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect();
        self.projects.push(IndexEntry { path: path.to_path_buf(), id: id.clone(), last_build: None, window: None, hidden: false });
        self.save()?;
        Ok(id)
    }

    pub fn set_hidden(&mut self, id: &str, hidden: bool) -> Result<()> {
        if let Some(e) = self.projects.iter_mut().find(|e| e.id == id) {
            e.hidden = hidden;
            self.save()?;
        }
        Ok(())
    }

    pub fn set_last_build(&mut self, id: &str, build: &str) -> Result<()> {
        if let Some(e) = self.projects.iter_mut().find(|e| e.id == id) {
            e.last_build = Some(build.to_string());
            self.save()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_ids_are_safe() {
        let id = new_build_id();
        assert_eq!(id.len(), "2026-10-30T1412-a1b2".len());
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }
}
