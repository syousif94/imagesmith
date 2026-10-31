//! Files tab: browses the project's stage directory through one
//! `fsctl serve` process; edits are recorded in the recipe.

use crate::edits;
use crate::editor::{TextArea, TextAreaEvent};
use crate::text_input::{InputEvent, TextInput};
use crate::theme::{Colors, MONO};
use crate::widgets::{button, pill, tooltip, ClickIf, Kind};
use crate::workspace::{runtime, Workspace};
use base64::Engine;
use gpui::prelude::*;
use gpui::{div, px, AnyElement, App, ClickEvent, Context, Entity, Focusable, PromptLevel, SharedString, Subscription, Task, WeakEntity, Window};
use imagesmith_core::runner::{Runtime, STORE_MOUNT};
use imagesmith_core::size::human;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::{Arc, Mutex};

const STEP_REMOVED: &str = "Removed in Files tab";
const STEP_SHELL: &str = "Captured from Shell";

gpui::actions!(files, [EditFile]);

/// JSON-lines RPC client for `fsctl serve`.
pub struct FsClient {
    io: Mutex<(ChildStdin, BufReader<ChildStdout>, u64)>,
    child: Mutex<Child>,
}

impl FsClient {
    fn spawn(rt: &Runtime, project_id: &str) -> Result<FsClient, String> {
        let mut child = rt
            .exec(true, &["fsctl", "serve", "--project-id", project_id])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Ok(FsClient { io: Mutex::new((stdin, stdout, 0)), child: Mutex::new(child) })
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let mut g = self.io.lock().map_err(|_| "fsctl client poisoned".to_string())?;
        g.2 += 1;
        let id = g.2;
        let req = json!({ "id": id, "method": method, "params": params });
        writeln!(g.0, "{req}").and_then(|_| g.0.flush()).map_err(|e| format!("fsctl serve: {e}"))?;
        loop {
            let mut line = String::new();
            if g.1.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
                return Err("fsctl serve exited".into());
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            if v["id"] != json!(id) {
                continue;
            }
            if let Some(e) = v.get("error") {
                return Err(e["message"].as_str().unwrap_or("fsctl error").to_string());
            }
            return Ok(v["result"].clone());
        }
    }
}

