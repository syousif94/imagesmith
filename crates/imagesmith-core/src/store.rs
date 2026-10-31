//! Store usage and LRU eviction across bases and rootfs cache entries.

use crate::runner::Runtime;
use crate::Result;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Usage {
    pub total: u64,
    pub used: u64,
    pub avail: u64,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    pub kind: String,
    pub key: String,
    pub bytes: u64,
    pub last_used: i64,
}

pub fn usage(rt: &Runtime) -> Result<Usage> {
    Ok(serde_json::from_value(rt.fsctl(&["df"])?)?)
}

/// Picks the least recently used base/rootfs entries to drop until their
/// total fits under `cap`, never touching `keep`.
pub fn plan_eviction<'a>(u: &'a Usage, cap: u64, keep: &[(&str, &str)]) -> Vec<&'a Entry> {
    let mut lru: Vec<&Entry> = u.entries.iter().filter(|e| e.kind == "base" || e.kind == "rootfs").collect();
    let mut total: u64 = lru.iter().map(|e| e.bytes).sum();
    lru.sort_by_key(|e| e.last_used);
    let mut out = Vec::new();
    for e in lru {
        if total <= cap {
            break;
        }
        if keep.iter().any(|(k, key)| *k == e.kind && *key == e.key) {
            continue;
        }
        total -= e.bytes;
        out.push(e);
    }
    out
}

pub fn evict(rt: &Runtime, kind: &str, key: &str) -> Result<()> {
    rt.fsctl(&["evict", kind, key]).map(|_| ())
}

pub fn prune(rt: &Runtime, cap: u64, keep: &[(&str, &str)]) -> Result<Vec<Entry>> {
    let u = usage(rt)?;
    let victims: Vec<Entry> = plan_eviction(&u, cap, keep).into_iter().cloned().collect();
    for e in &victims {
        evict(rt, &e.kind, &e.key)?;
    }
    Ok(victims)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_oldest_but_keeps_current() {
        let e = |kind: &str, key: &str, bytes, last_used| Entry { kind: kind.into(), key: key.into(), bytes, last_used };
        let u = Usage {
            total: 0,
            used: 0,
            avail: 0,
            entries: vec![
                e("base", "old", 10, 1),
                e("rootfs", "cur", 10, 0),
                e("base", "new", 10, 5),
                e("apt", "x", 100, 0),
            ],
        };
        let v = plan_eviction(&u, 15, &[("rootfs", "cur")]);
        let keys: Vec<_> = v.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["old", "new"]);
    }
}
