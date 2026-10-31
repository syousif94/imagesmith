//! Store-level operations: base import, artifact export, usage, eviction.

use crate::manifest::hex;
use crate::paths::{safe_name, store_root};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;

fn valid_sha(sha: &str) -> io::Result<()> {
    if sha.len() == 64 && sha.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidInput, "sha must be 64 lowercase hex chars"))
    }
}

pub fn touch_used(path: &Path) {
    let mut used = path.as_os_str().to_owned();
    used.push(".used");
    let _ = fs::write(PathBuf::from(used), b"");
}

pub fn base_path(sha: &str) -> PathBuf {
    store_root().join("bases").join(format!("{sha}.img"))
}

/// Reads a (possibly compressed) base image from stdin, verifies its SHA-256
/// against `sha` (hash of the bytes as sent), and stores it raw.
pub fn import_base(sha: &str, check_only: bool) -> io::Result<Value> {
    valid_sha(sha)?;
    let dest = base_path(sha);
    if dest.exists() && still_compressed(&dest)? {
        // Imported raw by an older builder that couldn't decompress this format.
        fs::remove_file(&dest)?;
    }
    if dest.exists() {
        touch_used(&dest);
        return Ok(json!({ "present": true, "path": dest }));
    }
    if check_only {
        return Ok(json!({ "present": false }));
    }
    let dir = dest.parent().unwrap();
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".import-{sha}-{}", std::process::id()));
    let result = import_into(sha, &tmp);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    fs::rename(&tmp, &dest)?;
    touch_used(&dest);
    let size = fs::metadata(&dest)?.len();
    Ok(json!({ "present": true, "path": dest, "bytes": size }))
}

fn still_compressed(path: &Path) -> io::Result<bool> {
    let mut head = [0u8; 6];
    let n = fs::File::open(path)?.read(&mut head)?;
    let head = &head[..n];
    Ok([&b"7z\xbc\xaf\x27\x1c"[..], b"\xfd7zXZ\x00", b"\x28\xb5\x2f\xfd", b"\x1f\x8b"]
        .iter()
        .any(|m| head.starts_with(m)))
}

fn import_into(sha: &str, tmp: &Path) -> io::Result<()> {
    let mut stdin = io::stdin().lock();
    let mut buf = vec![0u8; 4 << 20];
    let mut first = 0;
    while first < 6 {
        let n = stdin.read(&mut buf[first..])?;
        if n == 0 {
            break;
        }
        first += n;
    }
    let head = &buf[..first];
    let seven_zip = head.starts_with(b"7z\xbc\xaf\x27\x1c");
    let decompressor = if seven_zip {
        None
    } else if head.starts_with(b"\xfd7zXZ\x00") {
        Some("xz")
    } else if head.starts_with(b"\x28\xb5\x2f\xfd") {
        Some("zstd")
    } else if head.starts_with(b"\x1f\x8b") {
        Some("gzip")
    } else {
        None
    };

    // 7z can't be decompressed from a pipe: spool the archive, extract after the hash check.
    let archive = tmp.with_extension("7z");
    let out = fs::File::create(if seven_zip { &archive } else { tmp })?;
    let mut child = None;
    let mut sink: Box<dyn Write> = match decompressor {
        Some(cmd) => {
            let mut c = Command::new(cmd)
                .arg("-dc")
                .stdin(Stdio::piped())
                .stdout(out)
                .spawn()?;
            let w = c.stdin.take().unwrap();
            child = Some(c);
            Box::new(w)
        }
        None => Box::new(out),
    };

    let mut hasher = Sha256::new();
    let mut total = first as u64;
    let mut next_report = 256u64 << 20;
    hasher.update(head);
    sink.write_all(head)?;
    loop {
        let n = stdin.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        sink.write_all(&buf[..n])?;
        total += n as u64;
        if total >= next_report {
            println!(r#"@@{{"type":"progress","stage":"import","done":{total},"total":null,"unit":"bytes"}}"#);
            next_report += 256 << 20;
        }
    }
    sink.flush()?;
    drop(sink);
    if let Some(mut c) = child {
        let st = c.wait()?;
        if !st.success() {
            return Err(io::Error::other(format!("decompression failed: {st}")));
        }
    }
    let got = hex(&hasher.finalize());
    if got != sha {
        let _ = fs::remove_file(&archive);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("checksum mismatch: expected {sha}, got {got}"),
        ));
    }
    if seven_zip {
        let result = extract_7z(&archive, tmp);
        let _ = fs::remove_file(&archive);
        result?;
    }
    Ok(())
}

/// Extracts the disk image from a .7z archive (the largest .img/.raw/.bin entry).
fn extract_7z(archive: &Path, dest: &Path) -> io::Result<()> {
    let list = Command::new("7z").args(["l", "-slt", "-ba"]).arg(archive).output()?;
    if !list.status.success() {
        return Err(io::Error::other(format!(
            "7z list failed: {}",
            String::from_utf8_lossy(&list.stderr).trim()
        )));
    }
    let entry = pick_7z_entry(&String::from_utf8_lossy(&list.stdout))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no .img file found in the 7z archive"))?;
    let st = Command::new("7z")
        .args(["e", "-so"])
        .arg(archive)
        .arg("--")
        .arg(&entry)
        .stdout(fs::File::create(dest)?)
        .stderr(Stdio::inherit())
        .status()?;
    if !st.success() {
        return Err(io::Error::other(format!("7z extraction failed: {st}")));
    }
    Ok(())
}