impl Drop for FsClient {
    fn drop(&mut self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub mode: String,
    pub mode_str: String,
    pub owner: String,
    pub size: u64,
    pub mtime: i64,
    pub target: Option<String>,
}

impl Entry {
    fn from(v: &Value) -> Entry {
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let user = v["user"].as_str().map(String::from).unwrap_or_else(|| v["uid"].to_string());
        let group = v["group"].as_str().map(String::from).unwrap_or_else(|| v["gid"].to_string());
        Entry {
            name: s("name"),
            path: s("path"),
            kind: s("type"),
            mode: s("mode"),
            mode_str: s("mode_str"),
            owner: format!("{user}:{group}"),
            size: v["size"].as_u64().unwrap_or(0),
            mtime: v["mtime"].as_i64().unwrap_or(0),
            target: v["target"].as_str().map(String::from),
        }
    }
    fn is_dir(&self) -> bool {
        self.kind == "dir"
    }
}

pub enum Status {
    Connecting,
    Ready,
    NoStage,
    Error(String),
}

pub enum Preview {
    Loading,
    Text { meta: Entry, text: String, truncated: bool },
    Binary { meta: Entry, describe: String, sha: Option<String> },
    Link { meta: Entry, resolved: Option<String> },
    Dir(Entry),
    Error(String),
}

pub struct FilesView {
    project_id: String,
    project_path: PathBuf,
    ws: WeakEntity<Workspace>,
    client: Option<Arc<FsClient>>,
    status: Status,
    partial: bool,
    cwd: String,
    children: HashMap<String, Vec<Entry>>,
    expanded: HashSet<String>,
    selected: Option<String>,
    preview: Option<Preview>,
    changes: HashMap<String, String>,
    overlay_paths: HashSet<String>,
    recipe_paths: HashSet<String>,
    changed_only: bool,
    search: Entity<TextInput>,
    new_name: Entity<TextInput>,
    results: Option<Vec<Entry>>,
    capture: Option<Vec<(String, String, bool)>>,
    editor: Option<(Entry, Entity<TextArea>, Subscription)>,
    pending_capture: bool,
    _subs: Vec<Subscription>,
}

fn parent_of(p: &str) -> String {
    match p.trim_end_matches('/').rsplit_once('/') {
        Some(("", _)) | None => "/".into(),
        Some((a, _)) => a.into(),
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

impl FilesView {
    pub fn new(project_id: String, project_path: PathBuf, ws: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> FilesView {
        let search = cx.new(|cx| TextInput::new(cx, "Find by name"));
        let new_name = cx.new(|cx| TextInput::new(cx, "new-file.conf"));
        let s1 = cx.subscribe(&search, |this: &mut FilesView, input, e: &InputEvent, cx| match e {
            InputEvent::Submit => {
                let q = input.read(cx).text();
                this.find(q, cx);
            }
            InputEvent::Changed => {
                if input.read(cx).text().is_empty() {
                    this.results = None;
                    cx.notify();
                }
            }
        });
        let s2 = cx.subscribe_in(&new_name, window, |this: &mut FilesView, input, e: &InputEvent, window, cx| {
            if matches!(e, InputEvent::Submit) {
                let n = input.read(cx).text();
                this.new_file(n, window, cx);
                input.update(cx, |i, cx| i.set_text("", cx));
            }
        });
        let mut v = FilesView {
            project_id,
            project_path,
            ws,
            client: None,
            status: Status::Connecting,
            partial: false,
            cwd: "/".into(),
            children: HashMap::new(),
            expanded: HashSet::from(["/".to_string()]),
            selected: None,
            preview: None,
            changes: HashMap::new(),
            overlay_paths: HashSet::new(),
            recipe_paths: HashSet::new(),
            changed_only: false,
            search,
            new_name,
            results: None,
            capture: None,
            editor: None,
            pending_capture: false,
            _subs: vec![s1, s2],
        };
        v.refresh(cx);
        v
    }

    fn busy(&self, cx: &App) -> bool {
        self.ws.upgrade().is_some_and(|w| w.read(cx).is_building(&self.project_id))
    }

    /// Reconnects and reloads after a build or on first open.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.status = Status::Connecting;
        self.client = None;
        self.children.clear();
        self.preview = None;
        self.results = None;
        self.overlay_paths = self.compute_overlay_paths();
        let rt = runtime(cx);
        let id = self.project_id.clone();
        let cwd = self.cwd.clone();
        let task = cx.background_spawn(async move {
            rt.ensure_store(&mut |_| {}).map_err(|e| e.to_string())?;
            let incomplete = format!("{STORE_MOUNT}/projects/{id}/stage.incomplete");
            let partial = rt.exec(false, &["test", "-e", &incomplete]).stdin(Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
            let client = Arc::new(FsClient::spawn(&rt, &id)?);
            let root = match client.call("ls", json!({ "path": "/" })) {
                Ok(v) => v,
                Err(e) if e.contains("stage does not exist") => return Ok(None),
                Err(e) => return Err(e),
            };
            let here = if cwd != "/" { client.call("ls", json!({ "path": cwd })).ok() } else { None };
            let changes = client.call("diff", json!({ "from_base": true })).unwrap_or(json!({ "changes": [] }));
            Ok(Some((client, partial, root, here, changes)))
        });
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |this, cx| {
                match r {
                    Ok(None) => this.status = Status::NoStage,
                    Err(e) => this.status = Status::Error(e),
                    Ok(Some((client, partial, root, here, changes))) => {
                        this.client = Some(client);
                        this.partial = partial;
                        this.status = Status::Ready;
                        this.store_listing(&root);
                        match here {
                            Some(h) => this.store_listing(&h),
                            None => this.cwd = "/".into(),
                        }
                        this.changes = changes["changes"]
                            .as_array()
                            .map(|a| a.iter().map(|c| (c["path"].as_str().unwrap_or("").to_string(), c["change"].as_str().unwrap_or("").to_string())).collect())
                            .unwrap_or_default();
                        if std::mem::take(&mut this.pending_capture) {
                            this.start_capture(cx);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// [[files]] destinations plus overlay/ contents; called from render,
    /// where reading the workspace is safe.
    fn sync_recipe_paths(&mut self, cx: &App) {
        let mut out = self.overlay_paths.clone();
        if let Some(ws) = self.ws.upgrade() {
            if let Some(p) = ws.read(cx).projects.iter().find(|p| p.id() == self.project_id) {
                if let Some(rf) = &p.recipe {
                    out.extend(rf.recipe.files.iter().map(|f| f.dst.clone()));
                }
            }
        }
        self.recipe_paths = out;
    }

    fn compute_overlay_paths(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        fn walk(dir: &std::path::Path, prefix: &str, out: &mut HashSet<String>) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let p = format!("{prefix}/{name}");
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    walk(&e.path(), &p, out);
                } else {
                    out.insert(p);
                }
            }
        }
        walk(&self.project_path.join("overlay"), "", &mut out);
        out
    }

    fn store_listing(&mut self, v: &Value) {
        let path = v["path"].as_str().unwrap_or("/").to_string();
        let entries = v["entries"].as_array().map(|a| a.iter().map(Entry::from).collect()).unwrap_or_default();
        self.children.insert(path, entries);
    }

    fn call_bg(&self, cx: &mut Context<Self>, method: &'static str, params: Value) -> Option<Task<Result<Value, String>>> {
        let client = self.client.clone()?;
        Some(cx.background_spawn(async move { client.call(method, params) }))
    }

    fn load_dir(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(task) = self.call_bg(cx, "ls", json!({ "path": path })) else { return };
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |this, cx| {
                match r {
                    Ok(v) => this.store_listing(&v),
                    Err(e) => this.preview = Some(Preview::Error(e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn navigate(&mut self, path: String, cx: &mut Context<Self>) {
        self.results = None;
        let mut p = path.clone();
        while p != "/" {
            self.expanded.insert(p.clone());
            p = parent_of(&p);
        }
        if !self.children.contains_key(&path) {
            self.load_dir(path.clone(), cx);
        }
        self.cwd = path;
        cx.notify();
    }

    fn toggle_tree(&mut self, path: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&path) {
            self.expanded.insert(path.clone());
            if !self.children.contains_key(&path) {
                self.load_dir(path.clone(), cx);
            }
        }
        self.navigate_only(path, cx);
    }

    fn navigate_only(&mut self, path: String, cx: &mut Context<Self>) {
        self.results = None;
        if !self.children.contains_key(&path) {
            self.load_dir(path.clone(), cx);
        }
        self.cwd = path;
        cx.notify();
    }

    fn select(&mut self, e: Entry, cx: &mut Context<Self>) {
        self.selected = Some(e.path.clone());
        if e.is_dir() {
            self.preview = Some(Preview::Dir(e));
            cx.notify();
            return;
        }
        self.preview = Some(Preview::Loading);
        let Some(client) = self.client.clone() else { return };
        let path = e.path.clone();
        let task = cx.background_spawn(async move {
            let st = client.call("stat", json!({ "path": path, "hash": false }))?;
            let meta = Entry::from(&st);
            if meta.kind == "symlink" {
                return Ok(Preview::Link { resolved: st["resolved"].as_str().map(String::from), meta });
            }
            if meta.kind != "file" {
                return Ok(Preview::Binary { describe: meta.kind.clone(), sha: None, meta });
            }
            let describe = st["describe"].as_str().unwrap_or("").to_string();
            if describe == "text" || meta.size == 0 {
                let c = client.call("cat", json!({ "path": path, "max": 1 << 20 }))?;
                let bytes = base64::engine::general_purpose::STANDARD.decode(c["content_b64"].as_str().unwrap_or("")).unwrap_or_default();
                if c["text"].as_bool().unwrap_or(true) {
                    return Ok(Preview::Text { text: String::from_utf8_lossy(&bytes).into_owned(), truncated: c["truncated"].as_bool().unwrap_or(false), meta });
                }
            }
            let sha = client.call("stat", json!({ "path": path, "hash": true })).ok().and_then(|v| v["sha256"].as_str().map(String::from));
            Ok::<_, String>(Preview::Binary { describe, sha, meta })
        });
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.selected.as_deref() == Some(e.path.as_str()) {
                    this.preview = Some(r.unwrap_or_else(Preview::Error));
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn find(&mut self, q: String, cx: &mut Context<Self>) {
        let q = q.trim().to_string();
        if q.is_empty() {
            self.results = None;
            cx.notify();
            return;
        }
        let Some(task) = self.call_bg(cx, "find", json!({ "pattern": q, "under": self.cwd, "limit": 500 })) else { return };
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |this, cx| {
                match r {
                    Ok(v) => {
                        let list = v["results"].as_array();
                        this.results = Some(list.map(|a| a.iter().map(Entry::from).collect()).unwrap_or_default());
                    }
                    Err(e) => this.preview = Some(Preview::Error(e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ------------------------------------------------------------ edits

    fn local_copy(&self, path: &str) -> PathBuf {
        self.project_path.join("files/rootfs").join(path.trim_start_matches('/'))
    }

    /// Records `path` in the recipe as a [[files]] entry and pushes it into
    /// the stage so the tree reflects it immediately.
    fn record_file(&mut self, path: String, content: Vec<u8>, mode: String, owner: String, cx: &mut Context<Self>) {
        let local = self.local_copy(&path);
        if let Err(e) = local.parent().map(std::fs::create_dir_all).transpose().and_then(|_| std::fs::write(&local, &content)) {
            self.toast(format!("Cannot write {}: {e}", local.display()), cx);
            return;
        }
        let src = format!("files/rootfs{path}");
        let (p2, m2, o2) = (path.clone(), mode.clone(), owner.clone());
        self.edit_recipe(
            move |doc| {
                let a = edits::aot_mut(doc, "files");
                let existing = a.iter_mut().find(|t| t.get("dst").and_then(|v| v.as_str()) == Some(p2.as_str()));
                match existing {
                    Some(t) => {
                        t.insert("src", toml_edit::value(src.as_str()));
                    }
                    None => a.push(edits::table_from(&[
                        ("src", src.as_str().into()),
                        ("dst", p2.as_str().into()),
                        ("mode", m2.as_str().into()),
                        ("owner", o2.as_str().into()),
                    ])),
                }
            },
            cx,
        );
        self.recipe_paths.insert(path.clone());
        let b64 = base64::engine::general_purpose::STANDARD.encode(&content);
        if let Some(task) = self.call_bg(cx, "put", json!({ "path": path, "content_b64": b64, "mode": mode, "owner": owner })) {
            let dir = parent_of(&path);
            cx.spawn(async move |this, cx| {
                let r = task.await;
                let _ = this.update(cx, |this, cx| {
                    if let Err(e) = r {
                        this.toast(format!("Stage update failed: {e}"), cx);
                    }
                    this.load_dir(dir, cx);
                });
            })
            .detach();
        }
    }

    fn edit_recipe(&mut self, f: impl FnOnce(&mut toml_edit::DocumentMut), cx: &mut Context<Self>) {
        let id = self.project_id.clone();
        if let Some(ws) = self.ws.upgrade() {
            ws.update(cx, |ws, cx| ws.edit_project(&id, f, cx));
        }
    }

    fn toast(&self, msg: String, cx: &mut Context<Self>) {
        if let Some(ws) = self.ws.upgrade() {
            ws.update(cx, |ws, cx| ws.toast(msg, cx));
        }
    }

    fn add_rm_step(&mut self, paths: &[String], step: &str, cx: &mut Context<Self>) {
        let lines: Vec<String> = paths.iter().map(|p| format!("rm -rf -- {}", crate::workspace::shell_quote(p))).collect();
        let step = step.to_string();
        self.edit_recipe(
            move |doc| {
                let a = edits::aot_mut(doc, "steps");
                let existing = a.iter_mut().find(|t| t.get("name").and_then(|v| v.as_str()) == Some(step.as_str()));
                match existing {
                    Some(t) => {
                        let run = t.get("run").and_then(|v| v.as_str()).unwrap_or("").trim_end().to_string();
                        let mut all: Vec<String> = run.lines().map(String::from).collect();
                        for l in &lines {
                            if !all.contains(l) {
                                all.push(l.clone());
                            }
                        }
                        t.insert("run", toml_edit::value(all.join("\n") + "\n"));
                    }
                    None => a.push(edits::table_from(&[("name", step.as_str().into()), ("run", (lines.join("\n") + "\n").into())])),
                }
            },
            cx,
        );
    }

    fn delete(&mut self, e: Entry, window: &mut Window, cx: &mut Context<Self>) {
        let rx = window.prompt(
            PromptLevel::Warning,
            &format!("Delete {}?", e.path),
            Some("It is removed from the stage now, and a step in the recipe removes it on every build."),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if rx.await.ok() != Some(0) {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                this.add_rm_step(std::slice::from_ref(&e.path), STEP_REMOVED, cx);
                if let Some(task) = this.call_bg(cx, "rm", json!({ "path": e.path })) {
                    let dir = parent_of(&e.path);
                    cx.spawn(async move |this, cx| {
                        let r = task.await;
                        let _ = this.update(cx, |this, cx| {
                            if let Err(err) = r {
                                this.toast(format!("Delete failed: {err}"), cx);
                            }
                            this.selected = None;
                            this.preview = None;
                            this.load_dir(dir, cx);
                        });
                    })
                    .detach();
                }
            });
        })
        .detach();
    }

    fn new_file(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let name = name.trim().to_string();
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return;
        }
        let path = join(&self.cwd, &name);
        let mode = if name.ends_with(".sh") { "0755" } else { "0644" };
        self.record_file(path.clone(), Vec::new(), mode.into(), "root:root".into(), cx);
        let meta = Entry {
            name,
            path,
            kind: "file".into(),
            mode: mode.into(),
            mode_str: String::new(),
            owner: "root:root".into(),
            size: 0,
            mtime: 0,
            target: None,
        };
        self.open_editor(meta, String::new(), window, cx);
    }

    pub fn request_capture(&mut self, cx: &mut Context<Self>) {
        if self.client.is_some() {
            self.start_capture(cx);
        } else {
            self.pending_capture = true;
        }
    }

    fn open_editor(&mut self, meta: Entry, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let area = cx.new(|cx| TextArea::new(text, cx));
        let sub = cx.subscribe(&area, |this: &mut FilesView, _, e: &TextAreaEvent, cx| match e {
            TextAreaEvent::Save => this.save_editor(cx),
            TextAreaEvent::Changed => cx.notify(),
        });
        window.focus(&area.focus_handle(cx));
        self.editor = Some((meta, area, sub));
        cx.notify();
    }

    fn save_editor(&mut self, cx: &mut Context<Self>) {
        let Some((meta, area, _)) = &self.editor else { return };
        if self.busy(cx) {
            self.toast("A build is running; save again when it finishes".into(), cx);
            return;
        }
        let text = area.read(cx).text().to_string();
        area.update(cx, |a, cx| {
            a.dirty = false;
            cx.notify();
        });
        let (path, mode, owner) = (meta.path.clone(), meta.mode.clone(), meta.owner.clone());
        if let Some(Preview::Text { meta: m, text: t, .. }) = &mut self.preview {
            if m.path == path {
                *t = text.clone();
            }
        }
        self.record_file(path.clone(), text.into_bytes(), mode, owner, cx);
        self.toast(format!("Saved {path} into the recipe"), cx);
    }

    fn close_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = self.editor.as_ref().is_some_and(|(_, a, _)| a.read(cx).dirty);
        if !dirty {
            self.editor = None;
            cx.notify();
            return;
        }
        let rx = window.prompt(PromptLevel::Warning, "Discard unsaved changes?", None, &["Discard", "Keep Editing"], cx);
        cx.spawn(async move |this, cx| {
            if rx.await.ok() == Some(0) {
                let _ = this.update(cx, |this, cx| {
                    this.editor = None;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn start_capture(&mut self, cx: &mut Context<Self>) {
        let Some(task) = self.call_bg(cx, "diff", json!({})) else { return };
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |this, cx| {
                match r {
                    Ok(v) => {
                        let list: Vec<(String, String, bool)> = v["changes"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .map(|c| {
                                        let kind = c["after"]["type"].as_str().or(c["before"]["type"].as_str()).unwrap_or("file").to_string();
                                        (c["path"].as_str().unwrap_or("").to_string(), format!("{} {}", c["change"].as_str().unwrap_or(""), kind), true)
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        if list.is_empty() {
                            this.toast("No uncaptured changes in the stage".into(), cx);
                            this.capture = None;
                        } else {
                            this.capture = Some(list);
                        }
                    }
                    Err(e) => this.toast(format!("Diff failed: {e}"), cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn import_capture(&mut self, cx: &mut Context<Self>) {
        let Some(list) = self.capture.take() else { return };
        let Some(client) = self.client.clone() else { return };
        let chosen: Vec<(String, String)> = list.into_iter().filter(|(_, _, keep)| *keep).map(|(p, k, _)| (p, k)).collect();
        let task = cx.background_spawn(async move {
            let mut files = Vec::new();
            let mut removed = Vec::new();
            let mut extra = Vec::new();
            for (path, kind) in chosen {
                if kind.starts_with("removed") {
                    removed.push(path);
                } else if kind.ends_with(" file") {
                    let st = client.call("stat", json!({ "path": path }))?;
                    let meta = Entry::from(&st);
                    let c = client.call("cat", json!({ "path": path, "max": 256u64 << 20 }))?;
                    let bytes = base64::engine::general_purpose::STANDARD.decode(c["content_b64"].as_str().unwrap_or("")).unwrap_or_default();
                    files.push((path, bytes, meta.mode, meta.owner));
                } else if kind.ends_with(" dir") {
                    extra.push(format!("mkdir -p -- {}", crate::workspace::shell_quote(&path)));
                } else if kind.ends_with(" symlink") {
                    let st = client.call("stat", json!({ "path": path }))?;
                    if let Some(t) = st["target"].as_str() {
                        extra.push(format!("ln -sfn -- {} {}", crate::workspace::shell_quote(t), crate::workspace::shell_quote(&path)));
                    }
                }
            }
            Ok::<_, String>((files, removed, extra))
        });
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |this, cx| match r {
                Ok((files, removed, extra)) => {
                    let n = files.len() + removed.len() + extra.len();
                    for (p, b, m, o) in files {
                        this.record_file(p, b, m, o, cx);
                    }
                    if !removed.is_empty() {
                        this.add_rm_step(&removed, STEP_SHELL, cx);
                    }
                    if !extra.is_empty() {
                        let run = extra.join("\n") + "\n";
                        this.edit_recipe(move |d| edits::aot_push(d, "steps", edits::table_from(&[("name", STEP_SHELL.into()), ("run", run.as_str().into())])), cx);
                    }
                    this.toast(format!("Captured {n} change(s) into the recipe"), cx);
                    cx.notify();
                }
                Err(e) => this.toast(format!("Capture failed: {e}"), cx),
            });
        })
        .detach();
        cx.notify();
    }

    // ------------------------------------------------------------ render

    fn badge(&self, path: &str, c: &Colors) -> Option<AnyElement> {
        let (label, fg) = if self.recipe_paths.contains(path) {
            ("Recipe", c.accent)
        } else {
            match self.changes.get(path).map(String::as_str) {
                Some("added") => ("Added", c.success),
                Some("modified") => ("Modified", c.accent),
                Some("removed") => ("Removed", c.danger),
                _ => return None,
            }
        };
        Some(pill(label, fg, fg.opacity(0.1)).into_any_element())
    }

    fn is_changed_or_parent(&self, path: &str) -> bool {
        let prefix = if path == "/" { "/".to_string() } else { format!("{path}/") };
        self.recipe_paths.contains(path)
            || self.changes.contains_key(path)
            || self.changes.keys().any(|k| k.starts_with(&prefix))
            || self.recipe_paths.iter().any(|k| k.starts_with(&prefix))
    }

    fn render_tree(&self, path: &str, depth: usize, c: &Colors, out: &mut Vec<AnyElement>, cx: &mut Context<Self>) {
        let Some(kids) = self.children.get(path) else { return };
        for e in kids.iter().filter(|e| e.is_dir()) {
            if self.changed_only && !self.is_changed_or_parent(&e.path) {
                continue;
            }
            let open = self.expanded.contains(&e.path);
            let sel = self.cwd == e.path;
            let p = e.path.clone();
            let hover = c.hover;
            out.push(
                div()
                    .id(SharedString::from(format!("tree-{}", e.path)))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .pl(px(8. + depth as f32 * 14.))
                    .h(px(22.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .text_size(px(12.5))
                    .whitespace_nowrap()
                    .when(sel, |d| d.bg(c.selected).font_weight(gpui::FontWeight::SEMIBOLD))
                    .when(!sel, move |d| d.hover(move |s| s.bg(hover)))
                    .child(div().w(px(10.)).text_size(px(9.)).text_color(c.muted).child(if open { "▾" } else { "▸" }))
                    .child(e.name.clone())
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_tree(p.clone(), cx)))
                    .into_any_element(),
            );
            if open {
                self.render_tree(&e.path, depth + 1, c, out, cx);
            }
        }
    }

    fn row(&self, ix: usize, e: &Entry, removed: bool, c: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let sel = self.selected.as_deref() == Some(e.path.as_str());
        let e2 = e.clone();
        let hover = c.hover;
        let icon = match e.kind.as_str() {
            "dir" => "▸",
            "symlink" => "↪",
            _ => " ",
        };
        div()
            .id(("file-row", ix))
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(10.))
            .h(px(24.))
            .cursor_pointer()
            .text_size(px(12.5))
            .when(sel, |d| d.bg(c.selected))
            .when(!sel, move |d| d.hover(move |s| s.bg(hover)))
            .when(removed, |d| d.text_color(c.faint).line_through())
            .child(div().w(px(12.)).text_color(c.muted).child(icon))
            .child(div().flex_1().overflow_hidden().whitespace_nowrap().text_ellipsis().when(e.is_dir(), |d| d.font_weight(gpui::FontWeight::MEDIUM)).child(e.name.clone()))
            .child(div().w(px(90.)).font_family(MONO).text_size(px(11.5)).child(e.mode_str.clone()))
            .child(div().w(px(110.)).overflow_hidden().whitespace_nowrap().child(e.owner.clone()))
            .child(div().w(px(70.)).text_color(c.muted).child(if e.is_dir() { String::new() } else { human(e.size) }))
            .child(div().w(px(120.)).text_color(c.muted).child(fmt_mtime(e.mtime)))
            .child(div().w(px(76.)).children(self.badge(&e.path, c)))
            .when(!removed, |d| {
                d.on_click(cx.listener(move |this, ev: &ClickEvent, w, cx| {
                    if ev.click_count() >= 2 {
                        if e2.is_dir() {
                            this.navigate(e2.path.clone(), cx);
                        } else if let Some(Preview::Text { meta, text, truncated: false }) = &this.preview {
                            if !this.busy(cx) {
                                let (m, t) = (meta.clone(), text.clone());
                                this.open_editor(m, t, w, cx);
                            }
                        }
                    } else {
                        this.select(e2.clone(), cx);
                    }
                }))
            })
            .into_any_element()
    }

    fn render_preview(&self, c: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.busy(cx);
        let Some(p) = &self.preview else {
            return div().p(px(12.)).text_color(c.faint).child("Select a file to preview").into_any_element();
        };
        let header = |meta: &Entry, extra: String| {
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(px(12.))
                .text_color(c.muted)
                .child(div().font_family(MONO).text_color(c.text).child(meta.path.clone()))
                .child(format!("· {} · {} · {}{extra}", meta.mode_str, meta.owner, human(meta.size)))
        };
        let from_recipe = |meta: &Entry| if self.recipe_paths.contains(&meta.path) { " · from recipe" } else { "" };
        match p {
            Preview::Loading => div().p(px(12.)).text_color(c.muted).child("Loading …").into_any_element(),
            Preview::Error(e) => div().p(px(12.)).text_color(c.danger).child(e.clone()).into_any_element(),
            Preview::Dir(e) => div().p(px(12.)).child(header(e, String::new())).into_any_element(),
            Preview::Link { meta, resolved } => {
                let target = meta.target.clone().unwrap_or_default();
                let go = resolved.clone();
                div()
                    .p(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(header(meta, from_recipe(meta).into()))
                    .child(div().flex().items_center().gap(px(8.)).child(format!("→ {target}")).when_some(go, |d, g| {
                        d.child(button("goto-target", "Go to Target", Kind::Normal, true, c).on_click(cx.listener(move |this, _, _, cx| {
                            this.navigate(parent_of(&g), cx);
                            this.selected = Some(g.clone());
                            cx.notify();
                        })))
                    }))
                    .into_any_element()
            }
            Preview::Binary { meta, describe, sha } => div()
                .p(px(12.))
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(header(meta, from_recipe(meta).into()))
                .child(div().child(describe.clone()))
                .children(sha.clone().map(|s| div().font_family(MONO).text_size(px(11.5)).text_color(c.muted).child(format!("SHA-256 {s}"))))
                .into_any_element(),
            Preview::Text { meta, text, truncated } => {
                let (m1, t1) = (meta.clone(), text.clone());
                let m2 = meta.clone();
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(12.))
                            .py(px(6.))
                            .border_b_1()
                            .border_color(c.border)
                            .child(div().flex_1().child(header(meta, format!("{}{}", from_recipe(meta), if *truncated { " · first 1 MB" } else { "" }))))
                            .child(button("edit-file", "Edit", Kind::Primary, !busy && !truncated, c).tooltip(tooltip("Edit this file (⌘E)")).on_click_if(!busy && !truncated, cx.listener(move |this, _, w, cx| {
                                this.open_editor(m1.clone(), t1.clone(), w, cx)
                            })))
                            .child(button("delete-file", "Delete", Kind::Danger, !busy, c).on_click_if(!busy, cx.listener(move |this, _, w, cx| this.delete(m2.clone(), w, cx)))),
                    )
                    .child(
                        div()
                            .id("preview-text")
                            .flex_1()
                            .overflow_scroll()
                            .p(px(10.))
                            .bg(c.log_bg)
                            .font_family(MONO)
                            .text_size(px(12.))
                            .line_height(px(17.))
                            .children(text.lines().take(5000).map(|l| div().whitespace_nowrap().child(l.replace('\t', "    ")))),
                    )
                    .into_any_element()
            }
        }
    }
}

impl Render for FilesView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = Colors::get(window, cx);
        self.sync_recipe_paths(cx);
        let busy = self.busy(cx);
        let center = |content: AnyElement| div().size_full().flex().flex_col().items_center().justify_center().gap(px(10.)).child(content);
        match &self.status {
            Status::Connecting => return center(div().text_color(c.muted).child("Connecting to the store …").into_any_element()).into_any_element(),
            Status::Error(e) => {
                return center(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(10.))
                        .child(div().text_color(c.danger).max_w(px(560.)).child(e.clone()))
                        .child(button("files-retry", "Retry", Kind::Normal, true, &c).on_click(cx.listener(|this, _, _, cx| this.refresh(cx))))
                        .into_any_element(),
                )
                .into_any_element()
            }
            Status::NoStage => {
                let ws = self.ws.clone();
                return center(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(10.))
                        .child(div().text_size(px(16.)).font_weight(gpui::FontWeight::SEMIBOLD).child("No stage yet"))
                        .child(div().text_color(c.muted).child("Extract the base image to browse its root filesystem before the first build."))
                        .child(button("extract", if busy { "Extracting …" } else { "Extract Base to Browse" }, Kind::Primary, !busy, &c).on_click_if(!busy, move |_, w, cx| {
                            let _ = ws.update(cx, |ws, cx| ws.request_build(true, w, cx));
                        }))
                        .into_any_element(),
                )
                .into_any_element();
            }
            Status::Ready => {}
        }

        let stale = self.ws.upgrade().and_then(|w| {
            let ws = w.read(cx);
            let p = ws.projects.iter().find(|p| p.id() == self.project_id)?;
            let last = p.builds.first()?;
            let built = std::fs::metadata(last.dir.join("build.json")).and_then(|m| m.modified()).ok()?;
            (p.mtime? > built).then_some(())
        });

        // Breadcrumbs.
        let mut crumbs = div().flex().items_center().gap(px(2.)).text_size(px(12.5));
        let mut acc = String::new();
        let parts: Vec<String> = std::iter::once("/".to_string()).chain(self.cwd.split('/').filter(|s| !s.is_empty()).map(String::from)).collect();
        for (i, part) in parts.iter().enumerate() {
            if i > 0 {
                acc = join(if acc.is_empty() { "/" } else { &acc }, part);
                crumbs = crumbs.child(div().text_color(c.faint).child("›"));
            } else {
                acc = "/".into();
            }
            let target = acc.clone();
            crumbs = crumbs.child(
                div()
                    .id(("crumb", i))
                    .px(px(4.))
                    .rounded(px(3.))
                    .cursor_pointer()
                    .hover(|s| s.bg(c.hover))
                    .child(part.clone())
                    .on_click(cx.listener(move |this, _, _, cx| this.navigate(target.clone(), cx))),
            );
        }
        let changed_only = self.changed_only;
        let toolbar = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .px(px(14.))
            .h(px(40.))
            .border_b_1()
            .border_color(c.border)
            .child(crumbs)
            .child(div().flex_1())
            .child(div().w(px(200.)).child(self.search.clone()))
            .child(
                div()
                    .id("changed-only")
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .text_size(px(12.5))
                    .child(div().w(px(26.)).h(px(15.)).rounded(px(8.)).bg(if changed_only { c.accent } else { c.border }).flex().when(changed_only, |d| d.justify_end()).child(div().size(px(13.)).m(px(1.)).rounded_full().bg(gpui::white())))
                    .child("Changed only")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.changed_only = !this.changed_only;
                        cx.notify();
                    })),
            )
            .child(button("refresh-files", "Refresh", Kind::Ghost, true, &c).on_click(cx.listener(|this, _, _, cx| this.refresh(cx))));

        let mut banners = Vec::new();
        if busy {
            banners.push(("A build is running; the Files tab is read-only until it finishes.".to_string(), c.info_soft, None));
        } else if self.partial {
            banners.push(("Partial build: the last build was interrupted, so this tree may be incomplete.".to_string(), c.warning_soft, None));
        }
        if stale.is_some() && !busy {
            banners.push(("The recipe changed since this tree was built; it reflects the last build.".to_string(), c.info_soft, Some("build")));
        }
        let ws = self.ws.clone();
        let banners = banners.into_iter().enumerate().map(|(i, (msg, bg, action))| {
            let ws = ws.clone();
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(14.))
                .py(px(6.))
                .bg(bg)
                .text_size(px(12.5))
                .child(div().flex_1().child(msg))
                .when(action.is_some(), |d| {
                    d.child(button(("banner-build", i), "Build", Kind::Normal, true, &c).on_click(move |_, w, cx| {
                        let _ = ws.update(cx, |ws, cx| ws.request_build(false, w, cx));
                    }))
                })
        });

        let mut tree = Vec::new();
        tree.push(
            div()
                .id("tree-root")
                .px(px(8.))
                .h(px(22.))
                .flex()
                .items_center()
                .rounded(px(4.))
                .cursor_pointer()
                .text_size(px(12.5))
                .when(self.cwd == "/", |d| d.bg(c.selected).font_weight(gpui::FontWeight::SEMIBOLD))
                .child("/")
                .on_click(cx.listener(|this, _, _, cx| this.navigate_only("/".into(), cx)))
                .into_any_element(),
        );
        self.render_tree("/", 1, &c, &mut tree, cx);

        // File list for the current directory (or search results).
        let mut rows = Vec::new();
        let listing: Vec<Entry> = match &self.results {
            Some(r) => r.clone(),
            None => self.children.get(&self.cwd).cloned().unwrap_or_default(),
        };
        for (i, e) in listing.iter().enumerate() {
            if self.changed_only && self.results.is_none() && !self.is_changed_or_parent(&e.path) {
                continue;
            }
            let mut e = e.clone();
            if self.results.is_some() {
                e.name = e.path.clone();
            }
            rows.push(self.row(i, &e, false, &c, cx));
        }
        if self.results.is_none() {
            let prefix = if self.cwd == "/" { "/".to_string() } else { format!("{}/", self.cwd) };
            let mut removed: Vec<&String> = self
                .changes
                .iter()
                .filter(|(p, k)| *k == "removed" && p.starts_with(&prefix) && !p[prefix.len()..].contains('/'))
                .map(|(p, _)| p)
                .collect();
            removed.sort();
            for (j, p) in removed.into_iter().enumerate() {
                let e = Entry {
                    name: p[prefix.len()..].to_string(),
                    path: p.clone(),
                    kind: "file".into(),
                    mode: String::new(),
                    mode_str: String::new(),
                    owner: String::new(),
                    size: 0,
                    mtime: 0,
                    target: None,
                };
                rows.push(self.row(100_000 + j, &e, true, &c, cx));
            }
        }
        let header = div()
            .flex()
            .gap(px(10.))
            .px(px(10.))
            .h(px(24.))
            .items_center()
            .border_b_1()
            .border_color(c.border)
            .text_size(px(11.))
            .text_color(c.muted)
            .child(div().w(px(12.)))
            .child(div().flex_1().child("NAME"))
            .child(div().w(px(90.)).child("MODE"))
            .child(div().w(px(110.)).child("OWNER"))
            .child(div().w(px(70.)).child("SIZE"))
            .child(div().w(px(120.)).child("MODIFIED"))
            .child(div().w(px(76.)).child("CHANGE"));

        let capture = self.capture.clone().map(|list| {
            let mut col = div().flex().flex_col().gap(px(2.)).max_h(px(220.));
            for (i, (p, kind, keep)) in list.iter().enumerate() {
                col = col.child(
                    div()
                        .id(("cap", i))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .cursor_pointer()
                        .child(crate::widgets::checkbox(("cap-cb", i), *keep, "", &c))
                        .child(div().w(px(130.)).text_size(px(12.)).text_color(c.muted).child(kind.clone()))
                        .child(div().font_family(MONO).text_size(px(12.)).child(p.clone()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(l) = &mut this.capture {
                                l[i].2 = !l[i].2;
                            }
                            cx.notify();
                        })),
                );
            }
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .p(px(12.))
                .border_b_1()
                .border_color(c.border)
                .bg(c.surface_alt)
                .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("Capture changes made in the Shell"))
                .child(div().id("cap-list").overflow_y_scroll().child(col))
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(button("cap-import", "Import Selected into Recipe", Kind::Primary, true, &c).on_click(cx.listener(|this, _, _, cx| this.import_capture(cx))))
                        .child(button("cap-discard", "Discard", Kind::Normal, true, &c).on_click(cx.listener(|this, _, _, cx| {
                            this.capture = None;
                            cx.notify();
                        }))),
                )
        });

        let editor = self.editor.as_ref().map(|(meta, area, _)| {
            let dirty = area.read(cx).dirty;
            div()
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .bg(c.surface)
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap(px(10.))
                        .px(px(14.))
                        .h(px(42.))
                        .border_b_1()
                        .border_color(c.border)
                        .child(div().font_family(MONO).font_weight(gpui::FontWeight::SEMIBOLD).child(meta.path.clone()))
                        .child(div().text_size(px(12.)).text_color(c.muted).child(format!("{} · {}", meta.mode, meta.owner)))
                        .when(dirty, |d| d.child(div().text_size(px(12.)).text_color(c.warning).child("● unsaved")))
                        .child(div().flex_1())
                        .child(div().text_size(px(12.)).text_color(c.muted).child("Saved to files/rootfs and recorded as a [[files]] entry"))
                        .child(button("ed-close", "Close", Kind::Normal, true, &c).on_click(cx.listener(|this, _, w, cx| this.close_editor(w, cx))))
                        .child(button("ed-save", "Save", Kind::Primary, !busy, &c).tooltip(tooltip("Save (⌘S)")).on_click_if(!busy, cx.listener(|this, _, _, cx| this.save_editor(cx)))),
                )
                .child(div().flex_1().min_h_0().child(area.clone()))
        });

        let footer = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(12.))
            .h(px(38.))
            .border_t_1()
            .border_color(c.border)
            .child(div().w(px(180.)).child(self.new_name.clone()))
            .child(button("new-file", "New File", Kind::Normal, !busy, &c).on_click_if(!busy, cx.listener(|this, _, w, cx| {
                let n = this.new_name.read(cx).text();
                this.new_file(n, w, cx);
                this.new_name.update(cx, |i, cx| i.set_text("", cx));
            })))
            .child(div().flex_1())
            .child(button("capture", "Capture Shell Changes…", Kind::Normal, !busy, &c).on_click_if(!busy, cx.listener(|this, _, _, cx| this.start_capture(cx))));

        div()
            .flex()
            .flex_col()
            .size_full()
            .relative()
            .key_context("Files")
            .on_action(cx.listener(|this, _: &EditFile, w, cx| {
                if this.busy(cx) || this.editor.is_some() {
                    return;
                }
                if let Some(Preview::Text { meta, text, truncated: false }) = &this.preview {
                    let (m, t) = (meta.clone(), text.clone());
                    this.open_editor(m, t, w, cx);
                }
            }))
            .child(toolbar)
            .children(banners)
            .children(capture)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(div().id("tree").w(px(220.)).flex_none().h_full().p(px(6.)).border_r_1().border_color(c.border).overflow_y_scroll().children(tree))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(header)
                            .child(div().id("file-list").flex_1().min_h_0().overflow_y_scroll().children(rows))
                            .child(div().h(px(240.)).flex_none().border_t_1().border_color(c.border).child(self.render_preview(&c, cx)))
                            .child(footer),
                    ),
            )
            .children(editor)
            .into_any_element()
    }
}

fn fmt_mtime(t: i64) -> String {
    if t <= 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(t, 0).map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

impl Workspace {
    pub fn edit_project(&mut self, id: &str, f: impl FnOnce(&mut toml_edit::DocumentMut), cx: &mut Context<Self>) {
        let Some(p) = self.projects.iter_mut().find(|p| p.id() == id) else { return };
        if let Err(e) = p.edit(f) {
            self.toast(format!("Edit not applied: {e}"), cx);
        }
        self.recipe_ui.dirty = true;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn paths() {
        assert_eq!(super::parent_of("/etc/ssh"), "/etc");
        assert_eq!(super::parent_of("/etc"), "/");
        assert_eq!(super::join("/", "etc"), "/etc");
        assert_eq!(super::fmt_mtime(0), "");
        assert_eq!(super::fmt_mtime(1_700_000_000).len(), 16);
    }
}
