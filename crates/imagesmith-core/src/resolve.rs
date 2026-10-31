//! Turns a validated recipe into the self-contained recipe.json and input
//! tar that build.sh consumes, plus the cache key and lock file.

use crate::probe::Probe;
use crate::recipe::{Checksum, PasswordMode, Recipe, SizeMode, StepWhen};
use crate::size::{parse_percent, parse_size};
use crate::validate::resolve_project_path;
use crate::{Error, Result, BUILDER_VERSION};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct Resolved {
    pub builder_version: String,
    pub project_id: String,
    pub build_id: String,
    pub base_sha256: String,
    pub cache_key: String,
    pub name: String,
    pub output_stem: String,
    pub arch: String,
    pub rootfs_partition: u32,
    pub size: ResolvedSize,
    pub system: crate::recipe::System,
    pub users: Vec<ResolvedUser>,
    pub repos: Vec<ResolvedRepo>,
    pub packages: crate::recipe::Packages,
    pub services: crate::recipe::Services,
    pub files: Vec<ResolvedFile>,
    pub first_steps: Vec<ResolvedStep>,
    pub steps: Vec<ResolvedStep>,
    pub output: ResolvedOutput,
}

#[derive(Debug, Serialize)]
pub struct ResolvedSize {
    pub mode: SizeMode,
    pub headroom_pct: f64,
    pub fixed_bytes: Option<u64>,
    pub grow_bytes: Option<u64>,
    pub expand_on_boot: bool,
}

#[derive(Debug, Serialize)]
pub struct ResolvedUser {
    pub name: String,
    pub groups: Vec<String>,
    /// Public key lines, read on the host.
    pub ssh_keys: Vec<String>,
    pub password: PasswordMode,
}

#[derive(Debug, Serialize)]
pub struct ResolvedFile {
    /// Path inside the input tar.
    pub src: String,
    pub dst: String,
    pub mode: String,
    pub owner: String,
}

#[derive(Debug, Serialize)]
pub struct ResolvedRepo {
    pub name: String,
    pub url: String,
    pub suite: Option<String>,
    pub components: Vec<String>,
    pub key_url: Option<String>,
    /// Path inside the input tar.
    pub key_file: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ResolvedStep {
    pub name: String,
    pub run: String,
}

#[derive(Debug, Serialize)]
pub struct ResolvedOutput {
    pub compress: crate::recipe::Compress,
    pub level: i32,
    pub bmap: bool,
    pub checksum: bool,
}

pub fn enabled_steps(r: &Recipe, when: StepWhen) -> Vec<ResolvedStep> {
    r.steps.iter().filter(|s| s.enabled && s.when == when).map(|s| ResolvedStep { name: s.name.clone(), run: s.run.clone() }).collect()
}

pub fn cache_key(base_sha: &str, partition: u32) -> String {
    let mut h = Sha256::new();
    h.update(format!("{base_sha}:{partition}:{BUILDER_VERSION}"));
    h.finalize().iter().take(16).map(|b| format!("{b:02x}")).collect()
}

pub fn resolve(
    project: &Path,
    project_id: &str,
    build_id: &str,
    r: &Recipe,
    base_sha: &str,
    probe: &Probe,
) -> Result<Resolved> {
    let part = probe
        .resolve(r.image.rootfs_partition)
        .ok_or_else(|| Error::Other("rootfs partition not found in base image".into()))?
        .number;
    let mut users = Vec::new();
    for u in &r.users {
        let mut keys = Vec::new();
        for k in &u.ssh_keys {
            let text = std::fs::read_to_string(resolve_project_path(project, k))?;
            keys.extend(text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(String::from));
        }
        users.push(ResolvedUser { name: u.name.clone(), groups: u.groups.clone(), ssh_keys: keys, password: u.password });
    }
    let date = chrono::Local::now().format("%Y%m%d");
    Ok(Resolved {
        builder_version: BUILDER_VERSION.into(),
        project_id: project_id.into(),
        build_id: build_id.into(),
        base_sha256: base_sha.into(),
        cache_key: cache_key(base_sha, part),
        name: r.image.name.clone(),
        output_stem: format!("{}-{date}", r.image.name),
        arch: r.image.arch.as_str().into(),
        rootfs_partition: part,
        size: ResolvedSize {
            mode: r.size.mode,
            headroom_pct: parse_percent(&r.size.headroom).unwrap_or(15.0),
            fixed_bytes: r.size.fixed.as_deref().and_then(parse_size),
            grow_bytes: r.size.grow.as_deref().and_then(parse_size),
            expand_on_boot: r.size.expand_on_boot,
        },
        system: r.system.clone(),
        users,
        repos: r
            .repos
            .iter()
            .enumerate()
            .map(|(i, x)| ResolvedRepo {
                name: x.name.clone(),
                url: x.url.clone(),
                suite: x.suite.clone(),
                components: x.components.clone(),
                key_url: x.key.clone().filter(|_| x.key_is_url()),
                key_file: x.key.as_ref().filter(|_| !x.key_is_url()).map(|_| format!("inputs/repos/{i}.key")),
            })
            .collect(),
        packages: r.packages.clone(),
        services: r.services.clone(),
        files: r
            .files
            .iter()
            .enumerate()
            .map(|(i, f)| ResolvedFile { src: format!("inputs/files/{i}"), dst: f.dst.clone(), mode: f.mode.clone(), owner: f.owner.clone() })
            .collect(),
        first_steps: enabled_steps(r, StepWhen::First),
        steps: enabled_steps(r, StepWhen::Last),
        output: ResolvedOutput {
            compress: r.output.compress,
            level: r.output.effective_level(),
            bmap: r.output.bmap,
            checksum: r.output.checksum == Checksum::Sha256,
        },
    })
}

/// Writes the input tar (recipe.json, inputs/files/N, overlay/) to `out`.
/// Everything is owned by root in the archive; overlay modes are kept.
pub fn write_input_tar(project: &Path, r: &Recipe, resolved: &Resolved, out: impl Write) -> Result<()> {
    let mut tar = tar::Builder::new(out);
    tar.follow_symlinks(false);
    // Complete keeps overlay modes; build.sh extracts with --no-same-owner.
    tar.mode(tar::HeaderMode::Complete);

    let json = serde_json::to_vec_pretty(resolved)?;
    let mut h = tar::Header::new_gnu();
    h.set_size(json.len() as u64);
    h.set_mode(0o600);
    h.set_cksum();
    tar.append_data(&mut h, "recipe.json", json.as_slice())?;

    for (i, f) in r.files.iter().enumerate() {
        let src = resolve_project_path(project, &f.src);
        let mut file = std::fs::File::open(&src)?;
        tar.append_file(format!("inputs/files/{i}"), &mut file)?;
    }
    for (i, repo) in r.repos.iter().enumerate() {
        if let Some(k) = repo.key.as_deref().filter(|_| !repo.key_is_url()) {
            let mut file = std::fs::File::open(resolve_project_path(project, k))?;
            tar.append_file(format!("inputs/repos/{i}.key"), &mut file)?;
        }
    }
    let overlay = project.join("overlay");
    if overlay.is_dir() {
        tar.append_dir_all("overlay", &overlay)?;
    }
    tar.into_inner()?.flush()?;
    Ok(())
}

#[derive(Serialize)]
pub struct Lock<'a> {
    pub recipe: &'a Recipe,
    pub resolved: &'a Resolved,
    pub base_sha256: &'a str,
    pub builder_version: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::Partition;

