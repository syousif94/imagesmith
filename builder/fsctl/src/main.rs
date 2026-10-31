//! fsctl: the only way the ImageSmith host touches Linux files in the store.
//! Every command prints one JSON document on stdout (except `cat`, `export`
//! and `manifest`, which stream raw bytes); errors go to stderr as JSON.

mod manifest;
mod meta;
mod ops;
mod paths;
mod store;

use base64::Engine;
use ops::Stage;
use serde_json::{json, Value};
use std::io::{self, BufRead, Read, Write};
use std::path::Path;

const USAGE: &str = "usage: fsctl <ls|stat|cat|put|rm|find|diff|import-base|export|df|evict|manifest|serve> ...";

struct Args {
    positional: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

impl Args {
    fn parse(raw: impl Iterator<Item = String>) -> Args {
        const BOOL_FLAGS: &[&str] = &["--hash", "--check", "--dir", "--from-base"];
        let mut positional = Vec::new();
        let mut flags = Vec::new();
        let mut it = raw.peekable();
        while let Some(a) = it.next() {
            if let Some(f) = a.strip_prefix("--") {
                if let Some((k, v)) = f.split_once('=') {
                    flags.push((format!("--{k}"), Some(v.to_string())));
                } else if BOOL_FLAGS.contains(&a.as_str()) {
                    flags.push((a, None));
                } else {
                    let v = it.next();
                    flags.push((a, v));
                }
            } else {
                positional.push(a);
            }
        }
        Args { positional, flags }
    }

    fn flag(&self, name: &str) -> Option<&str> {
        self.flags.iter().find(|(k, _)| k == name).and_then(|(_, v)| v.as_deref())
    }

    fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(k, _)| k == name)
    }

    fn pos(&self, i: usize) -> io::Result<&str> {
        self.positional
            .get(i)
            .map(String::as_str)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, USAGE))
    }

    fn project(&self) -> io::Result<Stage> {
        let id = self
            .flag("--project-id")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "--project-id is required"))?;
        Stage::open(id)
    }
}

fn parse_mode(s: Option<&str>) -> io::Result<Option<u32>> {
    s.map(|m| u32::from_str_radix(m, 8).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "mode must be octal")))
        .transpose()
}

fn print(v: &Value) {
    let mut out = io::stdout().lock();
    let _ = serde_json::to_writer(&mut out, v);
    let _ = writeln!(out);
}

fn run(cmd: &str, a: &Args) -> io::Result<Option<Value>> {
    let v = match cmd {
        "ls" => a.project()?.ls(a.positional.first().map_or("/", String::as_str))?,
        "stat" => a.project()?.stat(a.pos(0)?, a.has("--hash"))?,
        "cat" => {
            a.project()?.cat_raw(a.pos(0)?)?;
            return Ok(None);
        }
        "put" => {
            let mut content = Vec::new();
            if !a.has("--dir") {
                io::stdin().read_to_end(&mut content)?;
            }
            a.project()?
                .put(a.pos(0)?, &content, parse_mode(a.flag("--mode"))?, a.flag("--owner"), a.has("--dir"))?
        }
        "rm" => a.project()?.rm(a.pos(0)?)?,
        "find" => {
            let limit = a.flag("--limit").and_then(|l| l.parse().ok()).unwrap_or(1000);
            a.project()?.find(a.pos(0)?, a.flag("--under").unwrap_or("/"), limit)?
        }
        "diff" => {
            if a.positional.len() == 2 {
                let x = manifest::read(Path::new(a.pos(0)?))?;
                let y = manifest::read(Path::new(a.pos(1)?))?;
                json!({ "changes": manifest::diff(&x, &y, a.flag("--prefix")) })
            } else if a.has("--from-base") {
                a.project()?.diff_from_base(a.flag("--prefix"))?
            } else {
                a.project()?.diff_since_build()?
            }
        }
        "import-base" => {
            let sha = a
                .flag("--sha")
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "--sha is required"))?;
            store::import_base(sha, a.has("--check"))?
        }
        "export" => {
            store::export(a.pos(0)?, a.pos(1)?)?;
            return Ok(None);
        }
        "df" => store::df()?,
        "evict" => store::evict(a.pos(0)?, a.pos(1)?)?,
        "manifest" => {
            let m = manifest::generate(Path::new(a.pos(0)?))?;
            let mut out = io::BufWriter::new(io::stdout().lock());
            manifest::write(&m, &mut out)?;
            out.flush()?;
            return Ok(None);
        }
        "serve" => {
            let id = a
                .flag("--project-id")
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "--project-id is required"))?;
            serve(id)?;
            return Ok(None);
        }
        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE)),
    };
    Ok(Some(v))
}

fn rpc(project_id: &str, method: &str, p: &Value) -> io::Result<Value> {
    let s = |k: &str| p.get(k).and_then(Value::as_str);
    let need = |k: &str| s(k).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("missing param: {k}")));
    if method == "df" {
        return store::df();
    }
    if method == "ping" {
        return Ok(json!({ "pong": true, "version": env!("CARGO_PKG_VERSION") }));
    }
    let stage = Stage::open(project_id)?;
    match method {
        "ls" => stage.ls(s("path").unwrap_or("/")),
        "stat" => stage.stat(need("path")?, p.get("hash").and_then(Value::as_bool).unwrap_or(false)),
        "cat" => {
            let offset = p.get("offset").and_then(Value::as_u64).unwrap_or(0);
            let max = p.get("max").and_then(Value::as_u64).unwrap_or(1 << 20);
            stage.cat_json(need("path")?, offset, max)
        }
        "put" => {
            let content = match s("content_b64") {
                Some(b) => base64::engine::general_purpose::STANDARD
                    .decode(b)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
                None => Vec::new(),
            };
            let dir = p.get("dir").and_then(Value::as_bool).unwrap_or(false);
            stage.put(need("path")?, &content, parse_mode(s("mode"))?, s("owner"), dir)
        }
        "rm" => stage.rm(need("path")?),
        "find" => {
            let limit = p.get("limit").and_then(Value::as_u64).unwrap_or(1000) as usize;
            stage.find(need("pattern")?, s("under").unwrap_or("/"), limit)
        }
        "diff" => {
            if p.get("from_base").and_then(Value::as_bool).unwrap_or(false) {
                stage.diff_from_base(s("prefix"))
            } else {
                stage.diff_since_build()
            }
        }
        _ => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("unknown method: {method}"))),
    }
}

fn error_json(e: &io::Error) -> Value {
    json!({ "message": e.to_string(), "kind": format!("{:?}", e.kind()) })
}

/// JSON-lines RPC: `{"id":1,"method":"ls","params":{"path":"/etc"}}` in,
/// `{"id":1,"result":...}` or `{"id":1,"error":{...}}` out.
fn serve(project_id: &str) -> io::Result<()> {
    paths::safe_name(project_id)?;
    let stdin = io::stdin().lock();
    for line in stdin.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                print(&json!({ "id": null, "error": { "message": format!("bad request: {e}") } }));
                continue;
            }
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(json!({}));
        match rpc(project_id, method, &params) {
            Ok(result) => print(&json!({ "id": id, "result": result })),
            Err(e) => print(&json!({ "id": id, "error": error_json(&e) })),
        }
    }
    Ok(())
}

fn main() {
    let mut raw = std::env::args().skip(1);
    let Some(cmd) = raw.next() else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let args = Args::parse(raw);
    match run(&cmd, &args) {
        Ok(Some(v)) => print(&v),
        Ok(None) => {}
        Err(e) => {
            eprintln!("{}", json!({ "error": error_json(&e) }));
            std::process::exit(1);
        }
    }
}
