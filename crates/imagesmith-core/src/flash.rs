//! Writing a built image to a removable device (SD card / USB stick).
//!
//! Flashing needs root, so the work runs in a privileged copy of this same
//! executable (`--flash-writer`). The parent picks how to elevate — `sudo` for
//! the CLI, an authorization prompt for the app — and reads JSON-line progress
//! back from the helper. Decompression, writing, verification and ejecting all
//! happen inside the helper so only one privileged process is involved.

use crate::build::CancelToken;
use crate::size::human;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Argument that turns a normal invocation into the privileged writer.
pub const HELPER_FLAG: &str = "--flash-writer";

/// Chunk size for reads and writes; also the progress granularity.
const CHUNK: usize = 4 << 20;
/// Raw devices on macOS only accept block-aligned writes; 4096 covers both
/// 512-byte and 4096-byte logical sectors.
const ALIGN: u64 = 4096;

// ------------------------------------------------------------------ devices

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    /// "disk4" on macOS, "sdb" on Linux.
    pub id: String,
    /// Block device node, e.g. /dev/disk4 or /dev/sdb.
    pub node: PathBuf,
    pub name: String,
    pub size_bytes: u64,
    pub removable: bool,
    pub internal: bool,
    pub writable: bool,
    /// Backed by a disk image or RAM rather than real media; targetable only
    /// when asked for explicitly.
    #[serde(default)]
    pub virtual_media: bool,
    /// Bus protocol ("USB", "SDXC", "nvme", …) when known.
    pub bus: Option<String>,
    /// Mount points of the device's partitions.
    pub mounts: Vec<String>,
}

impl Device {
    /// True when the device holds the running system; never flashed.
    pub fn is_system(&self) -> bool {
        self.mounts.iter().any(|m| m == "/" || m == "/boot" || m == "/boot/efi" || m.starts_with("/System/Volumes"))
    }

    /// Flashing this device would be destructive in a way we refuse by default.
    pub fn is_protected(&self) -> bool {
        self.is_system() || !self.writable || (self.internal && !self.removable)
    }

    /// "disk4 · SanDisk Ultra · 29.7 GB · USB"
    pub fn label(&self) -> String {
        let mut parts = vec![self.id.clone()];
        if !self.name.is_empty() {
            parts.push(self.name.clone());
        }
        parts.push(human(self.size_bytes));
        parts.extend(self.bus.clone());
        parts.join(" · ")
    }
}

/// Candidate targets, removable first. `include_internal` keeps fixed disks in
/// the list (still flagged via [`Device::is_protected`]).
pub fn list_devices(include_internal: bool) -> Result<Vec<Device>> {
    let mut all = if cfg!(target_os = "macos") { list_macos()? } else { list_linux()? };
    all.retain(|d| d.size_bytes > 0 && (include_internal || (d.removable && !d.virtual_media && !d.is_protected())));
    all.sort_by(|a, b| b.removable.cmp(&a.removable).then(a.id.cmp(&b.id)));
    Ok(all)
}

/// Looks up one device by node ("/dev/disk4"), id ("disk4") or a partition node.
pub fn find_device(spec: &str) -> Result<Device> {
    let want = whole_disk_id(spec);
    list_devices(true)?
        .into_iter()
        .find(|d| d.id == want || d.node.as_os_str() == std::ffi::OsStr::new(spec))
        .ok_or_else(|| Error::Other(format!("no such device: {spec}")))
}

/// "/dev/rdisk4s1" -> "disk4"; "/dev/sdb2" -> "sdb".
fn whole_disk_id(spec: &str) -> String {
    let base = spec.rsplit('/').next().unwrap_or(spec);
    let base = base.strip_prefix('r').filter(|b| b.starts_with("disk")).unwrap_or(base);
    if let Some(rest) = base.strip_prefix("disk") {
        // macOS: diskNsM -> diskN
        let n: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !n.is_empty() {
            return format!("disk{n}");
        }
    }
    // Linux: sdb2 -> sdb, nvme0n1p3 -> nvme0n1, mmcblk0p1 -> mmcblk0
    if base.starts_with("nvme") || base.starts_with("mmcblk") || base.starts_with("loop") {
        if let Some(i) = base.rfind('p') {
            if base[i + 1..].chars().all(|c| c.is_ascii_digit()) && !base[i + 1..].is_empty() {
                return base[..i].to_string();
            }
        }
        return base.to_string();
    }
    base.trim_end_matches(|c: char| c.is_ascii_digit()).to_string()
}