    #[test]
    fn resolves_and_tars() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("files")).unwrap();
        std::fs::create_dir_all(d.path().join("overlay/etc")).unwrap();
        std::fs::write(d.path().join("files/a"), "A").unwrap();
        std::fs::write(d.path().join("overlay/etc/motd"), "hi").unwrap();
        std::fs::write(d.path().join("k.pub"), "# c\nssh-ed25519 AAAA x\n").unwrap();
        std::fs::write(d.path().join("repo.gpg"), "KEY").unwrap();
        let (_, r) = crate::recipe::parse(
            "[image]\nname='x'\nbase='b.img'\n[[users]]\nname='u'\nssh_keys=['k.pub']\n[[repos]]\nname='a'\nurl='https://a'\nkey='https://a/k'\n[[repos]]\nname='b'\nurl='https://b'\nkey='repo.gpg'\n[[files]]\nsrc='files/a'\ndst='/a'\n[[steps]]\nname='off'\nrun='x'\nenabled=false\n",
        )
        .unwrap();
        let probe = Probe {
            sha256: "a".repeat(64),
            table: "dos".into(),
            sector_size: 512,
            image_bytes: 1,
            partitions: vec![Partition { number: 2, start_bytes: 0, size_bytes: 1, fstype: Some("ext4".into()), label: None, uuid: None }],
            rootfs: None,
        };
        let res = resolve(d.path(), "p", "b", &r, &probe.sha256, &probe).unwrap();
        assert_eq!(res.rootfs_partition, 2);
        assert_eq!(res.users[0].ssh_keys, vec!["ssh-ed25519 AAAA x"]);
        assert!(res.steps.is_empty());
        assert_eq!(res.repos[0].key_url.as_deref(), Some("https://a/k"));
        assert_eq!(res.repos[1].key_file.as_deref(), Some("inputs/repos/1.key"));
        let mut buf = Vec::new();
        write_input_tar(d.path(), &r, &res, &mut buf).unwrap();
        let mut ar = tar::Archive::new(buf.as_slice());
        let names: Vec<String> =
            ar.entries().unwrap().map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned()).collect();
        assert!(names.contains(&"recipe.json".to_string()));
        assert!(names.contains(&"inputs/files/0".to_string()));
        assert!(names.contains(&"inputs/repos/1.key".to_string()));
        assert!(names.contains(&"overlay/etc/motd".to_string()));
    }
}
