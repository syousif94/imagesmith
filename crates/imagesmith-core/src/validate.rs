//! Recipe validation. Errors block Build; warnings don't.

use crate::app_dirs::expand_tilde;
use crate::probe::Probe;
use crate::recipe::{Arch, Compress, PasswordMode, Recipe, SizeMode, StepWhen};
use crate::size::{human, parse_percent, parse_size};
use regex::Regex;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    /// Dotted recipe path, e.g. "image.base", "users[0].name", "steps[2].run".
    pub field: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// Facts gathered outside the recipe. Each is optional: validation does
/// what it can with what's known.
#[derive(Default)]
pub struct Context<'a> {
    /// Computed SHA-256 of the base file, when available.
    pub base_sha256: Option<&'a str>,
    pub probe: Option<&'a Probe>,
    /// Whether the store container can run amd64 binaries (Rosetta).
    pub rosetta: Option<bool>,
}

static NAME_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z_][a-z0-9_-]*$").unwrap());
static STEM_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]*$").unwrap());
static HOST_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?$").unwrap());
static PKG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9+.:=~_-]*$").unwrap());
static UNIT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9:_.@\\-]+$").unwrap());
static TZ_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_+-]+(/[A-Za-z0-9_+-]+)*$").unwrap());
static LOCALE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.@-]+$").unwrap());
static OWNER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([a-z_][a-z0-9_-]*|[0-9]+):([a-z_][a-z0-9_-]*|[0-9]+)$").unwrap());

static REPO_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9._-]{0,63}$").unwrap());
static URL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^https?://[^\s\]\[]+$").unwrap());
static APT_WORD_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._/~+-]*$").unwrap());

pub const BASE_SUFFIXES: [&str; 7] = [".img", ".raw", ".img.xz", ".img.zst", ".img.gz", ".bin", ".7z"];

struct Out(Vec<Diagnostic>);

impl Out {
    fn error(&mut self, field: impl Into<String>, msg: impl Into<String>) {
        self.0.push(Diagnostic { severity: Severity::Error, field: field.into(), message: msg.into(), hint: None });
    }
    fn warn(&mut self, field: impl Into<String>, msg: impl Into<String>, hint: Option<String>) {
        self.0.push(Diagnostic { severity: Severity::Warning, field: field.into(), message: msg.into(), hint });
    }
}

pub fn resolve_project_path(project: &Path, p: &str) -> PathBuf {
    let e = expand_tilde(p);
    if e.is_absolute() {
        e
    } else {
        project.join(e)
    }
}

pub fn has_errors(d: &[Diagnostic]) -> bool {
    d.iter().any(|d| d.severity == Severity::Error)
}

