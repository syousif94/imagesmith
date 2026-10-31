//! SHA-256 of base images, cached by (path, size, mtime) so validation on
//! every edit doesn't re-hash multi-GB files.

use crate::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::time::UNIX_EPOCH;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
struct Key {
    size: u64,
    mtime_ns: u128,
    sha256: String,
}

fn cache_file() -> std::path::PathBuf {
    crate::app_dirs::cache_dir().join("hashes.json")
}

fn stamp(path: &Path) -> Result<(u64, u128)> {
    let m = std::fs::metadata(path)?;
    let mtime = m.modified()?.duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    Ok((m.len(), mtime))
}

pub fn sha256_file(path: &Path, mut progress: impl FnMut(u64, u64)) -> Result<String> {
    let total = std::fs::metadata(path)?.len();
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 4 << 20];
    let mut done = 0;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        done += n as u64;
        progress(done, total);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Returns the cached hash if the file is unchanged since it was hashed.
pub fn cached_sha256(path: &Path) -> Option<String> {
    let canon = path.canonicalize().ok()?;
    let (size, mtime_ns) = stamp(&canon).ok()?;
    let map: HashMap<String, Key> = serde_json::from_slice(&std::fs::read(cache_file()).ok()?).ok()?;
    map.get(&canon.to_string_lossy().into_owned())
        .filter(|k| k.size == size && k.mtime_ns == mtime_ns)
        .map(|k| k.sha256.clone())
}

pub fn sha256_cached(path: &Path, progress: impl FnMut(u64, u64)) -> Result<String> {
    if let Some(h) = cached_sha256(path) {
        return Ok(h);
    }
    let canon = path.canonicalize()?;
    let (size, mtime_ns) = stamp(&canon)?;
    let sha256 = sha256_file(&canon, progress)?;
    let file = cache_file();
    let mut map: HashMap<String, Key> = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    map.retain(|p, _| Path::new(p).exists());
    map.insert(canon.to_string_lossy().into_owned(), Key { size, mtime_ns, sha256: sha256.clone() });
    std::fs::create_dir_all(file.parent().unwrap())?;
    std::fs::write(&file, serde_json::to_vec(&map)?)?;
    Ok(sha256)
}
