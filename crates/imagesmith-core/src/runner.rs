//! Drives Apple's `container` CLI: runtime health, the builder image, and the
//! long-lived store container that every Linux-side operation runs in.

use crate::probe::Probe;
use crate::settings::Settings;
use crate::{Error, Result, BUILDER_VERSION};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const STORE: &str = "imagesmith-store";
pub const STORE_MOUNT: &str = "/store";

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeStatus {
    Ready,
    Starting,
    Unavailable(String),
}

#[derive(Debug, Clone)]
pub struct StoreInfo {
    pub running: bool,
    pub builder_version: Option<String>,
    pub cpus: Option<u32>,
    pub memory_gb: Option<u32>,
    pub rosetta: bool,
}

#[derive(Clone)]
pub struct Runtime {
    pub bin: PathBuf,
    pub cpus: u32,
    pub memory_gb: u32,
}

pub fn is_safe_id(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.') && s != "." && s != ".."
}

/// Where the Containerfile/build.sh/fsctl sources live: an explicit override,
/// the .app bundle's Resources, or the source tree.
pub fn builder_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("IMAGESMITH_BUILDER_DIR") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(res) = exe.parent().and_then(Path::parent).map(|c| c.join("Resources/builder")) {
            if res.join("Containerfile").is_file() {
                return res;
            }
        }
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../builder")
}

fn stderr_text(out: &std::process::Output) -> String {
    let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if e.is_empty() {
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    } else {
        e
    }
}

impl Runtime {
    pub fn from_settings(s: &Settings) -> Runtime {
        Runtime { bin: s.container_bin(), cpus: s.cpus, memory_gb: s.memory_gb }
    }

    pub fn cmd(&self) -> Command {
        Command::new(&self.bin)
    }

    fn output(&self, args: &[&str]) -> Result<std::process::Output> {
        self.cmd().args(args).stdin(Stdio::null()).output().map_err(|e| {
            Error::Runtime(format!("cannot run {}: {e}", self.bin.display()))
        })
    }

