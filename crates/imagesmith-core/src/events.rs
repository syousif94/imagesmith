//! JSON-lines event protocol: stdout lines starting with `@@` are events;
//! every other line is log text for the current stage.

use serde::{Deserialize, Serialize};

pub const PREFIX: &str = "@@";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Event {
    Stage {
        id: String,
        title: String,
        index: u32,
        total: u32,
        /// Set for recipe-step sub-stages ("steps").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
    },
    Progress {
        stage: String,
        done: u64,
        #[serde(default)]
        total: Option<u64>,
        unit: String,
    },
    Warn {
        message: String,
        #[serde(default)]
        hint: Option<String>,
    },
    Artifact {
        kind: String,
        path: String,
        bytes: u64,
    },
    Cache {
        hit: bool,
        key: String,
    },
    Error {
        stage: String,
        message: String,
        #[serde(default)]
        hint: Option<String>,
    },
    Done {
        ok: bool,
        duration_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    Event(Event),
    Log(String),
}

/// Lines that start with `@@` but don't parse are kept as log text so
/// nothing the builder prints is lost.
pub fn parse_line(line: &str) -> Line {
    let line = line.trim_end_matches(['\r', '\n']);
    match line.strip_prefix(PREFIX) {
        Some(json) => match serde_json::from_str(json) {
            Ok(e) => Line::Event(e),
            Err(_) => Line::Log(line.to_string()),
        },
        None => Line::Log(line.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spec_example() {
        let l = parse_line(r#"@@{"type":"progress","stage":"extract","done":812000000,"total":3200000000,"unit":"bytes"}"#);
        assert_eq!(
            l,
            Line::Event(Event::Progress { stage: "extract".into(), done: 812000000, total: Some(3200000000), unit: "bytes".into() })
        );
        assert_eq!(parse_line("hello\n"), Line::Log("hello".into()));
        assert_eq!(parse_line("@@{broken"), Line::Log("@@{broken".into()));
        let s = serde_json::to_string(&Event::Done { ok: true, duration_ms: 5 }).unwrap();
        assert_eq!(s, r#"{"type":"done","ok":true,"duration_ms":5}"#);
    }
}