// -- macOS ----------------------------------------------------------------

/// Runs a command that emits a plist and converts it to JSON with `plutil`.
fn plist_json(program: &str, args: &[&str]) -> Result<serde_json::Value> {
    let out = Command::new(program).args(args).stderr(Stdio::null()).output()?;
    if !out.status.success() {
        return Err(Error::Other(format!("{program} {} failed", args.join(" "))));
    }
    let mut p = Command::new("plutil")
        .args(["-convert", "json", "-o", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = p.stdin.take().expect("piped");
    let bytes = out.stdout;
    std::thread::spawn(move || {
        let _ = stdin.write_all(&bytes);
    });
    let o = p.wait_with_output()?;
    if !o.status.success() {
        return Err(Error::Other("plutil could not read diskutil output".into()));
    }
    Ok(serde_json::from_slice(&o.stdout)?)
}

fn list_macos() -> Result<Vec<Device>> {
    let list = plist_json("diskutil", &["list", "-plist"])?;
    let mut mounts: std::collections::HashMap<String, Vec<String>> = Default::default();
    let mut synthesized: std::collections::HashSet<String> = Default::default();
    for d in list["AllDisksAndPartitions"].as_array().into_iter().flatten() {
        let Some(id) = d["DeviceIdentifier"].as_str() else { continue };
        let mut add = |disk: &str, m: &str| {
            let e: &mut Vec<String> = mounts.entry(disk.to_string()).or_default();
            if !e.iter().any(|x| x == m) {
                e.push(m.to_string());
            }
        };
        if let Some(m) = d["MountPoint"].as_str().filter(|m| !m.is_empty()) {
            add(id, m);
        }
        for p in d["Partitions"].as_array().into_iter().flatten() {
            if let Some(m) = p["MountPoint"].as_str().filter(|m| !m.is_empty()) {
                add(id, m);
            }
        }
        // A synthesized APFS container carries the mounts, but the disk a user
        // would erase is the physical store behind it.
        let backing: Vec<String> = d["APFSPhysicalStores"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s["DeviceIdentifier"].as_str())
            .map(whole_disk_id)
            .collect();
        if !backing.is_empty() {
            synthesized.insert(id.to_string());
        }
        for v in d["APFSVolumes"].as_array().into_iter().flatten() {
            if let Some(m) = v["MountPoint"].as_str().filter(|m| !m.is_empty()) {
                add(id, m);
                for b in &backing {
                    add(b, m);
                }
            }
        }
    }
    let mut out = Vec::new();
    for id in list["WholeDisks"].as_array().into_iter().flatten().filter_map(|v| v.as_str()) {
        let Ok(i) = plist_json("diskutil", &["info", "-plist", id]) else { continue };
        // A synthesized APFS container is just a view onto its physical store,
        // never something to erase.
        if synthesized.contains(id) || i["SystemImage"].as_bool() == Some(true) {
            continue;
        }
        let b = |k: &str| i[k].as_bool().unwrap_or(false);
        let internal = b("Internal");
        let removable = b("Ejectable") || b("Removable") || b("RemovableMedia") || b("RemovableMediaOrExternalDevice") || !internal;
        out.push(Device {
            id: id.to_string(),
            node: PathBuf::from(i["DeviceNode"].as_str().unwrap_or(&format!("/dev/{id}")).to_string()),
            name: i["MediaName"].as_str().or_else(|| i["IORegistryEntryName"].as_str()).unwrap_or("").trim().to_string(),
            size_bytes: i["TotalSize"].as_u64().or_else(|| i["Size"].as_u64()).unwrap_or(0),
            removable,
            internal,
            writable: i["WritableMedia"].as_bool().unwrap_or(true),
            virtual_media: i["VirtualOrPhysical"].as_str() == Some("Virtual") || i["BusProtocol"].as_str() == Some("Disk Image"),
            bus: i["BusProtocol"].as_str().map(str::to_string),
            mounts: mounts.remove(id).unwrap_or_default(),
        });
    }
    Ok(out)
}

// -- Linux ----------------------------------------------------------------

fn list_linux() -> Result<Vec<Device>> {
    let out = Command::new("lsblk")
        .args(["--json", "--bytes", "--output", "NAME,PATH,SIZE,TYPE,RM,RO,HOTPLUG,MODEL,VENDOR,TRAN,MOUNTPOINT"])
        .stderr(Stdio::null())
        .output()
        .map_err(|e| Error::Other(format!("lsblk is required to list devices: {e}")))?;
    if !out.status.success() {
        return Err(Error::Other("lsblk failed".into()));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    fn collect_mounts(n: &serde_json::Value, into: &mut Vec<String>) {
        if let Some(m) = n["mountpoint"].as_str().filter(|m| !m.is_empty()) {
            into.push(m.to_string());
        }
        for c in n["children"].as_array().into_iter().flatten() {
            collect_mounts(c, into);
        }
    }
    let mut out_devs = Vec::new();
    for d in v["blockdevices"].as_array().into_iter().flatten() {
        let Some(name) = d["name"].as_str() else { continue };
        if d["type"].as_str() != Some("disk") || ["loop", "ram", "zram", "dm-", "sr"].iter().any(|p| name.starts_with(p)) {
            continue;
        }
        let mut mounts = Vec::new();
        collect_mounts(d, &mut mounts);
        let removable = d["rm"].as_bool().unwrap_or(false) || d["hotplug"].as_bool().unwrap_or(false);
        let name_parts: Vec<String> =
            [d["vendor"].as_str(), d["model"].as_str()].into_iter().flatten().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        out_devs.push(Device {
            id: name.to_string(),
            node: PathBuf::from(d["path"].as_str().unwrap_or(&format!("/dev/{name}")).to_string()),
            name: name_parts.join(" "),
            size_bytes: d["size"].as_u64().unwrap_or(0),
            removable,
            internal: !removable,
            writable: !d["ro"].as_bool().unwrap_or(false),
            virtual_media: false,
            bus: d["tran"].as_str().map(str::to_string),
            mounts,
        });
    }
    Ok(out_devs)
}

// ------------------------------------------------------------------- source

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Compression {
    None,
    Zstd,
    Xz,
    Gzip,
}

impl Compression {
    pub fn of_path(p: &Path) -> Compression {
        match p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
            Some("zst" | "zstd") => Compression::Zstd,
            Some("xz") => Compression::Xz,
            Some("gz" | "gzip") => Compression::Gzip,
            _ => Compression::None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Source {
    pub path: PathBuf,
    pub compression: Compression,
    pub compressed_bytes: u64,
    /// Bytes that will actually be written, when it can be determined ahead of
    /// time. `None` means progress has no total.
    pub uncompressed_bytes: Option<u64>,
}

impl Source {
    pub fn detect(path: &Path) -> Result<Source> {
        let compressed_bytes = std::fs::metadata(path)
            .map_err(|e| Error::Other(format!("cannot read {}: {e}", path.display())))?
            .len();
        let compression = Compression::of_path(path);
        let uncompressed_bytes = match compression {
            Compression::None => Some(compressed_bytes),
            Compression::Zstd => zstd_frame_content_size(path).or_else(|| sidecar_image_bytes(path)),
            _ => sidecar_image_bytes(path),
        };
        Ok(Source { path: path.to_path_buf(), compression, compressed_bytes, uncompressed_bytes })
    }

    /// A reader that yields the raw disk image.
    pub fn open(&self) -> Result<Box<dyn Read>> {
        let f = std::fs::File::open(&self.path)?;
        Ok(match self.compression {
            Compression::None => Box::new(f),
            Compression::Zstd => Box::new(zstd::stream::read::Decoder::new(f)?),
            Compression::Xz => Box::new(xz2::read::XzDecoder::new_multi_decoder(f)),
            Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(f)),
        })
    }
}

/// `name.img.zst` -> stem `name`, used to find `name.summary.json` / `name.img.bmap`.
fn artifact_stem(path: &Path) -> Option<String> {
    let mut n = path.file_name()?.to_str()?.to_string();
    for suffix in [".zst", ".zstd", ".xz", ".gz", ".gzip"] {
        if let Some(s) = n.strip_suffix(suffix) {
            n = s.to_string();
            break;
        }
    }
    Some(n.strip_suffix(".img").unwrap_or(&n).to_string())
}

/// Uncompressed size from the build's `*.summary.json` or `*.img.bmap`.
fn sidecar_image_bytes(path: &Path) -> Option<u64> {
    let dir = path.parent()?;
    let stem = artifact_stem(path)?;
    if let Some(v) = std::fs::read(dir.join(format!("{stem}.summary.json")))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v["image_bytes"].as_u64())
    {
        return Some(v);
    }
    let bmap = std::fs::read_to_string(dir.join(format!("{stem}.img.bmap"))).ok()?;
    let start = bmap.find("<ImageSize>")? + "<ImageSize>".len();
    let end = bmap[start..].find("</ImageSize>")? + start;
    bmap[start..end].trim().parse().ok()
}

/// Decompressed size from the zstd frame header, when the producer recorded it.
fn zstd_frame_content_size(path: &Path) -> Option<u64> {
    let mut head = [0u8; 18];
    let mut f = std::fs::File::open(path).ok()?;
    let n = f.read(&mut head).ok()?;
    parse_zstd_content_size(&head[..n])
}

fn parse_zstd_content_size(h: &[u8]) -> Option<u64> {
    if h.len() < 5 || h[..4] != [0x28, 0xb5, 0x2f, 0xfd] {
        return None;
    }
    let d = h[4];
    let fcs_flag = d >> 6;
    let single = (d >> 5) & 1 == 1;
    let did_flag = d & 3;
    let mut i = 5 + if single { 0 } else { 1 };
    i += [0usize, 1, 2, 4][did_flag as usize];
    let len = match fcs_flag {
        0 if single => 1,
        0 => return None,
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let b = h.get(i..i + len)?;
    Some(match len {
        1 => b[0] as u64,
        2 => u16::from_le_bytes([b[0], b[1]]) as u64 + 256,
        4 => u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64,
        _ => u64::from_le_bytes(b.try_into().ok()?),
    })
}

// ------------------------------------------------------------------ request

#[derive(Debug, Clone)]
pub struct Request {
    pub image: PathBuf,
    /// Whole-disk node, e.g. /dev/disk4.
    pub device: PathBuf,
    /// Read the card back and compare checksums.
    pub verify: bool,
    /// Eject when finished.
    pub eject: bool,
    /// Allow internal / non-removable targets.
    pub force: bool,
}

impl Request {
    /// Fails fast, before any privileges are requested.
    pub fn check(&self) -> Result<(Source, Device)> {
        let src = Source::detect(&self.image)?;
        let dev = find_device(&self.device.to_string_lossy())?;
        if dev.is_system() {
            return Err(Error::Other(format!("{} holds the running system", dev.id)));
        }
        if !dev.writable {
            return Err(Error::Other(format!("{} is write protected", dev.id)));
        }
        if dev.is_protected() && !self.force {
            return Err(Error::Other(format!("{} is an internal disk; pass force to flash it anyway", dev.id)));
        }
        if let Some(n) = src.uncompressed_bytes {
            if n > dev.size_bytes && dev.size_bytes > 0 {
                return Err(Error::Other(format!("image is {} but {} only holds {}", human(n), dev.id, human(dev.size_bytes))));
            }
        }
        Ok((src, dev))
    }
}

// ------------------------------------------------------------------- events

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum FlashEvent {
    Stage { title: String },
    Progress { phase: String, done: u64, total: Option<u64> },
    Done { ok: bool, bytes: u64, message: Option<String> },
}

pub enum Progress<'a> {
    Stage(&'a str),
    Bytes { phase: &'a str, done: u64, total: Option<u64> },
    Note(&'a str),
}

// ---------------------------------------------------------------- elevation

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elevation {
    /// Already root.
    None,
    /// `sudo`, prompting on the terminal.
    Sudo,
    /// Graphical authorization prompt.
    Gui,
}

pub fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Set when the caller is already privileged by other means (a root container,
/// CI) and no `sudo`/prompt should be inserted. It can only remove elevation,
/// never grant it: the helper still has to be able to open the device.
const NO_ELEVATE: &str = "IMAGESMITH_FLASH_NO_ELEVATE";

fn elevation_disabled() -> bool {
    std::env::var_os(NO_ELEVATE).is_some_and(|v| v != "0" && !v.is_empty())
}

/// What a terminal program should use.
pub fn cli_elevation() -> Elevation {
    if is_root() || elevation_disabled() {
        Elevation::None
    } else {
        Elevation::Sudo
    }
}

/// What a windowed program should use.
pub fn gui_elevation() -> Elevation {
    if is_root() || elevation_disabled() {
        Elevation::None
    } else {
        Elevation::Gui
    }
}

// ------------------------------------------------------------------- parent

#[cfg(target_os = "macos")]
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(target_os = "macos")]
fn applescript_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', r"\\").replace('"', "\\\""))
}

struct Temps {
    dir: PathBuf,
}

impl Temps {
    fn new() -> Result<Temps> {
        let dir = std::env::temp_dir().join(format!("imagesmith-flash-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(Temps { dir })
    }
}

impl Drop for Temps {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn helper_args(req: &Request, cancel_file: &Path) -> Vec<String> {
    let mut a = vec![
        HELPER_FLAG.to_string(),
        "--source".into(),
        req.image.to_string_lossy().into_owned(),
        "--device".into(),
        req.device.to_string_lossy().into_owned(),
        "--cancel-file".into(),
        cancel_file.to_string_lossy().into_owned(),
    ];
    if req.verify {
        a.push("--verify".into());
    }
    if req.eject {
        a.push("--eject".into());
    }
    a
}

#[cfg(target_os = "macos")]
fn mkfifo(path: &Path) -> Result<()> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| Error::Other(e.to_string()))?;
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Starts the privileged helper and returns it, a reader of its event lines,
/// and (for FIFO transports) a handle used to unblock that reader at the end.
fn spawn_helper(req: &Request, elevation: Elevation, t: &Temps, cancel_file: &Path) -> Result<(Child, Box<dyn Read + Send>, Option<std::fs::File>)> {
    let exe = std::env::current_exe()?;
    let args = helper_args(req, cancel_file);
    match elevation {
        Elevation::None | Elevation::Sudo => {
            let mut c = if elevation == Elevation::Sudo {
                let mut c = Command::new("sudo");
                c.arg("--").arg(&exe);
                c
            } else {
                Command::new(&exe)
            };
            let mut child = c.args(&args).stdout(Stdio::piped()).spawn().map_err(|e| Error::Other(format!("cannot start the flash helper: {e}")))?;
            let out = child.stdout.take().expect("piped");
            Ok((child, Box::new(out), None))
        }
        #[cfg(target_os = "macos")]
        Elevation::Gui => {
            // `do shell script` gives the child no stdout we can read, so the
            // helper's events are redirected through a FIFO instead.
            let fifo = t.dir.join("events");
            mkfifo(&fifo)?;
            // Read-write so the open succeeds even if authorization is
            // declined and no writer ever appears.
            let reader = std::fs::OpenOptions::new().read(true).write(true).open(&fifo)?;
            let poker = reader.try_clone()?;
            let cmd = format!(
                "{} {} > {} 2>&1",
                sh_quote(&exe.to_string_lossy()),
                args.iter().map(|a| sh_quote(a)).collect::<Vec<_>>().join(" "),
                sh_quote(&fifo.to_string_lossy())
            );
            let script = format!("do shell script {} with administrator privileges", applescript_quote(&cmd));
            let child = Command::new("osascript")
                .args(["-e", &script])
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| Error::Other(format!("cannot start the authorization prompt: {e}")))?;
            Ok((child, Box::new(reader), Some(poker)))
        }
        #[cfg(not(target_os = "macos"))]
        Elevation::Gui => {
            let _ = t;
            let mut child = Command::new("pkexec")
                .arg(&exe)
                .args(&args)
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| Error::Other(format!("cannot start pkexec: {e}")))?;
            let out = child.stdout.take().expect("piped");
            Ok((child, Box::new(out), None))
        }
    }
}

/// Flashes `req.image` onto `req.device`, reporting progress as it goes.
/// Returns the number of image bytes written.
pub fn flash(req: &Request, elevation: Elevation, cancel: &CancelToken, on: &mut dyn FnMut(Progress)) -> Result<u64> {
    let (src, dev) = req.check()?;
    on(Progress::Note(&format!(
        "Writing {} to {}",
        src.uncompressed_bytes.map(human).unwrap_or_else(|| human(src.compressed_bytes)),
        dev.label()
    )));

    let t = Temps::new()?;
    let cancel_file = t.dir.join("cancel");
    let (mut child, reader, poker) = spawn_helper(req, elevation, &t, &cancel_file)?;

    // The helper's events arrive on a thread so cancellation and child exit
    // stay responsive even when the stream goes quiet.
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(std::result::Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut requested_cancel = false;
    let mut written = 0u64;
    let mut outcome: Option<std::result::Result<u64, String>> = None;
    let mut stray = Vec::new();
    loop {
        if cancel.is_cancelled() && !requested_cancel {
            requested_cancel = true;
            let _ = std::fs::write(&cancel_file, b"1");
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => match serde_json::from_str::<FlashEvent>(line.trim()) {
                Ok(FlashEvent::Stage { title }) => on(Progress::Stage(&title)),
                Ok(FlashEvent::Progress { phase, done, total }) => {
                    if phase == "write" {
                        written = done;
                    }
                    on(Progress::Bytes { phase: &phase, done, total });
                }
                Ok(FlashEvent::Done { ok, bytes, message }) => {
                    outcome = Some(if ok { Ok(bytes) } else { Err(message.unwrap_or_else(|| "flashing failed".into())) });
                }
                Err(_) if line.trim().is_empty() => {}
                Err(_) => {
                    on(Progress::Note(line.trim()));
                    stray.push(line.trim().to_string());
                }
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if outcome.is_some() {
            break;
        }
        if let Some(status) = child.try_wait()? {
            // Drain anything still buffered before deciding what happened.
            while let Ok(line) = rx.recv_timeout(Duration::from_millis(50)) {
                if let Ok(FlashEvent::Done { ok, bytes, message }) = serde_json::from_str::<FlashEvent>(line.trim()) {
                    outcome = Some(if ok { Ok(bytes) } else { Err(message.unwrap_or_else(|| "flashing failed".into())) });
                } else if !line.trim().is_empty() {
                    stray.push(line.trim().to_string());
                }
            }
            if outcome.is_none() {
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    let _ = e.read_to_string(&mut err);
                }
                let detail = [err.trim().to_string(), stray.join("; ")].into_iter().find(|s| !s.is_empty()).unwrap_or_default();
                outcome = Some(Err(if cancel.is_cancelled() {
                    "cancelled".into()
                } else if detail.contains("User canceled") || detail.contains("(-128)") {
                    "authorization was declined".into()
                } else if detail.is_empty() {
                    format!("the flash helper exited with {status}")
                } else {
                    detail
                }));
            }
            break;
        }
    }
    let _ = child.wait();
    // A FIFO reader holds its own write end, so it never sees EOF; drop the
    // receiver and nudge it so the thread can notice and finish.
    drop(rx);
    if let Some(mut p) = poker {
        let _ = p.write_all(b"\n");
    }
    match outcome {
        Some(Ok(bytes)) => Ok(bytes.max(written)),
        Some(Err(_)) if cancel.is_cancelled() => Err(Error::Cancelled),
        Some(Err(e)) => Err(Error::Other(e)),
        None if cancel.is_cancelled() => Err(Error::Cancelled),
        None => Err(Error::Other("the flash helper stopped unexpectedly".into())),
    }
}

// ------------------------------------------------------------------- helper

fn emit(e: &FlashEvent) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", serde_json::to_string(e).unwrap_or_default());
    let _ = out.flush();
}

/// True when the process was started as the privileged writer.
pub fn is_helper_invocation(args: &[String]) -> bool {
    args.iter().any(|a| a == HELPER_FLAG)
}

/// Entry point of the privileged child. Returns the process exit code.
pub fn helper_main(args: &[String]) -> i32 {
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let (Some(source), Some(device)) = (value("--source"), value("--device")) else {
        emit(&FlashEvent::Done { ok: false, bytes: 0, message: Some("missing --source or --device".into()) });
        return 2;
    };
    let cancel_file = value("--cancel-file").map(PathBuf::from);
    match helper_run(Path::new(&source), Path::new(&device), flag("--verify"), flag("--eject"), cancel_file.as_deref()) {
        Ok(bytes) => {
            emit(&FlashEvent::Done { ok: true, bytes, message: None });
            0
        }
        Err(e) => {
            emit(&FlashEvent::Done { ok: false, bytes: 0, message: Some(e.to_string()) });
            1
        }
    }
}

fn cancelled(p: Option<&Path>) -> bool {
    p.is_some_and(Path::exists)
}

/// macOS raw device: far faster than the buffered node and the only one that
/// gives predictable throughput.
fn write_node(device: &Path) -> PathBuf {
    if !cfg!(target_os = "macos") {
        return device.to_path_buf();
    }
    match device.file_name().and_then(|n| n.to_str()) {
        Some(n) if n.starts_with("disk") => device.with_file_name(format!("r{n}")),
        _ => device.to_path_buf(),
    }
}

fn unmount(device: &Path) -> Result<()> {
    if cfg!(target_os = "macos") {
        let out = Command::new("diskutil").args(["unmountDisk", "force"]).arg(device).output()?;
        if !out.status.success() {
            return Err(Error::Other(format!("could not unmount {}: {}", device.display(), String::from_utf8_lossy(&out.stderr).trim())));
        }
    } else {
        let dev = find_device(&device.to_string_lossy())?;
        for m in &dev.mounts {
            let out = Command::new("umount").arg(m).output()?;
            if !out.status.success() {
                return Err(Error::Other(format!("could not unmount {m}: {}", String::from_utf8_lossy(&out.stderr).trim())));
            }
        }
    }
    Ok(())
}

fn eject(device: &Path) {
    let _ = if cfg!(target_os = "macos") {
        Command::new("diskutil").arg("eject").arg(device).output()
    } else {
        Command::new("eject").arg(device).output()
    };
}

/// Error raised when macOS privacy settings block the device.
pub const PRIVACY_BLOCKED: &str = "Please allow Full Disk Access for ImageSmith, then try again.";

fn open_failed(node: &Path, what: &str, e: std::io::Error) -> Error {
    // Root still gets EPERM when macOS privacy settings deny removable volumes to the app.
    if cfg!(target_os = "macos") && e.raw_os_error() == Some(libc::EPERM) {
        return Error::Other(PRIVACY_BLOCKED.into());
    }
    Error::Other(format!("cannot open {} {what}: {e}", node.display()))
}

fn open_device_write(node: &Path) -> Result<std::fs::File> {
    std::fs::OpenOptions::new().write(true).open(node).map_err(|e| open_failed(node, "for writing", e))
}

fn open_device_read(node: &Path) -> Result<std::fs::File> {
    std::fs::File::open(node).map_err(|e| open_failed(node, "to verify", e))
}

fn helper_run(source: &Path, device: &Path, verify: bool, do_eject: bool, cancel_file: Option<&Path>) -> Result<u64> {
    let src = Source::detect(source)?;
    let total = src.uncompressed_bytes;

    emit(&FlashEvent::Stage { title: "Unmounting".into() });
    unmount(device)?;

    emit(&FlashEvent::Stage { title: "Writing".into() });
    let node = write_node(device);
    let mut dev = open_device_write(&node)?;
    let mut reader = src.open()?;
    let mut buf = vec![0u8; CHUNK];
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    let mut done = 0u64;
    let mut last = Instant::now();
    loop {
        if cancelled(cancel_file) {
            return Err(Error::Cancelled);
        }
        // Decoders routinely return short reads; fill the buffer so writes stay
        // block aligned on raw devices.
        let mut n = 0;
        while n < buf.len() {
            match reader.read(&mut buf[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::Other(format!("reading {}: {e}", source.display()))),
            }
        }
        if n == 0 {
            break;
        }
        sha2::Digest::update(&mut hasher, &buf[..n]);
        let padded = (n as u64).div_ceil(ALIGN) * ALIGN;
        if padded as usize > n {
            buf[n..padded as usize].fill(0);
        }
        dev.write_all(&buf[..padded as usize]).map_err(|e| Error::Other(format!("writing to {}: {e}", node.display())))?;
        done += n as u64;
        if last.elapsed() >= Duration::from_millis(250) {
            last = Instant::now();
            emit(&FlashEvent::Progress { phase: "write".into(), done, total });
        }
    }
    emit(&FlashEvent::Progress { phase: "write".into(), done, total: total.or(Some(done)) });

    emit(&FlashEvent::Stage { title: "Flushing".into() });
    dev.flush()?;
    if unsafe { libc::fsync(std::os::unix::io::AsRawFd::as_raw_fd(&dev)) } != 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    drop(dev);

    if verify {
        emit(&FlashEvent::Stage { title: "Verifying".into() });
        let want: String = sha2::Digest::finalize(hasher).iter().map(|b| format!("{b:02x}")).collect();
        let got = read_back(&node, done, cancel_file)?;
        if got != want {
            return Err(Error::Other("verification failed: the card does not match the image".into()));
        }
    }

    if do_eject {
        emit(&FlashEvent::Stage { title: "Ejecting".into() });
        eject(device);
    }
    Ok(done)
}

/// SHA-256 of the first `total` bytes of the device.
fn read_back(node: &Path, total: u64, cancel_file: Option<&Path>) -> Result<String> {
    let mut f = open_device_read(node)?;
    f.seek(SeekFrom::Start(0))?;
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    let mut last = Instant::now();
    while done < total {
        if cancelled(cancel_file) {
            return Err(Error::Cancelled);
        }
        let want = (total - done).min(CHUNK as u64);
        // Raw devices need aligned reads, so always read whole blocks and keep
        // only the part that belongs to the image.
        let aligned = (want.div_ceil(ALIGN) * ALIGN) as usize;
        let mut n = 0;
        while n < aligned {
            match f.read(&mut buf[n..aligned]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(Error::Other(format!("reading back from {}: {e}", node.display()))),
            }
        }
        if n == 0 {
            return Err(Error::Other("the device ended before the whole image could be verified".into()));
        }
        let take = (n as u64).min(want) as usize;
        sha2::Digest::update(&mut hasher, &buf[..take]);
        done += take as u64;
        if last.elapsed() >= Duration::from_millis(250) {
            last = Instant::now();
            emit(&FlashEvent::Progress { phase: "verify".into(), done, total: Some(total) });
        }
    }
    emit(&FlashEvent::Progress { phase: "verify".into(), done, total: Some(total) });
    Ok(sha2::Digest::finalize(hasher).iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_disk_ids() {
        assert_eq!(whole_disk_id("/dev/disk4s1"), "disk4");
        assert_eq!(whole_disk_id("/dev/rdisk4"), "disk4");
        assert_eq!(whole_disk_id("disk12"), "disk12");
        assert_eq!(whole_disk_id("/dev/sdb2"), "sdb");
        assert_eq!(whole_disk_id("/dev/nvme0n1p3"), "nvme0n1");
        assert_eq!(whole_disk_id("/dev/mmcblk0p1"), "mmcblk0");
    }

    #[test]
    fn compression_from_extension() {
        assert_eq!(Compression::of_path(Path::new("a.img.zst")), Compression::Zstd);
        assert_eq!(Compression::of_path(Path::new("a.img.xz")), Compression::Xz);
        assert_eq!(Compression::of_path(Path::new("a.img.gz")), Compression::Gzip);
        assert_eq!(Compression::of_path(Path::new("a.img")), Compression::None);
    }

    #[test]
    fn stems_ignore_compression_suffix() {
        assert_eq!(artifact_stem(Path::new("/o/pi-2026-01-02.img.zst")).as_deref(), Some("pi-2026-01-02"));
        assert_eq!(artifact_stem(Path::new("/o/pi.img")).as_deref(), Some("pi"));
    }

    #[test]
    fn zstd_header_sizes() {
        // Single segment, 1-byte content size of 10.
        assert_eq!(parse_zstd_content_size(&[0x28, 0xb5, 0x2f, 0xfd, 0x20, 10]), Some(10));
        // 4-byte content size, window descriptor present.
        let mut h = vec![0x28, 0xb5, 0x2f, 0xfd, 0x80, 0x00];
        h.extend(1_000_000u32.to_le_bytes());
        assert_eq!(parse_zstd_content_size(&h), Some(1_000_000));
        // Streamed input records no size.
        assert_eq!(parse_zstd_content_size(&[0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x00]), None);
        assert_eq!(parse_zstd_content_size(b"not zstd"), None);
    }

    #[test]
    fn protected_devices_are_flagged() {
        let d = Device {
            id: "disk0".into(),
            node: "/dev/disk0".into(),
            name: "APPLE SSD".into(),
            size_bytes: 1 << 40,
            removable: false,
            internal: true,
            writable: true,
            virtual_media: false,
            bus: Some("PCI-Express".into()),
            mounts: vec!["/System/Volumes/Data".into()],
        };
        assert!(d.is_system() && d.is_protected());
        let sd = Device { id: "disk4".into(), internal: false, removable: true, mounts: vec!["/Volumes/boot".into()], ..d };
        assert!(!sd.is_system() && !sd.is_protected());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn quoting_survives_awkward_paths() {
        assert_eq!(sh_quote("/a b/c'd"), r#"'/a b/c'\''d'"#);
        assert_eq!(applescript_quote(r#"say "hi"\"#), r#""say \"hi\"\\""#);
    }
}