pub fn validate(project: &Path, r: &Recipe, ctx: &Context) -> Vec<Diagnostic> {
    let mut o = Out(Vec::new());

    // [image]
    if !STEM_RE.is_match(&r.image.name) {
        o.error("image.name", "name must be a plain file stem (letters, digits, . _ -)");
    }
    let base = resolve_project_path(project, &r.image.base);
    let lower = r.image.base.to_lowercase();
    if !base.is_file() {
        o.error("image.base", format!("base image not found: {}", base.display()));
    } else if !BASE_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
        o.error("image.base", "base must be a raw .img or .img.xz / .img.zst / .img.gz / .7z");
    }
    if let Some(want) = &r.image.base_sha256 {
        if want.len() != 64 || !want.chars().all(|c| c.is_ascii_hexdigit()) {
            o.error("image.base_sha256", "base_sha256 must be 64 hex characters");
        } else if let Some(got) = ctx.base_sha256 {
            if !got.eq_ignore_ascii_case(want) {
                o.error("image.base_sha256", format!("checksum mismatch: file is {got}"));
            }
        }
    }

    // [size]
    let size_field = match r.size.mode {
        SizeMode::Shrink => {
            if parse_percent(&r.size.headroom).is_none_or(|p| p > 100.0) {
                o.error("size.headroom", "headroom must be a percentage like \"15%\"");
            }
            None
        }
        SizeMode::Fixed => Some(("size.fixed", &r.size.fixed)),
        SizeMode::Grow => Some(("size.grow", &r.size.grow)),
    };
    if let Some((field, value)) = size_field {
        match value.as_deref().map(parse_size) {
            None => o.error(field, format!("{field} is required for this size mode")),
            Some(None) => o.error(field, "size must look like \"8G\" or \"512M\""),
            Some(Some(0)) => o.error(field, "size must be greater than zero"),
            _ => {}
        }
    }

    // Probe-based checks.
    if let Some(p) = ctx.probe {
        match p.resolve(r.image.rootfs_partition) {
            None => o.error("image.rootfs_partition", "rootfs partition not found in the base image"),
            Some(part) => {
                if part.fstype.as_deref() != Some("ext4") {
                    o.error(
                        "image.rootfs_partition",
                        format!("partition {} is {}, not ext4", part.number, part.fstype.as_deref().unwrap_or("unknown")),
                    );
                }
                if r.size.mode != SizeMode::Fixed && !p.is_last(part.number) {
                    o.0.push(Diagnostic {
                        severity: Severity::Error,
                        field: "image.rootfs_partition".into(),
                        message: format!("partition {} is not the last partition; {:?} mode needs it last", part.number, r.size.mode).to_lowercase(),
                        hint: Some("Use size mode \"fixed\" with the original partition size".into()),
                    });
                }
                if let (SizeMode::Fixed, Some(Some(total)), Some(rootfs)) =
                    (r.size.mode, r.size.fixed.as_deref().map(parse_size), &p.rootfs)
                {
                    let min = part.start_bytes + rootfs.used_bytes;
                    if total < min {
                        o.warn(
                            "size.fixed",
                            format!("fixed size {} is smaller than the base content ({} needed)", human(total), human(min)),
                            None,
                        );
                    }
                }
            }
        }
        if let Some(info) = &p.rootfs {
            if let Some(arch) = &info.arch {
                if arch != r.image.arch.as_str() {
                    o.warn("image.arch", format!("arch is {} but the base image is {arch}", r.image.arch.as_str()), None);
                }
            }
        }
    }
    if r.image.arch == Arch::Amd64 && ctx.rosetta == Some(false) {
        o.error("image.arch", "amd64 images need Rosetta in the container runtime, which is unavailable");
    }

    // [system]
    if let Some(h) = &r.system.hostname {
        if !HOST_RE.is_match(h) {
            o.error("system.hostname", "hostname must be a valid DNS label (1–63 letters, digits, hyphens)");
        }
    }
    if let Some(tz) = &r.system.timezone {
        if !TZ_RE.is_match(tz) {
            o.error("system.timezone", "timezone must look like \"Region/City\"");
        }
    }
    if let Some(l) = &r.system.locale {
        if !LOCALE_RE.is_match(l) {
            o.error("system.locale", "locale must look like \"en_US.UTF-8\"");
        }
    }

    // [[users]]
    let mut reachable = false;
    for (i, u) in r.users.iter().enumerate() {
        if !NAME_RE.is_match(&u.name) || u.name.len() > 32 {
            o.error(format!("users[{i}].name"), format!("invalid user name {:?}", u.name));
        }
        for g in &u.groups {
            if !NAME_RE.is_match(g) || g.len() > 32 {
                o.error(format!("users[{i}].groups"), format!("invalid group name {g:?}"));
            }
        }
        for (k, key) in u.ssh_keys.iter().enumerate() {
            let path = resolve_project_path(project, key);
            match std::fs::read_to_string(&path) {
                Err(_) => o.error(format!("users[{i}].ssh_keys[{k}]"), format!("SSH key not found: {}", path.display())),
                Ok(text) if text.contains("PRIVATE KEY") => {
                    o.error(format!("users[{i}].ssh_keys[{k}]"), "this is a private key; use the .pub file")
                }
                Ok(text) if !text.lines().any(is_public_key_line) => {
                    o.error(format!("users[{i}].ssh_keys[{k}]"), "file does not contain an SSH public key")
                }
                Ok(_) => reachable = true,
            }
        }
        if u.password == PasswordMode::Prompt {
            reachable = true;
        }
    }
    if !r.users.is_empty() && !reachable {
        o.warn("users", "no SSH key and every password is locked: the image may be unreachable", None);
    }

    // [[repos]]
    let mut repo_names = std::collections::HashSet::new();
    for (i, repo) in r.repos.iter().enumerate() {
        if !REPO_RE.is_match(&repo.name) {
            o.error(format!("repos[{i}].name"), format!("invalid repository name {:?}: use lowercase letters, digits, . _ -", repo.name));
        } else if !repo_names.insert(repo.name.as_str()) {
            o.error(format!("repos[{i}].name"), format!("repository {:?} is listed twice", repo.name));
        }
        if !URL_RE.is_match(&repo.url) {
            o.error(format!("repos[{i}].url"), "url must start with http:// or https:// and contain no spaces");
        }
        if repo.suite.as_deref().is_some_and(|s| !APT_WORD_RE.is_match(s)) {
            o.error(format!("repos[{i}].suite"), "suite must be a single word such as bookworm or stable");
        }
        for c in &repo.components {
            if !APT_WORD_RE.is_match(c) {
                o.error(format!("repos[{i}].components"), format!("invalid component {c:?}"));
            }
        }
        match repo.key.as_deref() {
            None => o.warn(
                format!("repos[{i}].key"),
                format!("repository {} has no signing key; apt rejects unsigned repositories", repo.name),
                Some("Add the repository's GPG key URL or file".into()),
            ),
            Some(k) if repo.key_is_url() => {
                if !URL_RE.is_match(k) {
                    o.error(format!("repos[{i}].key"), "key URL must not contain spaces");
                }
            }
            Some(k) => {
                let path = resolve_project_path(project, k);
                if !path.is_file() {
                    o.error(format!("repos[{i}].key"), format!("key file not found: {}", path.display()));
                }
            }
        }
    }

    // [packages], [services]
    for (field, list) in [("packages.install", &r.packages.install), ("packages.remove", &r.packages.remove)] {
        for p in list {
            if !PKG_RE.is_match(p) {
                o.error(field, format!("invalid package name {p:?}"));
            }
        }
    }
    for (field, list) in [("services.enable", &r.services.enable), ("services.disable", &r.services.disable)] {
        for s in list {
            if !UNIT_RE.is_match(s) {
                o.error(field, format!("invalid unit name {s:?}"));
            }
        }
    }

    // [[files]]
    let host_first = r.host_scripts.iter().any(|s| s.enabled && s.when == StepWhen::First);
    for (i, f) in r.files.iter().enumerate() {
        let src = resolve_project_path(project, &f.src);
        if !src.is_file() && host_first {
            o.warn(format!("files[{i}].src"), format!("source not found yet: {}", src.display()), Some("A Mac script that runs first must create it".into()));
        } else if !src.is_file() {
            o.error(format!("files[{i}].src"), format!("source not found: {}", src.display()));
        }
        if !f.dst.starts_with('/') {
            o.error(format!("files[{i}].dst"), "destination must be an absolute path");
        } else if f.dst.split('/').any(|c| c == "..") {
            o.error(format!("files[{i}].dst"), "destination must not contain \"..\"");
        }
        if u32::from_str_radix(&f.mode, 8).map_or(true, |m| m > 0o7777) {
            o.error(format!("files[{i}].mode"), "mode must be octal like \"0644\"");
        }
        if !OWNER_RE.is_match(&f.owner) {
            o.error(format!("files[{i}].owner"), "owner must look like \"user:group\"");
        }
    }

    // [[steps]]
    for (i, s) in r.steps.iter().enumerate() {
        if s.name.trim().is_empty() {
            o.error(format!("steps[{i}].name"), "step needs a name");
        }
        if s.run.trim().is_empty() {
            o.error(format!("steps[{i}].run"), "step has nothing to run");
        }
    }
    for (i, s) in r.host_scripts.iter().enumerate() {
        if s.name.trim().is_empty() {
            o.error(format!("host_scripts[{i}].name"), "script needs a name");
        }
        if s.run.trim().is_empty() {
            o.error(format!("host_scripts[{i}].run"), "script has nothing to run");
        }
    }

    // [output]
    let level = r.output.effective_level();
    match r.output.compress {
        Compress::Zstd if !(1..=19).contains(&level) => o.error("output.level", "zstd level must be 1–19"),
        Compress::Xz if !(0..=9).contains(&level) => o.error("output.level", "xz level must be 0–9"),
        _ => {}
    }

    o.0
}

