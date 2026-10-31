//! recipe.toml: the source of truth for a project.

use crate::{Error, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub image: Image,
    #[serde(default)]
    pub size: Size,
    #[serde(default)]
    pub system: System,
    #[serde(default)]
    pub users: Vec<User>,
    #[serde(default)]
    pub repos: Vec<Repo>,
    #[serde(default)]
    pub packages: Packages,
    #[serde(default)]
    pub services: Services,
    #[serde(default)]
    pub files: Vec<FileEntry>,
    #[serde(default)]
    pub steps: Vec<Step>,
    /// Scripts run on the Mac in the project folder: `first` before the build, `last` after export.
    #[serde(default)]
    pub host_scripts: Vec<Step>,
    #[serde(default)]
    pub output: Output,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Image {
    pub name: String,
    pub base: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha256: Option<String>,
    #[serde(default)]
    pub arch: Arch,
    #[serde(default)]
    pub rootfs_partition: PartitionRef,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    #[default]
    Arm64,
    Amd64,
}

impl Arch {
    pub fn as_str(self) -> &'static str {
        match self {
            Arch::Arm64 => "arm64",
            Arch::Amd64 => "amd64",
        }
    }
}

/// 1-based partition number, or `"auto"` for the last ext4 partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PartitionRef {
    #[default]
    Auto,
    Number(u32),
}

impl Serialize for PartitionRef {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            PartitionRef::Auto => s.serialize_str("auto"),
            PartitionRef::Number(n) => s.serialize_u32(*n),
        }
    }
}