    fn run_ok(&self, args: &[&str]) -> Result<String> {
        let out = self.output(args)?;
        if !out.status.success() {
            return Err(Error::Runtime(format!("container {}: {}", args.join(" "), stderr_text(&out))));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub fn status(&self) -> RuntimeStatus {
        match self.output(&["system", "status"]) {
            Ok(o) if o.status.success() => RuntimeStatus::Ready,
            Ok(o) => RuntimeStatus::Unavailable(stderr_text(&o)),
            Err(e) => RuntimeStatus::Unavailable(e.to_string()),
        }
    }

    pub fn status_text(&self) -> String {
        self.output(&["system", "status"])
            .map(|o| format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
            .unwrap_or_else(|e| e.to_string())
    }

    pub fn start_system(&self) -> Result<()> {
        self.run_ok(&["system", "start"]).map(|_| ())
    }

    pub fn builder_image(&self) -> String {
        format!("imagesmith-builder:{BUILDER_VERSION}")
    }

    pub fn builder_image_present(&self) -> Result<bool> {
        let out = self.run_ok(&["image", "list", "--format", "json"])?;
        let want = self.builder_image();
        let list: Vec<Value> = serde_json::from_str(&out).unwrap_or_default();
        Ok(list.iter().filter_map(|i| i["reference"].as_str()).any(|r| r == want || r.ends_with(&format!("/{want}"))))
    }

    pub fn build_builder_image(&self, on_line: &mut dyn FnMut(&str)) -> Result<()> {
        let dir = builder_dir();
        let dir = dir.canonicalize().map_err(|e| Error::Runtime(format!("builder sources not found at {}: {e}", dir.display())))?;
        let mut child = self
            .cmd()
            .args(["build", "--progress", "plain", "-t", &self.builder_image(), "-f"])
            .arg(dir.join("Containerfile"))
            .arg(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stderr = child.stderr.take().unwrap();
        let err_thread = std::thread::spawn(move || {
            BufReader::new(stderr).lines().map_while(|l| l.ok()).collect::<Vec<_>>()
        });
        for line in BufReader::new(child.stdout.take().unwrap()).lines().map_while(|l| l.ok()) {
            on_line(&line);
        }
        let errs = err_thread.join().unwrap_or_default();
        for l in &errs {
            on_line(l);
        }
        if !child.wait()?.success() {
            return Err(Error::Runtime(format!(
                "builder image build failed: {}",
                errs.iter().rev().take(5).rev().cloned().collect::<Vec<_>>().join("\n")
            )));
        }
        Ok(())
    }

    pub fn store_info(&self) -> Result<Option<StoreInfo>> {
        let out = self.run_ok(&["inspect", STORE])?;
        let list: Vec<Value> = serde_json::from_str(&out).unwrap_or_default();
        let Some(c) = list.first() else { return Ok(None) };
        let labels = &c["configuration"]["labels"];
        let label = |k: &str| labels[k].as_str().map(String::from);
        Ok(Some(StoreInfo {
            running: c["status"].as_str() == Some("running"),
            builder_version: label("imagesmith.builder"),
            cpus: label("imagesmith.cpus").and_then(|v| v.parse().ok()),
            memory_gb: label("imagesmith.memory").and_then(|v| v.parse().ok()),
            rosetta: c["configuration"]["rosetta"].as_bool().unwrap_or(false),
        }))
    }

    fn volume_exists(&self) -> Result<bool> {
        Ok(self.output(&["volume", "inspect", STORE])?.status.success())
    }

    fn create_store(&self) -> Result<StoreInfo> {
        let cpus = self.cpus.to_string();
        let mem = format!("{}G", self.memory_gb);
        let vol = format!("{STORE}:{STORE_MOUNT}");
        let labels = [
            format!("imagesmith.builder={BUILDER_VERSION}"),
            format!("imagesmith.cpus={}", self.cpus),
            format!("imagesmith.memory={}", self.memory_gb),
        ];
        let image = self.builder_image();
        let mut args: Vec<&str> = vec!["run", "-d", "--name", STORE, "--cpus", &cpus, "--memory", &mem, "-v", &vol];
        for l in &labels {
            args.extend(["--label", l]);
        }
        // --init reaps orphans from exec'd builds; sleep is PID 1 otherwise.
        args.extend(["--init", "--entrypoint", "sleep"]);
        // Rosetta lets amd64 rootfs binaries run in the chroot; fall back if unavailable.
        let mut with_rosetta = args.clone();
        with_rosetta.push("--rosetta");
        with_rosetta.extend([image.as_str(), "infinity"]);
        if self.run_ok(&with_rosetta).is_err() {
            let _ = self.output(&["delete", "--force", STORE]);
            args.extend([image.as_str(), "infinity"]);
            self.run_ok(&args)?;
        }
        self.store_info()?.ok_or_else(|| Error::Runtime("store container did not start".into()))
    }

    /// Makes sure the builder image, the volume, and a running store container
    /// on the current builder version and resources all exist.
    pub fn ensure_store(&self, on_line: &mut dyn FnMut(&str)) -> Result<StoreInfo> {
        if let RuntimeStatus::Unavailable(msg) = self.status() {
            return Err(Error::Runtime(format!("container runtime unavailable: {msg}")));
        }
        if !self.builder_image_present()? {
            on_line(&format!("Building {} …", self.builder_image()));
            self.build_builder_image(on_line)?;
        }
        if !self.volume_exists()? {
            on_line("Creating store volume …");
            self.run_ok(&["volume", "create", STORE])?;
        }
        match self.store_info()? {
            Some(info)
                if info.builder_version.as_deref() == Some(BUILDER_VERSION)
                    && info.cpus == Some(self.cpus)
                    && info.memory_gb == Some(self.memory_gb) =>
            {
                if !info.running {
                    self.run_ok(&["start", STORE])?;
                }
                Ok(StoreInfo { running: true, ..info })
            }
            Some(_) => {
                on_line("Recreating store container (builder or resources changed) …");
                self.run_ok(&["delete", "--force", STORE])?;
                self.create_store()
            }
            None => {
                on_line("Creating store container …");
                self.create_store()
            }
        }
    }

    pub fn stop_store(&self) -> Result<()> {
        if self.store_info()?.is_some_and(|i| i.running) {
            self.run_ok(&["stop", STORE])?;
        }
        Ok(())
    }

    pub fn delete_store(&self) -> Result<()> {
        if self.store_info()?.is_some() {
            self.run_ok(&["delete", "--force", STORE])?;
        }
        Ok(())
    }

    /// Deletes the volume (all bases, caches and stages). Container goes first.
    pub fn delete_volume(&self) -> Result<()> {
        self.delete_store()?;
        if self.volume_exists()? {
            self.run_ok(&["volume", "delete", STORE])?;
        }
        Ok(())
    }

    /// `container exec [-i] imagesmith-store <args>`
    pub fn exec(&self, interactive: bool, args: &[&str]) -> Command {
        let mut c = self.cmd();
        c.arg("exec");
        if interactive {
            c.arg("-i");
        }
        c.arg(STORE).args(args);
        c
    }

    pub fn fsctl(&self, args: &[&str]) -> Result<Value> {
        let mut full = vec!["fsctl"];
        full.extend_from_slice(args);
        let out = self.exec(false, &full).stdin(Stdio::null()).output()?;
        if !out.status.success() {
            let err = stderr_text(&out);
            let msg = serde_json::from_str::<Value>(&err)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(String::from))
                .unwrap_or(err);
            return Err(Error::Runtime(format!("fsctl {}: {msg}", args.join(" "))));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let last = text.lines().rev().find(|l| !l.starts_with("@@")).unwrap_or("null");
        Ok(serde_json::from_str(last)?)
    }

    pub fn base_present(&self, sha: &str) -> Result<bool> {
        Ok(self.fsctl(&["import-base", "--sha", sha, "--check"])?["present"].as_bool() == Some(true))
    }

    /// Streams the base file into the store (first use only).
    pub fn import_base(&self, sha: &str, file: &Path, progress: &mut dyn FnMut(u64, u64)) -> Result<()> {
        if self.base_present(sha)? {
            return Ok(());
        }
        let total = std::fs::metadata(file)?.len();
        let mut child = self
            .exec(true, &["fsctl", "import-base", "--sha", sha])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        let mut f = std::fs::File::open(file)?;
        let mut buf = vec![0u8; 4 << 20];
        let mut done = 0;
        let write_result: std::io::Result<()> = (|| {
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                stdin.write_all(&buf[..n])?;
                done += n as u64;
                progress(done, total);
            }
            Ok(())
        })();
        drop(stdin);
        let out = child.wait_with_output()?;
        if !out.status.success() || write_result.is_err() {
            return Err(Error::Runtime(format!(
                "base import failed: {}",
                write_result.err().map(|e| e.to_string()).unwrap_or_else(|| stderr_text(&out))
            )));
        }
        Ok(())
    }

    /// Reads partition table, arch, distro, resize service and file caps.
    pub fn probe(&self, sha: &str) -> Result<Probe> {
        let out = self.exec(false, &["build.sh", "--probe", sha]).stdin(Stdio::null()).output()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let json = text.lines().rev().find(|l| l.starts_with('{'));
        match (out.status.success(), json) {
            (true, Some(j)) => {
                let p: Probe = serde_json::from_str(j)?;
                let _ = p.save_cached();
                Ok(p)
            }
            _ => Err(Error::Runtime(format!("probe failed: {}", stderr_text(&out)))),
        }
    }

    /// Exports one artifact to `dest`, verifying the byte count. A partial
    /// file is deleted.
    pub fn export(&self, build_id: &str, name: &str, dest: &Path, expected: u64) -> Result<()> {
        if !is_safe_id(build_id) || !is_safe_id(name) {
            return Err(Error::Other(format!("unsafe artifact name: {name}")));
        }
        let tmp = dest.with_extension("partial");
        let file = std::fs::File::create(&tmp)?;
        let status = self
            .exec(false, &["fsctl", "export", build_id, name])
            .stdin(Stdio::null())
            .stdout(file)
            .stderr(Stdio::piped())
            .status()?;
        let got = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
        if !status.success() || got != expected {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::Runtime(format!("export of {name} failed ({got} of {expected} bytes)")));
        }
        std::fs::rename(&tmp, dest)?;
        Ok(())
    }

    /// Signals the remote build's process group: TERM, then KILL after 10 s.
    pub fn kill_build(&self, build_id: &str, still_running: &mut dyn FnMut() -> bool) -> Result<()> {
        if !is_safe_id(build_id) {
            return Err(Error::Other("bad build id".into()));
        }
        let pidfile = format!("{STORE_MOUNT}/tmp/{build_id}/pid");
        let out = self.exec(false, &["cat", &pidfile]).stdin(Stdio::null()).output()?;
        let pid: u32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .map_err(|_| Error::Runtime("build has no pid file yet".into()))?;
        let group = format!("-{pid}");
        let _ = self.exec(false, &["kill", "-TERM", "--", &group]).stdin(Stdio::null()).status();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if !still_running() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        let _ = self.exec(false, &["kill", "-KILL", "--", &group]).stdin(Stdio::null()).status();
        Ok(())
    }
}
