//! File manifests: one TSV line per path with type, mode, owner, size and a
//! content digest (sha256 for files, link target for symlinks).

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub kind: String,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub digest: String,
}

pub type Manifest = BTreeMap<String, Entry>;

fn escape(p: &str) -> String {
    p.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n")
}

fn unescape(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    let mut it = p.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn entry_for(host: &Path) -> io::Result<Entry> {
    let m = fs::symlink_metadata(host)?;
    let kind = crate::meta::type_name(&m).to_string();
    let digest = match kind.as_str() {
        "file" => sha256_file(host)?,
        "symlink" => fs::read_link(host)?.to_string_lossy().into_owned(),
        "char" | "block" => format!("{}", m.rdev()),
        _ => "-".into(),
    };
    Ok(Entry {
        size: if kind == "file" { m.size() } else { 0 },
        kind,
        mode: m.mode() & 0o7777,
        uid: m.uid(),
        gid: m.gid(),
        digest,
    })
}

/// Walk `root` without following symlinks or crossing filesystems.
pub fn generate(root: &Path) -> io::Result<Manifest> {
    let dev = fs::symlink_metadata(root)?.dev();
    let mut out = Manifest::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut names: Vec<_> = fs::read_dir(&dir)?.collect::<Result<_, _>>()?;
        names.sort_by_key(|e| e.file_name());
        for e in names {
            let p = e.path();
            let entry = entry_for(&p)?;
            if entry.kind == "dir" && fs::symlink_metadata(&p)?.dev() == dev {
                stack.push(p.clone());
            }
            out.insert(crate::paths::rootfs_path(root, &p), entry);
        }
    }
    Ok(out)
}

pub fn write(m: &Manifest, w: &mut impl Write) -> io::Result<()> {
    for (path, e) in m {
        writeln!(
            w,
            "{}\t{}\t{:04o}\t{}\t{}\t{}\t{}",
            escape(path),
            e.kind,
            e.mode,
            e.uid,
            e.gid,
            e.size,
            escape(&e.digest)
        )?;
    }
    Ok(())
}

pub fn read(path: &Path) -> io::Result<Manifest> {
    let f = BufReader::new(fs::File::open(path)?);
    let mut out = Manifest::new();
    for line in f.lines() {
        let line = line?;
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 7 {
            continue;
        }
        let bad = || io::Error::new(io::ErrorKind::InvalidData, format!("bad manifest line: {line}"));
        out.insert(
            unescape(f[0]),
            Entry {
                kind: f[1].into(),
                mode: u32::from_str_radix(f[2], 8).map_err(|_| bad())?,
                uid: f[3].parse().map_err(|_| bad())?,
                gid: f[4].parse().map_err(|_| bad())?,
                size: f[5].parse().map_err(|_| bad())?,
                digest: unescape(f[6]),
            },
        );
    }
    Ok(out)
}

fn entry_json(e: &Entry) -> Value {
    json!({
        "type": e.kind,
        "mode": format!("{:04o}", e.mode),
        "uid": e.uid,
        "gid": e.gid,
        "size": e.size,
        "digest": e.digest,
    })
}

/// Changes from `a` to `b`, optionally limited to paths under `prefix`.
pub fn diff(a: &Manifest, b: &Manifest, prefix: Option<&str>) -> Vec<Value> {
    let under = |p: &str| match prefix {
        None | Some("/") => true,
        Some(pre) => {
            let pre = pre.trim_end_matches('/');
            p == pre || p.starts_with(&format!("{pre}/"))
        }
    };
    let mut out = Vec::new();
    for (path, eb) in b.iter().filter(|(p, _)| under(p)) {
        match a.get(path) {
            None => out.push(json!({ "path": path, "change": "added", "after": entry_json(eb) })),
            Some(ea) if ea != eb => out.push(json!({
                "path": path, "change": "modified",
                "before": entry_json(ea), "after": entry_json(eb),
            })),
            _ => {}
        }
    }
    for (path, ea) in a.iter().filter(|(p, _)| under(p)) {
        if !b.contains_key(path) {
            out.push(json!({ "path": path, "change": "removed", "before": entry_json(ea) }));
        }
    }
    out.sort_by(|x, y| x["path"].as_str().cmp(&y["path"].as_str()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_diff() {
        let dir = std::env::temp_dir().join(format!("fsctl-man-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("etc")).unwrap();
        fs::write(dir.join("etc/host\tname"), "a").unwrap();
        let a = generate(&dir).unwrap();
        let mut buf = Vec::new();
        write(&a, &mut buf).unwrap();
        let file = dir.with_extension("manifest");
        fs::write(&file, &buf).unwrap();
        assert_eq!(read(&file).unwrap(), a);

        fs::write(dir.join("etc/host\tname"), "b").unwrap();
        fs::write(dir.join("etc/new"), "n").unwrap();
        let b = generate(&dir).unwrap();
        let d = diff(&a, &b, Some("/etc"));
        let changes: Vec<_> = d.iter().map(|v| v["change"].as_str().unwrap().to_string()).collect();
        assert_eq!(changes, vec!["modified", "added"]);
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_file(&file).unwrap();
    }
}