fn is_public_key_line(l: &str) -> bool {
    let l = l.trim();
    ["ssh-ed25519 ", "ssh-rsa ", "ecdsa-sha2-", "sk-ssh-ed25519@", "sk-ecdsa-sha2-"]
        .iter()
        .any(|p| l.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{Partition, RootfsInfo};
    use crate::recipe::parse;

    fn project() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("bases")).unwrap();
        std::fs::write(d.path().join("bases/b.img"), b"x").unwrap();
        std::fs::write(d.path().join("k.pub"), "ssh-ed25519 AAAA test\n").unwrap();
        std::fs::write(d.path().join("id"), "-----BEGIN OPENSSH PRIVATE KEY-----\n").unwrap();
        d
    }

    fn fields(d: &[Diagnostic], sev: Severity) -> Vec<String> {
        d.iter().filter(|x| x.severity == sev).map(|x| x.field.clone()).collect()
    }

    #[test]
    fn clean_recipe() {
        let p = project();
        let (_, r) = parse(
            "[image]\nname='x'\nbase='bases/b.img'\n[[users]]\nname='sammy'\nssh_keys=['k.pub']\n",
        )
        .unwrap();
        assert_eq!(validate(p.path(), &r, &Context::default()), vec![]);
    }

    #[test]
    fn catches_errors() {
        let p = project();
        let (_, r) = parse(
            r#"
[image]
name = "x/y"
base = "missing.img"
[size]
mode = "fixed"
[system]
hostname = "-bad"
[[users]]
name = "Sammy"
ssh_keys = ["id"]
[[files]]
src = "nope"
dst = "etc/x"
mode = "999"
"#,
        )
        .unwrap();
        let d = validate(p.path(), &r, &Context::default());
        let e = fields(&d, Severity::Error);
        for f in ["image.name", "image.base", "size.fixed", "system.hostname", "users[0].name",
                  "users[0].ssh_keys[0]", "files[0].src", "files[0].dst", "files[0].mode"] {
            assert!(e.contains(&f.to_string()), "missing {f}: {e:?}");
        }
        assert!(fields(&d, Severity::Warning).contains(&"users".to_string()));
    }

    #[test]
    fn probe_checks() {
        let p = project();
        let probe = Probe {
            sha256: "0".repeat(64),
            table: "gpt".into(),
            sector_size: 512,
            image_bytes: 4 << 30,
            partitions: vec![
                Partition { number: 1, start_bytes: 1 << 20, size_bytes: 256 << 20, fstype: Some("vfat".into()), label: None, uuid: None },
                Partition { number: 2, start_bytes: 257 << 20, size_bytes: 3 << 30, fstype: Some("ext4".into()), label: None, uuid: None },
            ],
            rootfs: Some(RootfsInfo {
                partition: 2,
                arch: Some("amd64".into()),
                distro: None, distro_id: None, codename: None,
                used_bytes: 2 << 30,
                resize_service: None,
                caps: vec![crate::probe::FileCap { path: "/usr/bin/ping".into(), caps: "cap_net_raw+ep".into() }],
            }),
        };
        let (_, r) = parse("[image]\nname='x'\nbase='bases/b.img'\nrootfs_partition=1\n").unwrap();
        let d = validate(p.path(), &r, &Context { probe: Some(&probe), ..Default::default() });
        assert_eq!(fields(&d, Severity::Error), vec!["image.rootfs_partition", "image.rootfs_partition"]);
        assert_eq!(fields(&d, Severity::Warning), vec!["image.arch"]);
    }
}