impl<'de> Deserialize<'de> for PartitionRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            N(i64),
            S(String),
        }
        match Raw::deserialize(d)? {
            Raw::N(n) if (1..=128).contains(&n) => Ok(PartitionRef::Number(n as u32)),
            Raw::N(n) => Err(serde::de::Error::custom(format!("rootfs_partition {n} is out of range 1..128"))),
            Raw::S(s) if s == "auto" => Ok(PartitionRef::Auto),
            Raw::S(s) => Err(serde::de::Error::custom(format!("rootfs_partition must be a number or \"auto\", got {s:?}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SizeMode {
    #[default]
    Shrink,
    Fixed,
    Grow,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Size {
    #[serde(default)]
    pub mode: SizeMode,
    #[serde(default = "default_headroom")]
    pub headroom: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grow: Option<String>,
    #[serde(default = "yes")]
    pub expand_on_boot: bool,
}

impl Default for Size {
    fn default() -> Self {
        Size { mode: SizeMode::Shrink, headroom: default_headroom(), fixed: None, grow: None, expand_on_boot: true }
    }
}

fn default_headroom() -> String {
    "15%".into()
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct System {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    #[serde(default)]
    pub reset_machine_id: bool,
    #[serde(default)]
    pub reset_ssh_host_keys: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PasswordMode {
    #[default]
    Locked,
    Prompt,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    pub name: String,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub ssh_keys: Vec<String>,
    #[serde(default)]
    pub password: PasswordMode,
}

/// An extra apt source, written to /etc/apt/sources.list.d before packages install.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repo {
    pub name: String,
    pub url: String,
    /// Defaults to the image's release codename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite: Option<String>,
    #[serde(default)]
    pub components: Vec<String>,
    /// Signing key: an http(s) URL or a project-relative file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

impl Repo {
    pub fn key_is_url(&self) -> bool {
        self.key.as_deref().is_some_and(|k| k.starts_with("https://") || k.starts_with("http://"))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Packages {
    #[serde(default)]
    pub install: Vec<String>,
    #[serde(default)]
    pub remove: Vec<String>,
    #[serde(default = "yes")]
    pub apt_update: bool,
}

impl Default for Packages {
    fn default() -> Self {
        Packages { install: vec![], remove: vec![], apt_update: true }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Services {
    #[serde(default)]
    pub enable: Vec<String>,
    #[serde(default)]
    pub disable: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    pub src: String,
    pub dst: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_owner")]
    pub owner: String,
}

fn default_mode() -> String {
    "0644".into()
}

fn default_owner() -> String {
    "root:root".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub name: String,
    pub run: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "StepWhen::is_last")]
    pub when: StepWhen,
}

/// When a step runs: before anything else touches the image, or after everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepWhen {
    First,
    #[default]
    Last,
}

impl StepWhen {
    fn is_last(&self) -> bool {
        *self == StepWhen::Last
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Compress {
    None,
    #[default]
    Zstd,
    Xz,
}

impl Compress {
    pub fn extension(self) -> &'static str {
        match self {
            Compress::None => "",
            Compress::Zstd => ".zst",
            Compress::Xz => ".xz",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Checksum {
    None,
    #[default]
    Sha256,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Output {
    #[serde(default)]
    pub compress: Compress,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<i32>,
    #[serde(default = "yes")]
    pub bmap: bool,
    #[serde(default)]
    pub checksum: Checksum,
}

impl Default for Output {
    fn default() -> Self {
        Output { compress: Compress::Zstd, level: None, bmap: true, checksum: Checksum::Sha256 }
    }
}

impl Output {
    pub fn effective_level(&self) -> i32 {
        self.level.unwrap_or(match self.compress {
            Compress::Xz => 6,
            _ => 19,
        })
    }
}

/// Parsed recipe plus the editable document, so GUI edits keep comments.
pub struct RecipeFile {
    pub path: PathBuf,
    pub doc: toml_edit::DocumentMut,
    pub recipe: Recipe,
}

impl RecipeFile {
    pub fn load(path: &Path) -> Result<RecipeFile> {
        let text = std::fs::read_to_string(path)?;
        let (doc, recipe) = parse(&text)?;
        Ok(RecipeFile { path: path.to_path_buf(), doc, recipe })
    }

    /// Set a value by dotted path in the document (e.g. `["system", "hostname"]`),
    /// re-parse, and save. Comments and ordering elsewhere are untouched.
    pub fn set(&mut self, path: &[&str], value: toml_edit::Item) -> Result<()> {
        let (last, parents) = path.split_last().ok_or_else(|| Error::Other("empty path".into()))?;
        let mut table = self.doc.as_table_mut() as &mut toml_edit::Table;
        for key in parents {
            table = table
                .entry(key)
                .or_insert(toml_edit::table())
                .as_table_mut()
                .ok_or_else(|| Error::Other(format!("{key} is not a table")))?;
        }
        table.insert(last, value);
        self.commit()
    }

    /// Applies an arbitrary document edit, re-parses, and saves. The edit is
    /// discarded if the result no longer parses as a recipe.
    pub fn update(&mut self, f: impl FnOnce(&mut toml_edit::DocumentMut)) -> Result<()> {
        let mut doc = self.doc.clone();
        f(&mut doc);
        let (_, recipe) = parse(&doc.to_string())?;
        self.doc = doc;
        self.recipe = recipe;
        std::fs::write(&self.path, self.doc.to_string())?;
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        let text = self.doc.to_string();
        let (doc, recipe) = parse(&text)?;
        self.doc = doc;
        self.recipe = recipe;
        std::fs::write(&self.path, text)?;
        Ok(())
    }
}

pub fn parse(text: &str) -> Result<(toml_edit::DocumentMut, Recipe)> {
    let line_of = |span: Option<std::ops::Range<usize>>| {
        span.map(|r| text[..r.start.min(text.len())].matches('\n').count() + 1).unwrap_or(0)
    };
    let doc: toml_edit::DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| Error::RecipeParse {
        line: line_of(e.span()),
        message: e.message().to_string(),
    })?;
    let recipe: Recipe = toml_edit::de::from_str(text).map_err(|e| Error::RecipeParse {
        line: line_of(e.span()),
        message: e.message().to_string(),
    })?;
    Ok((doc, recipe))
}

pub fn starter(name: &str, base: &str, arch: Arch, partition: PartitionRef) -> String {
    let part = match partition {
        PartitionRef::Auto => "\"auto\"".to_string(),
        PartitionRef::Number(n) => n.to_string(),
    };
    format!(
        r#"[image]
name = "{name}"  # output file stem
base = "{base}"
arch = "{arch}"  # arm64 | amd64
rootfs_partition = {part}  # 1-based; "auto" = last ext4 partition

[size]
mode = "shrink"  # shrink | fixed | grow
headroom = "15%"
expand_on_boot = true

[system]
hostname = "{name}"
reset_machine_id = true
reset_ssh_host_keys = true

[packages]
install = []
remove = []
apt_update = true

[services]
enable = []
disable = []

[output]
compress = "zstd"  # none | zstd | xz
level = 19
bmap = true
checksum = "sha256"
"#,
        arch = arch.as_str(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const SAMPLE: &str = r#"
[image]
name = "rk3588-edge"
base = "bases/armbian-rk3588.img.xz"
arch = "arm64"
rootfs_partition = 2

[size]
mode = "shrink"
headroom = "15%"
expand_on_boot = true

[system]
hostname = "edge-01"
timezone = "America/Los_Angeles"
locale = "en_US.UTF-8"
reset_machine_id = true
reset_ssh_host_keys = true

[[users]]
name = "sammy"
groups = ["sudo", "dialout"]
ssh_keys = ["~/.ssh/id_ed25519.pub"]
password = "locked"

[[repos]]
name = "docker"
url = "https://download.docker.com/linux/debian"
components = ["stable"]
key = "https://download.docker.com/linux/debian/gpg"

[packages]
install = ["htop", "can-utils", "python3-venv"]
remove = ["unattended-upgrades"]

[services]
enable = ["ssh"]
disable = ["bluetooth"]

[[files]]
src = "files/app.service"
dst = "/etc/systemd/system/app.service"
mode = "0644"
owner = "root:root"

[[steps]]
name = "Restore ping caps"
run = "setcap cap_net_raw+ep /usr/bin/ping"

[[steps]]
name = "Pin mirror"
run = "true"
when = "first"

[output]
compress = "zstd"
level = 19
"#;

    #[test]
    fn parses_spec_sample() {
        let (_, r) = parse(SAMPLE).unwrap();
        assert_eq!(r.image.rootfs_partition, PartitionRef::Number(2));
        assert_eq!(r.users[0].groups, vec!["sudo", "dialout"]);
        assert!(r.output.bmap);
        assert!(r.packages.apt_update);
        assert!(r.steps[0].enabled);
        assert_eq!((r.steps[0].when, r.steps[1].when), (StepWhen::Last, StepWhen::First));
        assert_eq!(r.repos[0].components, vec!["stable"]);
        assert!(r.repos[0].suite.is_none() && r.repos[0].key_is_url());
    }

    #[test]
    fn auto_partition_and_errors() {
        let (_, r) = parse("[image]\nname='a'\nbase='b.img'\nrootfs_partition='auto'\n").unwrap();
        assert_eq!(r.image.rootfs_partition, PartitionRef::Auto);
        match parse("[image]\nname='a'\nbase='b.img'\nbogus=1\n") {
            Err(Error::RecipeParse { line, .. }) => assert_eq!(line, 4),
            other => panic!("expected parse error, got {:?}", other.map(|(_, r)| r)),
        }
    }

    #[test]
    fn set_keeps_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recipe.toml");
        std::fs::write(&path, SAMPLE.replace("[system]", "# keep me\n[system]")).unwrap();
        let mut f = RecipeFile::load(&path).unwrap();
        f.set(&["system", "hostname"], toml_edit::value("edge-02")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep me"));
        assert_eq!(f.recipe.system.hostname.as_deref(), Some("edge-02"));
    }

    #[test]
    fn starter_parses() {
        parse(&starter("x", "bases/x.img", Arch::Arm64, PartitionRef::Number(2))).unwrap();
    }
}
