use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub fn store_root() -> PathBuf {
    std::env::var_os("IMAGESMITH_STORE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/store"))
}

/// Accepts only simple names (ids, artifact names): no separators, no dot-dot.
pub fn safe_name(s: &str) -> io::Result<&str> {
    let ok = !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(s)
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidInput, format!("invalid name: {s:?}")))
    }
}

pub fn stage_dir(project_id: &str) -> io::Result<PathBuf> {
    Ok(store_root().join("projects").join(safe_name(project_id)?).join("stage"))
}

/// Normalized rootfs path ("/a/b") from user input; ".." never escapes "/".
pub fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    format!("/{}", parts.join("/"))
}

/// Resolve a rootfs path to a host path under `root`, interpreting symlinks
/// as if `root` were `/` (chroot semantics), so nothing escapes the tree.
pub fn resolve(root: &Path, path: &str, follow_last: bool) -> io::Result<PathBuf> {
    let mut pending: VecDeque<String> = normalize(path)
        .split('/')
        .filter(|c| !c.is_empty())
        .map(String::from)
        .collect();
    let mut resolved: Vec<String> = Vec::new();
    let mut hops = 0;
    while let Some(comp) = pending.pop_front() {
        if comp == ".." {
            resolved.pop();
            continue;
        }
        if comp == "." {
            continue;
        }
        let mut host = root.to_path_buf();
        host.extend(resolved.iter());
        host.push(&comp);
        let is_last = pending.is_empty();
        let meta = match fs::symlink_metadata(&host) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound && is_last => {
                resolved.push(comp);
                break;
            }
            Err(e) => return Err(e),
        };
        if meta.file_type().is_symlink() && (!is_last || follow_last) {
            hops += 1;
            if hops > 40 {
                return Err(io::Error::other("too many levels of symbolic links"));
            }
            let target = fs::read_link(&host)?;
            let target = target.to_string_lossy().into_owned();
            if target.starts_with('/') {
                resolved.clear();
            }
            for c in target.split('/').filter(|c| !c.is_empty()).rev() {
                pending.push_front(c.to_string());
            }
        } else {
            resolved.push(comp);
        }
    }
    let mut host = root.to_path_buf();
    host.extend(resolved.iter());
    Ok(host)
}

/// Inverse of `resolve` for display: host path under root -> "/a/b".
pub fn rootfs_path(root: &Path, host: &Path) -> String {
    match host.strip_prefix(root) {
        Ok(rel) if rel.as_os_str().is_empty() => "/".into(),
        Ok(rel) => format!("/{}", rel.to_string_lossy()),
        Err(_) => host.to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn symlinks_stay_inside_root() {
        let dir = std::env::temp_dir().join(format!("fsctl-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("usr/lib")).unwrap();
        symlink("usr/lib", dir.join("lib")).unwrap();
        symlink("/../../../etc", dir.join("escape")).unwrap();
        fs::create_dir_all(dir.join("etc")).unwrap();

        assert_eq!(resolve(&dir, "/lib/x", true).unwrap(), dir.join("usr/lib/x"));
        assert_eq!(resolve(&dir, "/escape", true).unwrap(), dir.join("etc"));
        assert_eq!(resolve(&dir, "/escape", false).unwrap(), dir.join("escape"));
        assert_eq!(resolve(&dir, "/../../etc", true).unwrap(), dir.join("etc"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn names() {
        assert!(safe_name("abc-1.img").is_ok());
        assert!(safe_name("../x").is_err());
        assert!(safe_name("..").is_err());
        assert_eq!(normalize("a/../../b/./c/"), "/b/c");
    }
}
