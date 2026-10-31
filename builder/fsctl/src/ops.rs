//! Operations on a project's stage directory (the rootfs of its last build).

use crate::manifest;
use crate::meta::{self, Owners};
use crate::paths::{normalize, resolve, stage_dir};
use base64::Engine;
use serde_json::{json, Value};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{lchown, PermissionsExt};
use std::path::{Path, PathBuf};

pub struct Stage {
    pub root: PathBuf,
    pub owners: Owners,
}

impl Stage {
    pub fn open(project_id: &str) -> io::Result<Stage> {
        let root = stage_dir(project_id)?;
        if !root.is_dir() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "stage does not exist; build or extract first"));
        }
        Ok(Stage { owners: Owners::load(&root), root })
    }

    pub fn project_dir(&self) -> &Path {
        self.root.parent().unwrap()
    }

    fn target(&self, path: &str) -> io::Result<PathBuf> {
        let p = normalize(path);
        if p == "/" {
            return Ok(self.root.clone());
        }
        // Follow symlinks in parent components only; the leaf is the entry itself.
        resolve(&self.root, &p, false)
    }

    pub fn ls(&self, path: &str) -> io::Result<Value> {
        let dir = resolve(&self.root, path, true)?;
        let mut entries = Vec::new();
        let mut rd: Vec<_> = fs::read_dir(&dir)?.collect::<Result<_, _>>()?;
        rd.sort_by_key(|e| e.file_name());
        for e in rd {
            entries.push(meta::entry_json(&self.root, &e.path(), &self.owners)?);
        }
        Ok(json!({ "path": meta::entry_json(&self.root, &dir, &self.owners)?["path"], "entries": entries }))
    }

    pub fn stat(&self, path: &str, hash: bool) -> io::Result<Value> {
        let host = self.target(path)?;
        let mut v = meta::entry_json(&self.root, &host, &self.owners)?;
        if v["type"] == "file" {
            v["describe"] = json!(meta::describe(&host)?);
            if hash {
                v["sha256"] = json!(manifest::sha256_file(&host)?);
            }
        }
        if v["type"] == "symlink" {
            let resolved = resolve(&self.root, path, true)?;
            v["resolved"] = json!(crate::paths::rootfs_path(&self.root, &resolved));
        }
        Ok(v)
    }

    pub fn read(&self, path: &str, offset: u64, max: u64) -> io::Result<(Vec<u8>, bool)> {
        let host = resolve(&self.root, path, true)?;
        let mut f = fs::File::open(&host)?;
        let size = f.metadata()?.len();
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = Vec::new();
        f.take(max).read_to_end(&mut buf)?;
        Ok((buf, offset + max < size))
    }

    pub fn cat_json(&self, path: &str, offset: u64, max: u64) -> io::Result<Value> {
        let (buf, truncated) = self.read(path, offset, max)?;
        Ok(json!({
            "text": meta::is_text(&buf),
            "truncated": truncated,
            "content_b64": base64::engine::general_purpose::STANDARD.encode(&buf),
        }))
    }

    pub fn cat_raw(&self, path: &str) -> io::Result<()> {
        let host = resolve(&self.root, path, true)?;
        let mut f = fs::File::open(host)?;
        let mut out = io::stdout().lock();
        io::copy(&mut f, &mut out)?;
        out.flush()
    }

    fn ensure_parents(&self, host: &Path) -> io::Result<()> {
        let parent = host.parent().unwrap();
        if !parent.exists() {
            self.ensure_parents(parent)?;
            fs::create_dir(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o755))?;
            lchown(parent, Some(0), Some(0))?;
        }
        Ok(())
    }

    pub fn put(&self, path: &str, content: &[u8], mode: Option<u32>, owner: Option<&str>, dir: bool) -> io::Result<Value> {
        let host = self.target(path)?;
        if host == self.root {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "cannot write /"));
        }
        let (uid, gid) = match owner {
            Some(o) => self.owners.parse_owner(o)?,
            None => fs::symlink_metadata(&host)
                .map(|m| {
                    use std::os::unix::fs::MetadataExt;
                    (m.uid(), m.gid())
                })
                .unwrap_or((0, 0)),
        };
        let mode = match mode {
            Some(m) => m,
            None => fs::symlink_metadata(&host)
                .map(|m| m.permissions().mode() & 0o7777)
                .unwrap_or(if dir { 0o755 } else { 0o644 }),
        };
        self.ensure_parents(&host)?;
        if dir {
            if !host.is_dir() {
                fs::create_dir(&host)?;
            }
        } else {
            let tmp = host.with_file_name(format!(
                ".fsctl-put-{}-{}",
                std::process::id(),
                host.file_name().unwrap().to_string_lossy()
            ));
            fs::write(&tmp, content)?;
            fs::rename(&tmp, &host)?;
        }
        lchown(&host, Some(uid), Some(gid))?;
        // chmod after chown: chown clears setuid/setgid bits.
        fs::set_permissions(&host, fs::Permissions::from_mode(mode))?;
        meta::entry_json(&self.root, &host, &self.owners)
    }

    pub fn rm(&self, path: &str) -> io::Result<Value> {
        let host = self.target(path)?;
        if host == self.root {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "refusing to remove /"));
        }
        let m = fs::symlink_metadata(&host)?;
        if m.is_dir() {
            fs::remove_dir_all(&host)?;
        } else {
            fs::remove_file(&host)?;
        }
        Ok(json!({ "removed": normalize(path) }))
    }

    pub fn find(&self, pattern: &str, under: &str, limit: usize) -> io::Result<Value> {
        let start = resolve(&self.root, under, true)?;
        let pat = pattern.to_lowercase();
        let mut results = Vec::new();
        let mut stack = vec![start];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_lowercase();
                let ft = e.file_type()?;
                if glob_match(&pat, &name) {
                    results.push(meta::entry_json(&self.root, &e.path(), &self.owners)?);
                    if results.len() >= limit {
                        return Ok(json!({ "results": results, "truncated": true }));
                    }
                }
                if ft.is_dir() {
                    stack.push(e.path());
                }
            }
        }
        results.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        Ok(json!({ "results": results, "truncated": false }))
    }

    /// Changes in the stage since the last build finished (Shell capture).
    pub fn diff_since_build(&self) -> io::Result<Value> {
        let post = manifest::read(&self.project_dir().join("post.manifest"))?;
        let now = manifest::generate(&self.root)?;
        Ok(json!({ "changes": manifest::diff(&post, &now, None) }))
    }

    /// Changes from the untouched base to the last build (change badges).
    pub fn diff_from_base(&self, prefix: Option<&str>) -> io::Result<Value> {
        let base = manifest::read(&self.project_dir().join("base.manifest"))?;
        let post = manifest::read(&self.project_dir().join("post.manifest"))?;
        Ok(json!({ "changes": manifest::diff(&base, &post, prefix) }))
    }
}

/// Glob with `*` and `?`; a pattern without wildcards matches as a substring.
pub fn glob_match(pat: &str, name: &str) -> bool {
    if !pat.contains(['*', '?']) {
        return name.contains(pat);
    }
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni, mut star, mut mark) = (0, 0, None, 0);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn globs() {
        assert!(glob_match("*.service", "app.service"));
        assert!(!glob_match("*.service", "app.socket"));
        assert!(glob_match("ssh", "sshd_config"));
        assert!(glob_match("a?c*", "abcdef"));
    }
}
