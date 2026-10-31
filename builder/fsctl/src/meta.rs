use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, Metadata};
use std::io::{self, Read};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

/// User and group names as defined by the rootfs, not the container.
#[derive(Default)]
pub struct Owners {
    users: HashMap<u32, String>,
    groups: HashMap<u32, String>,
}

impl Owners {
    pub fn load(root: &Path) -> Owners {
        Owners {
            users: parse_db(&root.join("etc/passwd")),
            groups: parse_db(&root.join("etc/group")),
        }
    }

    pub fn user(&self, uid: u32) -> Option<&str> {
        self.users.get(&uid).map(String::as_str)
    }

    pub fn group(&self, gid: u32) -> Option<&str> {
        self.groups.get(&gid).map(String::as_str)
    }

    /// Parses "user:group" with names or numbers.
    pub fn parse_owner(&self, spec: &str) -> io::Result<(u32, u32)> {
        let (u, g) = spec.split_once(':').unwrap_or((spec, spec));
        let lookup = |map: &HashMap<u32, String>, s: &str| -> io::Result<u32> {
            if let Ok(n) = s.parse() {
                return Ok(n);
            }
            map.iter()
                .find(|(_, name)| name.as_str() == s)
                .map(|(id, _)| *id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("unknown owner: {s}")))
        };
        Ok((lookup(&self.users, u)?, lookup(&self.groups, g)?))
    }
}

fn parse_db(path: &Path) -> HashMap<u32, String> {
    let Ok(text) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    text.lines()
        .filter_map(|l| {
            let mut f = l.split(':');
            let name = f.next()?;
            let id = f.nth(1)?.parse().ok()?;
            Some((id, name.to_string()))
        })
        .collect()
}

pub fn type_name(m: &Metadata) -> &'static str {
    let t = m.file_type();
    if t.is_symlink() {
        "symlink"
    } else if t.is_dir() {
        "dir"
    } else if t.is_file() {
        "file"
    } else if t.is_char_device() {
        "char"
    } else if t.is_block_device() {
        "block"
    } else if t.is_fifo() {
        "fifo"
    } else if t.is_socket() {
        "socket"
    } else {
        "unknown"
    }
}

/// `ls -l` style mode string, including setuid/setgid/sticky.
pub fn mode_string(m: &Metadata) -> String {
    let mode = m.mode();
    let kind = match type_name(m) {
        "dir" => 'd',
        "symlink" => 'l',
        "char" => 'c',
        "block" => 'b',
        "fifo" => 'p',
        "socket" => 's',
        _ => '-',
    };
    let mut s = String::with_capacity(10);
    s.push(kind);
    let bits = [
        (0o400, 'r'), (0o200, 'w'), (0o100, 'x'),
        (0o040, 'r'), (0o020, 'w'), (0o010, 'x'),
        (0o004, 'r'), (0o002, 'w'), (0o001, 'x'),
    ];
    for (i, (bit, ch)) in bits.iter().enumerate() {
        let set = mode & bit != 0;
        let special = match i {
            2 => mode & 0o4000 != 0,
            5 => mode & 0o2000 != 0,
            8 => mode & 0o1000 != 0,
            _ => false,
        };
        s.push(match (special, set, i) {
            (true, true, 8) => 't',
            (true, false, 8) => 'T',
            (true, true, _) => 's',
            (true, false, _) => 'S',
            (false, true, _) => *ch,
            (false, false, _) => '-',
        });
    }
    s
}

pub fn entry_json(root: &Path, host: &Path, owners: &Owners) -> io::Result<Value> {
    let m = fs::symlink_metadata(host)?;
    let name = host
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".into());
    let target = if m.file_type().is_symlink() {
        Some(fs::read_link(host)?.to_string_lossy().into_owned())
    } else {
        None
    };
    let rdev = if m.file_type().is_char_device() || m.file_type().is_block_device() {
        let r = m.rdev();
        Some(json!({ "major": libc::major(r as libc::dev_t), "minor": libc::minor(r as libc::dev_t) }))
    } else {
        None
    };
    Ok(json!({
        "name": name,
        "path": crate::paths::rootfs_path(root, host),
        "type": type_name(&m),
        "mode": format!("{:04o}", m.mode() & 0o7777),
        "mode_str": mode_string(&m),
        "uid": m.uid(),
        "gid": m.gid(),
        "user": owners.user(m.uid()),
        "group": owners.group(m.gid()),
        "size": m.size(),
        "mtime": m.mtime(),
        "target": target,
        "rdev": rdev,
    }))
}

/// Short description for previews: ELF arch, script, text, or binary.
pub fn describe(host: &Path) -> io::Result<String> {
    let mut f = fs::File::open(host)?;
    let mut buf = [0u8; 512];
    let n = f.read(&mut buf)?;
    let b = &buf[..n];
    if b.len() >= 20 && &b[..4] == b"\x7fELF" {
        let little = b[5] == 1;
        let machine = if little {
            u16::from_le_bytes([b[18], b[19]])
        } else {
            u16::from_be_bytes([b[18], b[19]])
        };
        let arch = match machine {
            0x3e => "x86_64",
            0xb7 => "aarch64",
            0x28 => "arm",
            0x03 => "i386",
            0xf3 => "riscv",
            _ => "unknown",
        };
        let kind = match u16::from_le_bytes([b[16], b[17]]) {
            2 => "executable",
            3 => "shared object / PIE",
            1 => "relocatable",
            _ => "object",
        };
        return Ok(format!("ELF {} {}", arch, kind));
    }
    let sigs: &[(&[u8], &str)] = &[
        (b"\x89PNG", "PNG image"),
        (b"\xff\xd8\xff", "JPEG image"),
        (b"GIF8", "GIF image"),
        (b"\x1f\x8b", "gzip archive"),
        (b"\xfd7zXZ\x00", "xz archive"),
        (b"\x28\xb5\x2f\xfd", "zstd archive"),
        (b"PK\x03\x04", "zip archive"),
        (b"!<arch>", "ar archive"),
        (b"#!", "script"),
    ];
    for (sig, name) in sigs {
        if b.starts_with(sig) {
            return Ok((*name).into());
        }
    }
    if b.len() >= 262 && &b[257..262] == b"ustar" {
        return Ok("tar archive".into());
    }
    Ok(if is_text(b) { "text" } else { "binary data" }.into())
}

/// No NULs and valid UTF-8, allowing a multibyte char cut off at the end.
pub fn is_text(b: &[u8]) -> bool {
    if b.contains(&0) {
        return false;
    }
    match std::str::from_utf8(b) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    }
}