fn pick_7z_entry(slt: &str) -> Option<String> {
    let mut best: Option<(u64, String)> = None;
    for block in slt.split("\n\n") {
        let field = |k: &str| block.lines().find_map(|l| l.strip_prefix(k).map(str::trim));
        let Some(path) = field("Path = ") else { continue };
        if field("Folder = ") == Some("+") {
            continue;
        }
        let lower = path.to_lowercase();
        if ![".img", ".raw", ".bin"].iter().any(|s| lower.ends_with(s)) {
            continue;
        }
        let size = field("Size = ").and_then(|s| s.parse().ok()).unwrap_or(0);
        if best.as_ref().is_none_or(|(b, _)| size > *b) {
            best = Some((size, path.to_string()));
        }
    }
    best.map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    use super::pick_7z_entry;

    #[test]
    fn picks_largest_image() {
        let slt = "Path = a/readme.txt\nFolder = -\nSize = 10\n\nPath = a/os.img\nFolder = -\nSize = 4000\n\nPath = a/os.img.sha\nFolder = -\nSize = 90\n\nPath = a\nFolder = +\nSize = 0\n";
        assert_eq!(pick_7z_entry(slt).as_deref(), Some("a/os.img"));
        assert_eq!(pick_7z_entry("Path = x.txt\nSize = 1\n"), None);
    }
}

pub fn export(build_id: &str, name: &str) -> io::Result<()> {
    let path = store_root()
        .join("tmp")
        .join(safe_name(build_id)?)
        .join("out")
        .join(safe_name(name)?);
    let mut f = fs::File::open(&path)?;
    let mut out = io::stdout().lock();
    io::copy(&mut f, &mut out)?;
    out.flush()
}

fn du(path: &Path, seen: &mut HashSet<(u64, u64)>) -> u64 {
    let Ok(m) = fs::symlink_metadata(path) else {
        return 0;
    };
    if m.nlink() > 1 && !m.is_dir() && !seen.insert((m.dev(), m.ino())) {
        return 0;
    }
    let mut total = m.blocks() * 512;
    if m.is_dir() {
        if let Ok(rd) = fs::read_dir(path) {
            for e in rd.flatten() {
                total += du(&e.path(), seen);
            }
        }
    }
    total
}

fn last_used(path: &Path) -> i64 {
    let mut used = path.as_os_str().to_owned();
    used.push(".used");
    fs::metadata(PathBuf::from(used))
        .or_else(|_| fs::metadata(path))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn df() -> io::Result<Value> {
    let root = store_root();
    let c = std::ffi::CString::new(root.as_os_str().as_encoded_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let frsize = st.f_frsize as u64;
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    let mut scan = |kind: &str, dir: PathBuf, strip: Option<&str>, sub: Option<&str>| {
        let Ok(rd) = fs::read_dir(&dir) else { return };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name.ends_with(".used") {
                continue;
            }
            let key = match strip {
                Some(s) => name.strip_suffix(s).unwrap_or(&name).to_string(),
                None => name.clone(),
            };
            let path = match sub {
                Some(s) => e.path().join(s),
                None => e.path(),
            };
            entries.push(json!({
                "kind": kind,
                "key": key,
                "bytes": du(&path, &mut seen),
                "last_used": last_used(&e.path()),
            }));
        }
    };
    scan("base", root.join("bases"), Some(".img"), None);
    scan("rootfs", root.join("rootfs"), None, None);
    scan("apt", root.join("apt"), None, None);
    scan("stage", root.join("projects"), None, None);
    scan("tmp", root.join("tmp"), None, None);
    Ok(json!({
        "total": st.f_blocks as u64 * frsize,
        "used": (st.f_blocks - st.f_bfree) as u64 * frsize,
        "avail": st.f_bavail as u64 * frsize,
        "entries": entries,
    }))
}

pub fn evict(kind: &str, key: &str) -> io::Result<Value> {
    let root = store_root();
    let key = safe_name(key)?;
    let path = match kind {
        "base" => root.join("bases").join(format!("{key}.img")),
        "rootfs" => root.join("rootfs").join(key),
        "apt" => root.join("apt").join(key),
        "stage" => root.join("projects").join(key),
        "tmp" => root.join("tmp").join(key),
        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("unknown kind: {kind}"))),
    };
    let mut used = path.as_os_str().to_owned();
    used.push(".used");
    let _ = fs::remove_file(PathBuf::from(used));
    if kind == "rootfs" {
        let _ = fs::remove_file(root.join("rootfs").join(format!(".meta-{key}")));
        let _ = fs::remove_file(root.join("rootfs").join(format!(".manifest-{key}")));
    }
    match fs::symlink_metadata(&path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(&path)?,
        Ok(_) => fs::remove_file(&path)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    Ok(json!({ "evicted": true, "kind": kind, "key": key }))
}
