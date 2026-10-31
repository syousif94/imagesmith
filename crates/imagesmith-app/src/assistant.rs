//! Assistant sidebar: chat with a model (OpenRouter or LM Studio) that edits
//! the active project's recipe through tool calls. Every change records a
//! before/after snapshot of recipe.toml so it can be undone and redone.
//! Conversations are kept per project in the app's support folder.

use crate::edits;
use crate::text_input::{InputEvent, TextInput};
use crate::theme::{settings, Colors, MONO};
use crate::widgets::{button, tooltip, ClickIf, Kind};
use crate::workspace::{OpenSettings, ProjectState, Workspace};
use gpui::prelude::*;
use gpui::{anchored, deferred, div, px, AnyElement, Context, Entity, Focusable, PromptLevel, ScrollHandle, Subscription, Window};
use imagesmith_core::settings::AiProvider;
use imagesmith_core::validate::Severity;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

/// The curl process of an in-flight request, so Stop can kill it.
type CurlSlot = Arc<Mutex<Option<Child>>>;

enum StreamMsg {
    Delta(String),
    Reasoning(String),
    Done(Result<Value, String>),
}

const ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";
const MAX_ROUNDS: usize = 16;
const SECTIONS: [&str; 6] = ["image", "size", "system", "packages", "services", "output"];
const LISTS: [&str; 4] = ["packages.install", "packages.remove", "services.enable", "services.disable"];
const KINDS: [&str; 4] = ["users", "repos", "files", "steps"];

#[derive(Serialize, Deserialize)]
pub struct ToolRecord {
    pub call_id: String,
    pub summary: String,
    pub result: Result<String, String>,
    /// recipe.toml before and after, when the call changed it.
    pub change: Option<(String, String)>,
    pub undone: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
pub enum RunState {
    Pending,
    Running,
    Skipped,
    Stopped,
    Exited(Option<i32>),
    Failed,
}

/// A script the model asked to run in the project's stage.
#[derive(Serialize, Deserialize)]
pub struct CommandRecord {
    pub call_id: String,
    pub script: String,
    pub state: RunState,
    pub output: String,
}

#[derive(Serialize, Deserialize)]
pub enum Item {
    User(String),
    /// Model reasoning, with how many seconds it took.
    Thinking(String, Option<u64>),
    Assistant(String),
    Tool(ToolRecord),
    Command(CommandRecord),
    Error(String),
}

type ToolResults = HashMap<String, (String, Result<String, String>)>;

/// Tool calls waiting for the user to approve the command at `item`.
struct Paused {
    round: usize,
    calls: Vec<Value>,
    done: ToolResults,
    item: usize,
}

#[derive(Serialize, Deserialize)]
pub struct Chat {
    pub id: u64,
    pub created: String,
    pub items: Vec<Item>,
    /// OpenAI-format history, without the system message.
    api: Vec<Value>,
    #[serde(skip)]
    pub busy: bool,
    #[serde(skip)]
    generation: u64,
    /// Assistant text arriving for the current round.
    #[serde(skip)]
    pub streaming: Option<String>,
    /// Reasoning tokens arriving for the current round.
    #[serde(skip)]
    pub streaming_reasoning: Option<String>,
    /// When reasoning started, and how long it took once the answer began.
    #[serde(skip)]
    reasoning_clock: Option<(std::time::Instant, Option<u64>)>,
    #[serde(skip)]
    curl: Option<CurlSlot>,
    #[serde(skip)]
    paused: Option<Paused>,
}

impl Chat {
    fn new() -> Chat {
        let id = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0);
        Chat {
            id,
            created: chrono::Local::now().format("%b %-d, %H:%M").to_string(),
            items: Vec::new(),
            api: Vec::new(),
            busy: false,
            generation: 0,
            streaming: None,
            streaming_reasoning: None,
            reasoning_clock: None,
            curl: None,
            paused: None,
        }
    }

    pub fn title(&self) -> String {
        let first = self.items.iter().find_map(|i| match i {
            Item::User(t) => Some(t.lines().next().unwrap_or("").trim().to_string()),
            _ => None,
        });
        match first {
            Some(t) if t.chars().count() > 48 => format!("{}…", t.chars().take(47).collect::<String>()),
            Some(t) if !t.is_empty() => t,
            _ => "New Chat".into(),
        }
    }
}

/// A project's conversations, oldest first.
pub struct ProjectChats {
    pub list: Vec<Chat>,
    pub current: usize,
}

impl ProjectChats {
    fn load(pid: &str) -> ProjectChats {
        let mut list: Vec<Chat> = std::fs::read(chats_file(pid)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        for item in list.iter_mut().flat_map(|c| c.items.iter_mut()) {
            if let Item::Command(r) = item {
                if matches!(r.state, RunState::Pending | RunState::Running) {
                    r.state = RunState::Stopped;
                }
            }
        }
        if list.last().is_none_or(|c| !c.items.is_empty()) {
            list.push(Chat::new());
        }
        let current = list.len() - 1;
        ProjectChats { list, current }
    }

    fn save(&self, pid: &str) {
        let keep: Vec<&Chat> = self.list.iter().filter(|c| !c.items.is_empty()).collect();
        let path = chats_file(pid);
        let tmp = path.with_extension("json.tmp");
        let r = std::fs::create_dir_all(imagesmith_core::app_dirs::chats_dir())
            .and_then(|_| std::fs::write(&tmp, serde_json::to_vec(&keep).unwrap_or_default()))
            .and_then(|_| std::fs::rename(&tmp, &path));
        if let Err(e) = r {
            eprintln!("could not save chats: {e}");
        }
    }

    pub fn current(&self) -> &Chat {
        &self.list[self.current]
    }
}

fn chats_file(pid: &str) -> std::path::PathBuf {
    let safe: String = pid.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    imagesmith_core::app_dirs::chats_dir().join(format!("{safe}.json"))
}

pub struct AssistantUi {
    pub open: bool,
    pub chats: HashMap<String, ProjectChats>,
    pub history_open: bool,
    /// Thinking boxes shown in full, keyed by (chat id, item index); usize::MAX is the live one.
    pub expanded: std::collections::HashSet<(u64, usize)>,
    think_scrolls: HashMap<(u64, usize), ScrollHandle>,
    pub input: Entity<TextInput>,
    pub model_picker: Entity<crate::model_picker::ModelPicker>,
    pub scroll: ScrollHandle,
    _sub: Subscription,
}

impl AssistantUi {
    pub fn new(cx: &mut Context<Workspace>) -> AssistantUi {
        let input = cx.new(|cx| TextInput::new(cx, "Ask for a change…").multiline(8));
        let sub = cx.subscribe(&input, |this: &mut Workspace, _, e: &InputEvent, cx| {
            if matches!(e, InputEvent::Submit) {
                this.send_chat(cx);
            }
        });
        let model_picker = cx.new(|cx| crate::model_picker::ModelPicker::new(320., cx));
        AssistantUi {
            open: false,
            chats: HashMap::new(),
            history_open: false,
            expanded: Default::default(),
            think_scrolls: HashMap::new(),
            input,
            model_picker,
            scroll: ScrollHandle::new(),
            _sub: sub,
        }
    }

    fn project(&mut self, pid: &str) -> &mut ProjectChats {
        self.chats.entry(pid.to_string()).or_insert_with(|| ProjectChats::load(pid))
    }

    fn chat_mut(&mut self, pid: &str, cid: u64) -> Option<&mut Chat> {
        self.chats.get_mut(pid)?.list.iter_mut().find(|c| c.id == cid)
    }

    fn save(&self, pid: &str) {
        if let Some(p) = self.chats.get(pid) {
            p.save(pid);
        }
    }
}

// ---------------------------------------------------------------- tools

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": { "type": "object", "properties": properties, "required": required },
        }
    })
}

/// Runs a tool that needs the network; called off the UI thread.
fn run_network_tool(name: &str, args: &Value, search: Option<&crate::web_search::Backend>) -> (String, Result<String, String>) {
    match name {
        "web_search" => {
            let q = args["query"].as_str().unwrap_or("").trim().to_string();
            let Some(b) = search else { return (format!("Searched for “{q}”"), Err("web search is turned off in Settings".into())) };
            let domains: Vec<String> = args["domains"].as_array().into_iter().flatten().filter_map(|d| d.as_str().map(String::from)).collect();
            match crate::web_search::search(b, &q, args["count"].as_u64().unwrap_or(5), &domains) {
                Ok((text, n)) => (format!("Searched {} for “{q}” · {n} result{}", b.label(), if n == 1 { "" } else { "s" }), Ok(text)),
                Err(e) => (format!("Searched {} for “{q}”", b.label()), Err(e)),
            }
        }
        _ => {
            let u = args["url"].as_str().unwrap_or("");
            (format!("Checked {u}"), check_url(u))
        }
    }
}

fn tools(search: bool) -> Value {
    let mut list = base_tools();
    if search {
        list.as_array_mut().unwrap().push(tool(
            "web_search",
            "Search the web. Use it to find official install instructions, e.g. a vendor's apt repository URL, suite, components and signing-key URL, before adding them. If results are off-topic, refine the query or limit it to the vendor's domains and search again.",
            json!({
                "query": { "type": "string" },
                "count": { "type": "integer", "description": "Results to return, 1-10 (default 5)" },
                "domains": { "type": "array", "items": { "type": "string" }, "description": "Only return results from these sites, e.g. [\"nodesource.com\", \"github.com\"]" },
            }),
            &["query"],
        ));
    }
    list
}

fn base_tools() -> Value {
    let kind = json!({ "type": "string", "enum": KINDS });
    let fields = json!({ "type": "object", "description": "Field names and values as in recipe.toml. Multi-line strings are fine." });
    json!([
        tool("get_recipe", "Return the current recipe.toml and its validation problems.", json!({}), &[]),
        tool(
            "identify_image",
            "Inspect the configured base image and report its path, SHA-256, partition layout, architecture, Linux distribution, and root filesystem details. Do not infer an image identity when probe data is unavailable.",
            json!({}),
            &[],
        ),
        tool(
            "set_field",
            "Set a key in a top-level recipe table. Pass value null to remove the key.",
            json!({
                "section": { "type": "string", "enum": SECTIONS },
                "key": { "type": "string" },
                "value": { "description": "String, number, boolean, array, or null to remove" },
            }),
            &["section", "key", "value"],
        ),
        tool(
            "add_to_list",
            "Add an item (a package or systemd unit) to a list.",
            json!({ "list": { "type": "string", "enum": LISTS }, "item": { "type": "string" } }),
            &["list", "item"],
        ),
        tool(
            "remove_from_list",
            "Remove an item from a list.",
            json!({ "list": { "type": "string", "enum": LISTS }, "item": { "type": "string" } }),
            &["list", "item"],
        ),
        tool(
            "add_entry",
            "Append a [[users]], [[repos]], [[files]] or [[steps]] entry.",
            json!({ "kind": kind, "fields": fields }),
            &["kind", "fields"],
        ),
        tool(
            "update_entry",
            "Change fields of an existing entry (0-based index). A null field removes it.",
            json!({ "kind": kind, "index": { "type": "integer" }, "fields": fields }),
            &["kind", "index", "fields"],
        ),
        tool("remove_entry", "Remove an entry (0-based index).", json!({ "kind": kind, "index": { "type": "integer" } }), &["kind", "index"]),
        tool(
            "replace_recipe",
            "Replace the whole recipe.toml. Only for large restructures; prefer the specific tools so comments survive.",
            json!({ "toml": { "type": "string" } }),
            &["toml"],
        ),
        tool(
            "run_command",
            "Run a bash script as root inside the project's stage: the image's root filesystem as left by the last build (or the extracted base), chrooted with /proc, /sys and /dev mounted, exactly like the user's Shell. The user must approve each run. Use it to inspect the image (installed packages, config files, service units) or to try a change before recording it. Output (stdout and stderr) and the exit status are returned. Stdin is /dev/null, so pass -y to apt. Changes stay in the stage only until the next build; they are not part of the recipe.",
            json!({ "script": { "type": "string", "description": "bash script" } }),
            &["script"],
        ),
        tool(
            "check_url",
            "Check that an http(s) URL exists and see its content type. Use it to verify repository and signing-key URLs before adding them; for an apt repository check {url}/dists/{suite}/Release.",
            json!({ "url": { "type": "string" } }),
            &["url"],
        ),
    ])
}

/// Fetches the first bytes of `url` and reports the status, type and final address.
fn check_url(url: &str) -> Result<String, String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("only http:// and https:// URLs can be checked".into());
    }
    let out = Command::new("/usr/bin/curl")
        .args(["-sS", "-L", "--max-time", "15", "-r", "0-511", "-o", "/dev/null", "-w", "%{http_code}\t%{content_type}\t%{url_effective}", url])
        .output()
        .map_err(|e| format!("could not run curl: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let mut parts = text.split('\t');
    let code: u16 = parts.next().and_then(|c| c.trim().parse().ok()).unwrap_or(0);
    let (ctype, effective) = (parts.next().unwrap_or(""), parts.next().unwrap_or(url));
    if code == 0 {
        return Err(format!("could not connect: {}", String::from_utf8_lossy(&out.stderr).trim().trim_start_matches("curl: ")));
    }
    let ctype = if ctype.is_empty() { "unknown type" } else { ctype };
    if (200..400).contains(&code) {
        Ok(format!("HTTP {code}, {ctype}, final URL {effective}"))
    } else {
        Err(format!("HTTP {code} ({ctype}) for {effective}"))
    }
}

fn str_arg<'a>(a: &'a Value, k: &str) -> Result<&'a str, String> {
    a.get(k).and_then(Value::as_str).ok_or_else(|| format!("missing string argument {k:?}"))
}

fn kind_arg(a: &Value) -> Result<&str, String> {
    let k = str_arg(a, "kind")?;
    KINDS.contains(&k).then_some(k).ok_or_else(|| format!("kind must be one of {}", KINDS.join(", ")))
}

fn list_arg(a: &Value) -> Result<(&'static str, &'static str), String> {
    let l = str_arg(a, "list")?;
    LISTS.iter().find(|x| **x == l).and_then(|x| x.split_once('.')).ok_or_else(|| format!("list must be one of {}", LISTS.join(", ")))
}

fn entry_count(p: &ProjectState, kind: &str) -> usize {
    let Some(rf) = &p.recipe else { return 0 };
    let r = &rf.recipe;
    match kind {
        "users" => r.users.len(),
        "repos" => r.repos.len(),
        "files" => r.files.len(),
        _ => r.steps.len(),
    }
}

fn entry_label(p: &ProjectState, kind: &str, i: usize) -> Option<String> {
    let r = &p.recipe.as_ref()?.recipe;
    match kind {
        "users" => r.users.get(i).map(|x| x.name.clone()),
        "repos" => r.repos.get(i).map(|x| x.name.clone()),
        "files" => r.files.get(i).map(|x| x.dst.clone()),
        _ => r.steps.get(i).map(|x| x.name.clone()),
    }
}

fn singular(kind: &str) -> &str {
    match kind {
        "users" => "user",
        "repos" => "repository",
        "files" => "file",
        _ => "step",
    }
}

fn fields_arg(a: &Value) -> Result<Vec<(String, Option<toml_edit::Value>)>, String> {
    let m = a.get("fields").and_then(Value::as_object).ok_or("missing object argument \"fields\"")?;
    m.iter().map(|(k, v)| Ok((k.clone(), if v.is_null() { None } else { Some(edits::json_to_value(v)?) }))).collect()
}

fn index_arg(a: &Value, p: &ProjectState, kind: &str) -> Result<usize, String> {
    let i = a.get("index").and_then(Value::as_u64).ok_or("missing integer argument \"index\"")? as usize;
    let n = entry_count(p, kind);
    (i < n).then_some(i).ok_or_else(|| format!("{kind}[{i}] does not exist; there are {n}"))
}

fn short(v: &Value) -> String {
    let s = match v {
        Value::String(s) => format!("{s:?}"),
        other => other.to_string(),
    };
    if s.chars().count() > 40 {
        format!("{}…", s.chars().take(39).collect::<String>())
    } else {
        s
    }
}

/// Recipe text and problems, as the model sees them.
fn recipe_report(p: &ProjectState) -> String {
    let mut s = format!("recipe.toml:\n```toml\n{}\n```\n", p.text.trim_end());
    if let Some((line, m)) = &p.parse_error {
        s.push_str(&format!("Parse error at line {line}: {m}\n"));
    } else if p.diagnostics.is_empty() {
        s.push_str("Validation: no problems.\n");
    } else {
        s.push_str("Validation problems:\n");
        for d in &p.diagnostics {
            let sev = if d.severity == Severity::Error { "error" } else { "warning" };
            s.push_str(&format!("- {sev} {}: {}\n", d.field, d.message));
        }
    }
    s
}

fn image_report(p: &ProjectState) -> Result<String, String> {
    let rf = p.recipe.as_ref().ok_or("the recipe has a parse error; the base image cannot be identified")?;
    let base = imagesmith_core::validate::resolve_project_path(&p.project.path, &rf.recipe.image.base);
    let Some(probe) = &p.probe else {
        return Err(format!(
            "no cached probe is available for the configured base image at {}. Open or probe the image in ImageSmith first.",
            base.display()
        ));
    };
    let rootfs = probe.rootfs.as_ref().map(|r| json!({
        "partition": r.partition,
        "architecture": r.arch,
        "distribution": r.distro,
        "distribution_id": r.distro_id,
        "codename": r.codename,
        "used_bytes": r.used_bytes,
        "resize_service": r.resize_service,
    }));
    serde_json::to_string_pretty(&json!({
        "configured_path": base,
        "sha256": probe.sha256,
        "image_bytes": probe.image_bytes,
        "partition_table": probe.table,
        "sector_size": probe.sector_size,
        "partitions": probe.partitions,
        "rootfs": rootfs,
    }))
    .map_err(|e| format!("could not serialize image probe: {e}"))
}

fn problems(p: &ProjectState) -> String {
    let errs: Vec<String> = p.diagnostics.iter().filter(|d| d.severity == Severity::Error).map(|d| format!("{}: {}", d.field, d.message)).collect();
    if errs.is_empty() {
        "OK.".into()
    } else {
        format!("Applied, but the recipe now has errors:\n{}", errs.join("\n"))
    }
}

/// Runs one tool call against `p`. Returns a one-line summary for the UI and the result for the model.
fn run_tool(p: &mut ProjectState, name: &str, a: &Value) -> (String, Result<String, String>) {
    let summary = match name {
        "get_recipe" => "Read the recipe".to_string(),
        "identify_image" => "Identified the base image".to_string(),
        "set_field" => match a.get("value") {
            Some(Value::Null) | None => format!("Cleared {}.{}", short_str(a, "section"), short_str(a, "key")),
            Some(v) => format!("Set {}.{} to {}", short_str(a, "section"), short_str(a, "key"), short(v)),
        },
        "add_to_list" => format!("Added {} to {}", short_str(a, "item"), short_str(a, "list")),
        "remove_from_list" => format!("Removed {} from {}", short_str(a, "item"), short_str(a, "list")),
        "add_entry" => {
            let k = short_str(a, "kind");
            let label = ["name", "dst"].iter().find_map(|f| a.get("fields")?.get(*f)?.as_str().map(String::from)).unwrap_or_default();
            format!("Added {} {label}", singular(&k))
        }
        "update_entry" | "remove_entry" => {
            let k = short_str(a, "kind");
            let i = a.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let label = entry_label(p, &k, i).map(|l| format!(" {l}")).unwrap_or_default();
            format!("{} {}{label}", if name == "update_entry" { "Changed" } else { "Removed" }, singular(&k))
        }
        "replace_recipe" => "Rewrote recipe.toml".to_string(),
        other => format!("Unknown tool {other}"),
    };
    (summary, execute(p, name, a))
}

fn short_str(a: &Value, k: &str) -> String {
    a.get(k).and_then(Value::as_str).unwrap_or("?").to_string()
}

fn execute(p: &mut ProjectState, name: &str, a: &Value) -> Result<String, String> {
    match name {
        "get_recipe" => return Ok(recipe_report(p)),
        "identify_image" => return image_report(p),
        "set_field" => {
            let section = str_arg(a, "section")?;
            if !SECTIONS.contains(&section) {
                return Err(format!("section must be one of {}", SECTIONS.join(", ")));
            }
            let key = str_arg(a, "key")?.to_string();
            let value = match a.get("value") {
                None | Some(Value::Null) => None,
                Some(v) => Some(edits::json_to_value(v)?),
            };
            p.edit(|d| match value {
                Some(v) => edits::set_value(d, section, &key, v),
                None => {
                    edits::table_mut(d, section).remove(&key);
                }
            })?;
        }
        "add_to_list" => {
            let (s, k) = list_arg(a)?;
            let item = str_arg(a, "item")?.trim().to_string();
            p.edit(|d| edits::push_str(d, s, k, &item))?;
        }
        "remove_from_list" => {
            let (s, k) = list_arg(a)?;
            let item = str_arg(a, "item")?;
            let mut found = false;
            p.edit(|d| found = edits::remove_str(d, s, k, item))?;
            if !found {
                return Err(format!("{item:?} is not in {s}.{k}"));
            }
        }
        "add_entry" => {
            let kind = kind_arg(a)?;
            let fields = fields_arg(a)?;
            p.edit(|d| {
                let mut t = toml_edit::Table::new();
                for (k, v) in fields.into_iter().filter_map(|(k, v)| Some((k, v?))) {
                    t.insert(&k, toml_edit::Item::Value(v));
                }
                edits::aot_push(d, kind, t);
            })?;
        }
        "update_entry" => {
            let kind = kind_arg(a)?;
            let i = index_arg(a, p, kind)?;
            let fields = fields_arg(a)?;
            p.edit(|d| {
                if let Some(t) = edits::aot_entry(d, kind, i) {
                    for (k, v) in fields {
                        match v {
                            Some(v) => {
                                t.insert(&k, toml_edit::Item::Value(v));
                            }
                            None => {
                                t.remove(&k);
                            }
                        }
                    }
                }
            })?;
        }
        "remove_entry" => {
            let kind = kind_arg(a)?;
            let i = index_arg(a, p, kind)?;
            p.edit(|d| edits::aot_remove(d, kind, i))?;
        }
        "replace_recipe" => {
            let text = str_arg(a, "toml")?;
            imagesmith_core::recipe::parse(text).map_err(|e| format!("not applied: {e}"))?;
            std::fs::write(p.project.recipe_path(), text).map_err(|e| e.to_string())?;
            p.reload();
        }
        other => return Err(format!("unknown tool {other}")),
    }
    Ok(problems(p))
}

/// Keeps the first `head` and last `tail` characters of `s`.
fn clip(s: &str, head: usize, tail: usize) -> String {
    let n = s.chars().count();
    if n <= head + tail {
        return s.to_string();
    }
    let a: String = s.chars().take(head).collect();
    let b: String = s.chars().skip(n - tail).collect();
    format!("{a}\n… {} characters omitted …\n{b}", n - head - tail)
}

/// The script as fed to the stage's login shell. The braces make bash read all of it
/// before running anything, so commands that read stdin cannot swallow the rest.
fn wrap_script(script: &str) -> String {
    format!("{{\nexport DEBIAN_FRONTEND=noninteractive\n{}\n}} </dev/null 2>&1\n", script.trim_end())
}

/// Runs `script` chrooted into the project's stage; returns the exit code and combined output.
fn run_in_stage(rt: &imagesmith_core::runner::Runtime, pid: &str, script: &str, slot: &CurlSlot) -> Result<(Option<i32>, String), String> {
    rt.ensure_store(&mut |_| {}).map_err(|e| e.to_string())?;
    let mut cmd = rt.exec(true, &["/usr/local/lib/imagesmith/shell.sh", pid]);
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start the shell: {e}"))?;
    let (Some(mut stdin), Some(mut stdout), Some(mut stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
        return Err("no shell pipes".into());
    };
    *slot.lock().unwrap() = Some(child);
    let input = wrap_script(script);
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let errs = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = stderr.read_to_end(&mut s);
        s
    });
    let mut out = Vec::new();
    let _ = stdout.read_to_end(&mut out);
    let _ = writer.join();
    let errs = errs.join().unwrap_or_default();
    let Some(mut child) = slot.lock().unwrap().take() else { return Err("Stopped.".into()) };
    let st = child.wait().map_err(|e| e.to_string())?;
    let text = format!("{}{}", String::from_utf8_lossy(&errs), String::from_utf8_lossy(&out));
    Ok((st.code(), clip(text.trim_end(), 4000, 16000)))
}

// ---------------------------------------------------------------- HTTP

/// Where a chat request goes.
struct Target {
    endpoint: String,
    key: Option<String>,
    /// Provider name for error messages.
    label: &'static str,
    local: bool,
}

/// Streams a chat completion with the system curl, which keeps the bundle free of a TLS stack.
fn stream_chat(t: &Target, body: &Value, slot: &CurlSlot, send: &dyn Fn(StreamMsg)) -> Result<Value, String> {
    let key = t.key.as_deref().map(str::trim).filter(|k| !k.is_empty());
    if key.is_some_and(|k| k.chars().any(|c| c.is_control() || c.is_whitespace())) {
        return Err(format!("The {} API key is not valid. Check it in Settings.", t.label));
    }
    if key.is_none() && !t.local {
        return Err(format!("Add a {} API key in Settings.", t.label));
    }
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let path = std::env::temp_dir().join(format!("imagesmith-chat-{}-{nonce}.json", std::process::id()));
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).map_err(|e| e.to_string())?;
        f.write_all(body.to_string().as_bytes()).map_err(|e| e.to_string())?;
    }
    let r = (|| {
        let mut cmd = Command::new("/usr/bin/curl");
        cmd.args(["-sS", "-N", "--max-time", "900", "-X", "POST", &t.endpoint, "-H", "Content-Type: application/json", "-H", "X-Title: ImageSmith"]);
        if key.is_some() {
            // The key goes through stdin so it never shows up in the process list.
            cmd.args(["-H", "@-"]);
        }
        let mut child = cmd
            .arg("--data-binary")
            .arg(format!("@{}", path.display()))
            .args(["-w", "%{stderr}\n%{http_code}"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not run curl: {e}"))?;
        if let (Some(key), Some(mut stdin)) = (key, child.stdin.take()) {
            let _ = writeln!(stdin, "Authorization: Bearer {key}");
        }
        let stdout = child.stdout.take().ok_or("no curl output")?;
        let mut stderr = child.stderr.take().ok_or("no curl output")?;
        *slot.lock().unwrap() = Some(child);

        let (mut content, mut reasoning, mut calls, mut raw, mut stream_err) = (String::new(), String::new(), Vec::<Value>::new(), String::new(), None);
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let Some(data) = line.strip_prefix("data:") else {
                // SSE comments are keep-alives; anything else is a plain JSON error body.
                if !line.starts_with(':') {
                    raw.push_str(&line);
                }
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                break;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
            if let Some(m) = v.pointer("/error/message").or_else(|| v.get("error")).and_then(Value::as_str) {
                stream_err = Some(m.to_string());
                break;
            }
            let Some(delta) = v.pointer("/choices/0/delta") else { continue };
            // OpenRouter streams `reasoning`; LM Studio streams `reasoning_content`.
            if let Some(t) = ["reasoning", "reasoning_content"].iter().find_map(|k| delta.get(*k).and_then(Value::as_str)).filter(|t| !t.is_empty()) {
                reasoning.push_str(t);
                send(StreamMsg::Reasoning(t.to_string()));
            }
            if let Some(t) = delta.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                content.push_str(t);
                send(StreamMsg::Delta(t.to_string()));
            }
            for tc in delta.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                let i = match tc.get("index").and_then(Value::as_u64) {
                    Some(i) => i as usize,
                    None if tc.get("id").is_some() || calls.is_empty() => calls.len(),
                    None => calls.len() - 1,
                };
                while calls.len() <= i {
                    calls.push(json!({ "id": "", "type": "function", "function": { "name": "", "arguments": "" } }));
                }
                let c = &mut calls[i];
                if let Some(id) = tc.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    c["id"] = id.into();
                }
                for f in ["name", "arguments"] {
                    if let Some(s) = tc.get("function").and_then(|x| x.get(f)).and_then(Value::as_str) {
                        let joined = format!("{}{s}", c["function"][f].as_str().unwrap_or(""));
                        c["function"][f] = joined.into();
                    }
                }
            }
        }
        let mut err_text = String::new();
        let _ = stderr.read_to_string(&mut err_text);
        let Some(mut child) = slot.lock().unwrap().take() else { return Err("Stopped.".into()) };
        let status = child.wait().map_err(|e| e.to_string())?;
        let (curl_err, code) = err_text.trim_end().rsplit_once('\n').unwrap_or(("", err_text.trim()));
        let code = code.trim();
        if !status.success() {
            if t.local && status.code() == Some(7) {
                return Err(format!("Could not reach {} at {}. Start its local server.", t.label, t.endpoint.trim_end_matches("/v1/chat/completions")));
            }
            return Err(format!("Request failed: {}", if curl_err.trim().is_empty() { status.to_string() } else { curl_err.trim().to_string() }));
        }
        if code == "401" || code == "403" {
            return Err(format!("{} rejected the API key. Check it in Settings.", t.label));
        }
        let label = t.label;
        if let Some(m) = stream_err {
            return Err(format!("{label}: {m}"));
        }
        if let Some(m) = serde_json::from_str::<Value>(&raw).ok().and_then(|v| {
            v.pointer("/error/message").or_else(|| v.get("error")).and_then(Value::as_str).map(String::from)
        }) {
            return Err(format!("{label}: {m}"));
        }
        if code != "200" {
            return Err(format!("{label} returned HTTP {code}"));
        }
        for (i, c) in calls.iter_mut().enumerate() {
            if c["id"].as_str() == Some("") {
                c["id"] = format!("call_{nonce}_{i}").into();
            }
        }
        let mut msg = json!({ "content": content, "reasoning": reasoning });
        if !calls.is_empty() {
            msg["tool_calls"] = Value::Array(calls);
        }
        Ok(msg)
    })();
    let _ = std::fs::remove_file(&path);
    r
}

fn system_prompt(p: &ProjectState, search: bool) -> String {
    let research = if search {
        "Before adding a third-party repository, use web_search to find the vendor's official instructions, copy the repository URL, suite, components and key URL from them, and confirm them with check_url. Never guess these."
    } else {
        "Verify repository and key URLs with check_url instead of guessing them. If you cannot confirm the right values, say so and ask the user for the vendor's install instructions."
    };
    format!(
        r#"You are the assistant inside ImageSmith, a macOS app that customizes Linux disk images (Raspberry Pi OS, Armbian, Debian, Ubuntu) from a recipe.toml.
The user asks for changes in plain language; make them with the tools, then reply with one or two short sentences saying what changed. Make only the changes asked for. If a request is ambiguous, ask instead of guessing. Never invent package names you are unsure exist in Debian/Ubuntu.

Recipe schema:
[image] name, base (path to base image), base_sha256, arch ("arm64"|"amd64"), rootfs_partition (number or "auto")
[size] mode ("shrink"|"fixed"|"grow"), headroom ("15%"), fixed ("8G"), grow ("2G"), expand_on_boot (bool)
[system] hostname, timezone ("Europe/London"), locale ("en_US.UTF-8"), reset_machine_id (bool), reset_ssh_host_keys (bool)
[[users]] name, groups (["sudo"]), ssh_keys (project-relative paths to .pub files), password ("locked"|"prompt")
[[repos]] extra apt sources: name (lowercase id), url, suite (defaults to the image's release codename), components (["main"]), key (https URL or project file of the signing key)
[packages] install, remove (apt package names), apt_update (bool)
[services] enable, disable (systemd units)
[[files]] src (project-relative file), dst (absolute path in image), mode ("0644"), owner ("root:root")
[[steps]] name, run (bash script run in a chroot), enabled (bool), when ("first" runs right after the base is unpacked, before repos and packages; "last", the default, runs after packages, users, files and services)
[output] compress ("none"|"zstd"|"xz"), level, bmap (bool), checksum ("sha256"|"none")

Build order: first steps, packages (repos are added first), users and system settings, files, services, last steps, then shrink and export. To put a file with given contents in the image, add a step that writes it (e.g. with a heredoc). {research} The project folder is {path}.

run_command runs a script inside the project's stage after the user approves it. {stage} Use it to check facts about the image instead of guessing, or to test commands before putting them in a step. Changes it makes are lost on the next build unless they are recorded in the recipe: either add them to the recipe yourself, or tell the user they can press "Keep Changes…" on the command to record them. Explain briefly why you want to run a command; never run destructive commands the user did not ask for.

Current state:
{report}"#,
        path = p.project.path.display(),
        stage = if p.has_stage {
            "A stage exists from the last build or extraction."
        } else {
            "There is no stage yet; the user must build, or extract the base in the Files tab, first."
        },
        report = recipe_report(p),
    )
}

// ---------------------------------------------------------------- workspace

impl Workspace {
    pub fn toggle_assistant(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.assistant.open = !self.assistant.open;
        self.assistant.history_open = false;
        if self.assistant.open {
            crate::model_picker::fetch_models(false, cx);
            let fh = self.assistant.input.focus_handle(cx);
            window.focus(&fh);
        } else {
            window.focus(&self.focus);
        }
        cx.notify();
    }

    pub fn send_chat(&mut self, cx: &mut Context<Self>) {
        let Some(pid) = self.active().map(|p| p.id().to_string()) else { return };
        let text = self.assistant.input.read(cx).text().trim().to_string();
        if text.is_empty() || self.assistant.project(&pid).current().busy {
            return;
        }
        self.assistant.input.update(cx, |i, cx| i.set_text("", cx));
        let pc = self.assistant.project(&pid);
        let chat = &mut pc.list[pc.current];
        chat.items.push(Item::User(text.clone()));
        chat.api.push(json!({ "role": "user", "content": text }));
        chat.busy = true;
        chat.generation += 1;
        let (cid, generation) = (chat.id, chat.generation);
        self.assistant.save(&pid);
        self.assistant.scroll.scroll_to_bottom();
        self.chat_round(pid, cid, generation, 0, cx);
    }

    /// Opens a new chat that asks the model to fix `problem`.
    pub fn fix_with_assistant(&mut self, problem: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.active().is_none() {
            return;
        }
        if !self.assistant.open {
            self.assistant.open = true;
            crate::model_picker::fetch_models(false, cx);
        }
        self.new_chat(window, cx);
        let text = format!("Please fix this. Explain the cause briefly, then change the recipe.\n\n{}", problem.trim_end());
        self.assistant.input.update(cx, |i, cx| i.set_text(text, cx));
        self.send_chat(cx);
    }

    /// Starts a new conversation, unless the current one is still empty.
    pub fn new_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pid) = self.active().map(|p| p.id().to_string()) else { return };
        let pc = self.assistant.project(&pid);
        match pc.list.iter().position(|c| c.items.is_empty()) {
            Some(i) => pc.current = i,
            None => {
                pc.list.push(Chat::new());
                pc.current = pc.list.len() - 1;
            }
        }
        self.assistant.history_open = false;
        window.focus(&self.assistant.input.focus_handle(cx));
        cx.notify();
    }

    fn open_chat(&mut self, pid: &str, cid: u64, cx: &mut Context<Self>) {
        let pc = self.assistant.project(pid);
        if let Some(i) = pc.list.iter().position(|c| c.id == cid) {
            pc.current = i;
        }
        self.assistant.history_open = false;
        self.assistant.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn delete_chat(&mut self, pid: &str, cid: u64, cx: &mut Context<Self>) {
        let pc = self.assistant.project(pid);
        let Some(i) = pc.list.iter().position(|c| c.id == cid && !c.busy) else { return };
        let current = pc.list[pc.current].id;
        pc.list.remove(i);
        if pc.list.is_empty() {
            pc.list.push(Chat::new());
        }
        pc.current = pc.list.iter().position(|c| c.id == current).unwrap_or(pc.list.len() - 1);
        self.assistant.save(pid);
        cx.notify();
    }

    pub fn stop_chat(&mut self, cx: &mut Context<Self>) {
        let Some(pid) = self.active().map(|p| p.id().to_string()) else { return };
        let pc = self.assistant.project(&pid);
        let chat = &mut pc.list[pc.current];
        chat.generation += 1;
        chat.busy = false;
        chat.paused = None;
        for item in &mut chat.items {
            if let Item::Command(r) = item {
                if matches!(r.state, RunState::Pending | RunState::Running) {
                    r.state = RunState::Stopped;
                }
            }
        }
        if let Some(mut child) = chat.curl.take().and_then(|s| s.lock().unwrap().take()) {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(partial) = chat.streaming_reasoning.take().filter(|s| !s.trim().is_empty()) {
            let secs = chat.reasoning_clock.take().map(|(t, s)| s.unwrap_or(t.elapsed().as_secs()));
            chat.items.push(Item::Thinking(partial.trim().to_string(), secs));
        }
        if let Some(partial) = chat.streaming.take().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            chat.api.push(json!({ "role": "assistant", "content": partial }));
            chat.items.push(Item::Assistant(partial));
        }
        self.assistant.save(&pid);
        cx.notify();
    }

    fn chat_round(&mut self, pid: String, cid: u64, generation: u64, round: usize, cx: &mut Context<Self>) {
        let s = settings(cx);
        let (target, model) = match s.ai_provider {
            AiProvider::OpenRouter => (
                Target { endpoint: ENDPOINT.into(), key: s.openrouter_api_key.clone(), label: "OpenRouter", local: false },
                if s.ai_model.trim().is_empty() { imagesmith_core::settings::DEFAULT_AI_MODEL.to_string() } else { s.ai_model.trim().to_string() },
            ),
            AiProvider::LmStudio => (
                Target { endpoint: format!("{}/v1/chat/completions", s.lmstudio_base()), key: s.lmstudio_api_key.clone(), label: "LM Studio", local: true },
                s.lmstudio_model.trim().to_string(),
            ),
        };
        if model.is_empty() {
            self.on_chat_reply(pid, cid, generation, round, Err("Choose a model below the message box first.".into()), cx);
            return;
        }
        let Some(p) = self.projects.iter().find(|p| p.id() == pid) else { return };
        let Some(chat) = self.assistant.chats.get(&pid).and_then(|pc| pc.list.iter().find(|c| c.id == cid)) else { return };
        let undone: Vec<&str> = chat.items.iter().filter_map(|i| match i {
            Item::Tool(t) if t.undone => Some(t.call_id.as_str()),
            _ => None,
        }).collect();
        let search = crate::web_search::backend(settings(cx)).is_some();
        let mut messages = vec![json!({ "role": "system", "content": system_prompt(p, search) })];
        for m in &chat.api {
            let mut m = m.clone();
            let id = m.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
            if undone.contains(&id) {
                let c = m["content"].as_str().unwrap_or("").to_string();
                m["content"] = Value::from(format!("{c}\n[The user later undid this change.]"));
            }
            messages.push(m);
        }
        let body = json!({ "model": model, "messages": messages, "tools": tools(search), "stream": true });
        let slot = CurlSlot::default();
        if let Some(chat) = self.assistant.chat_mut(&pid, cid) {
            chat.curl = Some(slot.clone());
        }
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<StreamMsg>();
        std::thread::spawn(move || {
            let r = stream_chat(&target, &body, &slot, &|m| {
                let _ = tx.unbounded_send(m);
            });
            let _ = tx.unbounded_send(StreamMsg::Done(r));
        });
        cx.spawn(async move |this, cx| {
            use futures::StreamExt;
            while let Some(m) = rx.next().await {
                let done = matches!(m, StreamMsg::Done(_));
                let ok = this.update(cx, |this, cx| match m {
                    StreamMsg::Delta(t) => this.on_chat_delta(&pid, cid, generation, &t, false, cx),
                    StreamMsg::Reasoning(t) => this.on_chat_delta(&pid, cid, generation, &t, true, cx),
                    StreamMsg::Done(r) => this.on_chat_reply(pid.clone(), cid, generation, round, r, cx),
                });
                if ok.is_err() || done {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn on_chat_delta(&mut self, pid: &str, cid: u64, generation: u64, text: &str, reasoning: bool, cx: &mut Context<Self>) {
        let Some(chat) = self.assistant.chat_mut(pid, cid).filter(|c| c.generation == generation) else { return };
        if reasoning {
            chat.reasoning_clock.get_or_insert((std::time::Instant::now(), None));
            chat.streaming_reasoning.get_or_insert_with(String::new).push_str(text);
            if let Some(h) = self.assistant.think_scrolls.get(&(cid, usize::MAX)) {
                h.scroll_to_bottom();
            }
        } else {
            if let Some((t, s @ None)) = &mut chat.reasoning_clock {
                *s = Some(t.elapsed().as_secs());
            }
            chat.streaming.get_or_insert_with(String::new).push_str(text);
        }
        self.assistant.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn on_chat_reply(&mut self, pid: String, cid: u64, generation: u64, round: usize, r: Result<Value, String>, cx: &mut Context<Self>) {
        let Some(chat) = self.assistant.chat_mut(&pid, cid) else { return };
        if chat.generation != generation {
            return;
        }
        chat.streaming = None;
        chat.curl = None;
        let live_reasoning = chat.streaming_reasoning.take();
        let secs = chat.reasoning_clock.take().map(|(t, s)| s.unwrap_or(t.elapsed().as_secs()));
        if let Some(text) = live_reasoning.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            let ix = chat.items.len();
            chat.items.push(Item::Thinking(text, secs));
            // The finished box keeps the live box's expanded state and scroll position.
            if self.assistant.expanded.remove(&(cid, usize::MAX)) {
                self.assistant.expanded.insert((cid, ix));
            }
            if let Some(h) = self.assistant.think_scrolls.remove(&(cid, usize::MAX)) {
                self.assistant.think_scrolls.insert((cid, ix), h);
            }
        }
        let Some(chat) = self.assistant.chat_mut(&pid, cid) else { return };
        let msg = match r {
            Ok(m) => m,
            Err(e) => {
                chat.items.push(Item::Error(e));
                chat.busy = false;
                self.assistant.save(&pid);
                self.assistant.scroll.scroll_to_bottom();
                cx.notify();
                return;
            }
        };
        let content = msg.get("content").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let calls: Vec<Value> = msg.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut stored = json!({ "role": "assistant", "content": content });
        if !calls.is_empty() {
            stored["tool_calls"] = Value::Array(calls.clone());
        }
        chat.api.push(stored);
        if !content.is_empty() {
            chat.items.push(Item::Assistant(content));
        }
        if calls.is_empty() {
            chat.busy = false;
            self.assistant.save(&pid);
            self.assistant.scroll.scroll_to_bottom();
            cx.notify();
            return;
        }
        self.assistant.scroll.scroll_to_bottom();
        cx.notify();
        let search = crate::web_search::backend(settings(cx));
        let net: Vec<(String, String, Value)> = calls
            .iter()
            .filter_map(|c| {
                let name = c.pointer("/function/name").and_then(Value::as_str)?;
                matches!(name, "check_url" | "web_search").then(|| {
                    let args = c.pointer("/function/arguments").and_then(Value::as_str).and_then(|s| serde_json::from_str(s).ok()).unwrap_or(json!({}));
                    (c["id"].as_str().unwrap_or("").to_string(), name.to_string(), args)
                })
            })
            .collect();
        if net.is_empty() {
            self.run_tool_calls(pid, cid, generation, round, calls, HashMap::new(), cx);
            return;
        }
        // Searches and URL checks hit the network, so they run off the UI thread before the edits apply.
        let task = cx.background_spawn(async move {
            net.into_iter().map(|(id, name, args)| (id, run_network_tool(&name, &args, search.as_ref()))).collect::<HashMap<_, _>>()
        });
        cx.spawn(async move |this, cx| {
            let done = task.await;
            let _ = this.update(cx, |this, cx| this.run_tool_calls(pid, cid, generation, round, calls, done, cx));
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    fn run_tool_calls(
        &mut self,
        pid: String,
        cid: u64,
        generation: u64,
        round: usize,
        calls: Vec<Value>,
        mut done: ToolResults,
        cx: &mut Context<Self>,
    ) {
        if self.assistant.chat_mut(&pid, cid).is_none_or(|c| c.generation != generation) {
            return;
        }
        let Some(pi) = self.projects.iter().position(|p| p.id() == pid) else { return };
        for (i, call) in calls.iter().enumerate() {
            let id = call.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let name = call.pointer("/function/name").and_then(Value::as_str).unwrap_or("");
            let args: Value = call.pointer("/function/arguments").and_then(Value::as_str).and_then(|s| serde_json::from_str(s).ok()).unwrap_or(json!({}));
            if name == "run_command" {
                let script = args.get("script").and_then(Value::as_str).unwrap_or("").trim().to_string();
                let Some(chat) = self.assistant.chat_mut(&pid, cid) else { return };
                let result = match done.remove(&id) {
                    Some((_, r)) => r,
                    None if script.is_empty() => Err("missing string argument \"script\"".into()),
                    None => {
                        // Wait for the user; answer_command picks up from this call.
                        let item = chat.items.len();
                        chat.items.push(Item::Command(CommandRecord { call_id: id, script, state: RunState::Pending, output: String::new() }));
                        chat.paused = Some(Paused { round, calls: calls[i..].to_vec(), done: std::mem::take(&mut done), item });
                        self.assistant.save(&pid);
                        self.assistant.scroll.scroll_to_bottom();
                        cx.notify();
                        return;
                    }
                };
                let content = result.unwrap_or_else(|e| format!("Error: {e}"));
                chat.api.push(json!({ "role": "tool", "tool_call_id": id, "content": content }));
                continue;
            }
            let p = &mut self.projects[pi];
            let before = p.text.clone();
            let (summary, result) = match done.remove(&id) {
                Some(r) => r,
                None => run_tool(p, name, &args),
            };
            let after = p.text.clone();
            let change = (before != after).then_some((before, after));
            if change.is_some() {
                self.recipe_ui.dirty = true;
                self.recipe_ui.mark_invalid(&self.projects[pi].diagnostics, cx);
            }
            let content = match &result {
                Ok(s) => s.clone(),
                Err(e) => format!("Error: {e}"),
            };
            let Some(chat) = self.assistant.chat_mut(&pid, cid) else { return };
            chat.api.push(json!({ "role": "tool", "tool_call_id": id, "content": content }));
            chat.items.push(Item::Tool(ToolRecord { call_id: id, summary, result, change, undone: false }));
        }
        self.assistant.save(&pid);
        if round + 1 >= MAX_ROUNDS {
            let Some(chat) = self.assistant.chat_mut(&pid, cid) else { return };
            chat.items.push(Item::Error("Stopped after too many tool calls.".into()));
            chat.busy = false;
            self.assistant.save(&pid);
            cx.notify();
            return;
        }
        self.chat_round(pid, cid, generation, round + 1, cx);
    }

    /// Undo restores the recipe to before call `ix` (undoing every later call too);
    /// redo restores it to after `ix`.
    fn toggle_tool(&mut self, pid: String, cid: u64, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(chat) = self.assistant.chats.get(&pid).and_then(|pc| pc.list.iter().find(|c| c.id == cid)) else { return };
        let Some(Item::Tool(t)) = chat.items.get(ix) else { return };
        let Some((before, after)) = t.change.clone() else { return };
        let undo = !t.undone;
        // What the recipe should be if nothing else touched it since the latest applied change.
        let expected = chat
            .items
            .iter()
            .rev()
            .find_map(|i| match i {
                Item::Tool(t) if !t.undone => t.change.as_ref().map(|c| c.1.clone()),
                _ => None,
            })
            .or_else(|| {
                chat.items.iter().find_map(|i| match i {
                    Item::Tool(t) => t.change.as_ref().map(|c| c.0.clone()),
                    _ => None,
                })
            });
        let current = self.projects.iter().find(|p| p.id() == pid).map(|p| p.text.clone());
        let target = if undo { before } else { after };
        if current.is_some() && current != expected {
            let rx = window.prompt(
                PromptLevel::Warning,
                if undo { "Undo this change?" } else { "Redo this change?" },
                Some("recipe.toml was edited after the assistant's changes. Those edits will be lost."),
                &[if undo { "Undo" } else { "Redo" }, "Cancel"],
                cx,
            );
            cx.spawn(async move |this, cx| {
                if rx.await.ok() == Some(0) {
                    let _ = this.update(cx, |this, cx| this.apply_tool_toggle(pid, cid, ix, undo, target, cx));
                }
            })
            .detach();
            return;
        }
        self.apply_tool_toggle(pid, cid, ix, undo, target, cx);
    }

    fn apply_tool_toggle(&mut self, pid: String, cid: u64, ix: usize, undo: bool, text: String, cx: &mut Context<Self>) {
        let Some(pi) = self.projects.iter().position(|p| p.id() == pid) else { return };
        let p = &mut self.projects[pi];
        if let Err(e) = std::fs::write(p.project.recipe_path(), &text) {
            self.toast(format!("Could not write recipe.toml: {e}"), cx);
            return;
        }
        p.reload();
        if let Some(chat) = self.assistant.chat_mut(&pid, cid) {
            for (i, item) in chat.items.iter_mut().enumerate() {
                if let Item::Tool(t) = item {
                    if t.change.is_some() && (if undo { i >= ix } else { i <= ix }) {
                        t.undone = undo;
                    }
                }
            }
        }
        self.assistant.save(&pid);
        self.recipe_ui.dirty = true;
        self.recipe_ui.mark_invalid(&self.projects[pi].diagnostics, cx);
        cx.notify();
    }

    /// Runs or skips the command the chat is waiting on, then carries on with the model's calls.
    fn answer_command(&mut self, pid: String, cid: u64, run: bool, cx: &mut Context<Self>) {
        let building = self.is_building(&pid);
        let Some(chat) = self.assistant.chat_mut(&pid, cid) else { return };
        let Some(paused) = chat.paused.take() else { return };
        let generation = chat.generation;
        let Some(Item::Command(rec)) = chat.items.get_mut(paused.item) else { return };
        let id = rec.call_id.clone();
        if !run || building {
            let msg = if building { "a build is running for this project; try again after it finishes" } else { "the user chose not to run this command" };
            rec.state = RunState::Skipped;
            if building {
                rec.output = format!("Not run: {msg}.");
            }
            let mut done = paused.done;
            done.insert(id, (String::new(), Err(msg.into())));
            self.assistant.save(&pid);
            self.run_tool_calls(pid, cid, generation, paused.round, paused.calls, done, cx);
            return;
        }
        rec.state = RunState::Running;
        let script = rec.script.clone();
        let slot = CurlSlot::default();
        chat.curl = Some(slot.clone());
        let rt = crate::workspace::runtime(cx);
        self.touch_store();
        let task = cx.background_spawn({
            let pid = pid.clone();
            async move { run_in_stage(&rt, &pid, &script, &slot) }
        });
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |this, cx| this.on_command_done(pid, cid, generation, paused, r, cx));
        })
        .detach();
        cx.notify();
    }

    fn on_command_done(&mut self, pid: String, cid: u64, generation: u64, paused: Paused, r: Result<(Option<i32>, String), String>, cx: &mut Context<Self>) {
        self.touch_store();
        let Some(chat) = self.assistant.chat_mut(&pid, cid).filter(|c| c.generation == generation) else { return };
        chat.curl = None;
        let Some(Item::Command(rec)) = chat.items.get_mut(paused.item) else { return };
        let result = match r {
            Ok((code, out)) => {
                rec.state = RunState::Exited(code);
                let status = code.map_or("killed by a signal".to_string(), |c| format!("exit status {c}"));
                let shown = if out.is_empty() { "(no output)".to_string() } else { clip(&out, 2000, 6000) };
                rec.output = out;
                Ok(format!("{status}\n{shown}"))
            }
            Err(e) => {
                rec.state = RunState::Failed;
                rec.output = e.clone();
                Err(e)
            }
        };
        let mut done = paused.done;
        done.insert(rec.call_id.clone(), (String::new(), result));
        self.assistant.save(&pid);
        self.run_tool_calls(pid, cid, generation, paused.round, paused.calls, done, cx);
    }

    /// Like closing the Shell with Keep Changes: pick stage changes to record in the recipe.
    fn keep_stage_changes(&mut self, pid: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.active().is_none_or(|p| p.id() != pid) {
            return;
        }
        self.set_tab(crate::workspace::Tab::Files, window, cx);
        if let Some(f) = self.files.get(pid) {
            f.update(cx, |f, cx| f.request_capture(cx));
        }
    }

    /// A small reasoning box that follows the newest text; click its header to show it all.
    fn thinking_box(&self, cid: u64, ix: usize, text: &str, live: bool, secs: Option<u64>, c: &Colors, cx: &mut Context<Self>) -> gpui::Div {
        let key = (cid, ix);
        let expanded = self.assistant.expanded.contains(&key);
        let handle = self.assistant.think_scrolls.get(&key).cloned().unwrap_or_default();
        let label = match (live, secs) {
            (true, _) => "Thinking…".to_string(),
            (false, Some(s)) if s > 0 => format!("Thought for {s}s"),
            _ => "Thought".to_string(),
        };
        let hover = c.hover;
        div()
            .flex()
            .flex_col()
            .rounded(px(8.))
            .border_1()
            .border_color(c.separator)
            .bg(c.group)
            .overflow_hidden()
            .child(
                div()
                    .id(("think-head", ix))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(10.))
                    .py(px(5.))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(div().flex_none().text_color(c.muted).child("✦"))
                    .child(div().flex_1().text_size(px(12.)).text_color(c.muted).child(label))
                    .child(div().flex_none().text_size(px(10.)).text_color(c.muted).child(if expanded { "▾" } else { "▸" }))
                    .tooltip(tooltip(if expanded { "Collapse" } else { "Show all of the model's reasoning" }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let ui = &mut this.assistant;
                        if !ui.expanded.remove(&key) {
                            ui.expanded.insert(key);
                        } else if let Some(h) = ui.think_scrolls.get(&key) {
                            h.scroll_to_bottom();
                        }
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id(("think-body", ix))
                    .px(px(10.))
                    .pb(px(6.))
                    .text_size(px(11.5))
                    .line_height(px(16.))
                    .text_color(c.muted)
                    .when(!expanded, |d| d.max_h(px(64.)).overflow_y_scroll().track_scroll(&handle))
                    .child(text.to_string()),
            )
    }

    pub fn render_assistant(&mut self, c: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let s = settings(cx);
        let lmstudio = s.ai_provider == AiProvider::LmStudio;
        let ready = lmstudio || s.openrouter_api_key.as_deref().is_some_and(|k| !k.trim().is_empty());
        let pid = self.active().map(|p| p.id().to_string());
        if let Some(pid) = &pid {
            let pc = self.assistant.project(pid);
            let chat = pc.current();
            let mut keys: Vec<(u64, usize)> =
                chat.items.iter().enumerate().filter(|(_, i)| matches!(i, Item::Thinking(..))).map(|(ix, _)| (chat.id, ix)).collect();
            if chat.streaming_reasoning.is_some() {
                keys.push((chat.id, usize::MAX));
            }
            for k in keys {
                self.assistant.think_scrolls.entry(k).or_insert_with(|| {
                    let h = ScrollHandle::new();
                    h.scroll_to_bottom();
                    h
                });
            }
        }
        let pc = pid.as_ref().and_then(|id| self.assistant.chats.get(id));
        let chat = pc.map(|pc| pc.current());
        let busy = chat.is_some_and(|c| c.busy);
        let has_items = chat.is_some_and(|c| !c.items.is_empty());
        let title = chat.map(|c| c.title()).unwrap_or_else(|| "Assistant".into());
        let history: Vec<(u64, String, String, bool, bool)> = pc
            .map(|pc| pc.list.iter().rev().filter(|c| !c.items.is_empty()).map(|c| (c.id, c.title(), c.created.clone(), c.id == pc.current().id, c.busy)).collect())
            .unwrap_or_default();

        let mut list = div().flex().flex_col().gap(px(10.)).p(px(12.));
        if !ready {
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .p(px(12.))
                    .rounded(px(8.))
                    .bg(c.group)
                    .border_1()
                    .border_color(c.separator)
                    .child(div().text_size(px(12.5)).child("Add an OpenRouter API key in Settings, or switch to a local model served by LM Studio, to chat with a model that can edit this recipe."))
                    .child(div().flex().child(button("ai-settings", "Open Settings…", Kind::Normal, true, c).on_click(|_, w, cx| w.dispatch_action(Box::new(OpenSettings), cx)))),
            );
        } else if pid.is_none() {
            list = list.child(div().text_size(px(12.5)).text_color(c.muted).child("Open a project to chat about its recipe."));
        } else if !has_items {
            list = list.child(div().text_size(px(12.5)).text_color(c.muted).child(
                "Describe what you want in plain language, for example \"add docker from its official repository\" or \"set the hostname to kiosk and install chromium\". Every change can be undone.",
            ));
        }
        if let (Some(pid), Some(chat)) = (pid.clone(), chat) {
            let cid = chat.id;
            let live_thinking = chat.streaming_reasoning.clone().filter(|s| !s.is_empty());
            let live_secs = chat.reasoning_clock.and_then(|(_, s)| s);
            for (ix, item) in chat.items.iter().enumerate() {
                let el = match item {
                    Item::Thinking(t, secs) => self.thinking_box(cid, ix, t, false, *secs, c, cx),
                    Item::User(t) => div().flex().justify_end().child(
                        div().max_w(gpui::relative(0.9)).px(px(10.)).py(px(6.)).rounded(px(10.)).bg(c.accent_soft).text_size(px(13.)).child(t.clone()),
                    ),
                    Item::Assistant(t) => div().text_size(px(13.)).line_height(px(19.)).child(t.clone()),
                    Item::Error(e) => div().px(px(10.)).py(px(6.)).rounded(px(8.)).bg(c.danger_soft).text_color(c.danger).text_size(px(12.5)).child(e.clone()),
                    Item::Command(r) => {
                        let waiting = r.state == RunState::Pending && chat.paused.as_ref().is_some_and(|p| p.item == ix);
                        let (icon, color, label) = match r.state {
                            RunState::Pending if waiting => ("?", c.accent, "Run this in the image?".to_string()),
                            RunState::Pending | RunState::Stopped => ("■", c.faint, "Stopped".to_string()),
                            RunState::Running => ("…", c.muted, "Running in the image…".to_string()),
                            RunState::Skipped => ("↷", c.faint, "Not run".to_string()),
                            RunState::Exited(Some(0)) => ("✓", c.success, "Ran in the image".to_string()),
                            RunState::Exited(Some(n)) => ("✗", c.danger, format!("Exited with status {n}")),
                            RunState::Exited(None) => ("✗", c.danger, "Killed".to_string()),
                            RunState::Failed => ("✗", c.danger, "Could not run".to_string()),
                        };
                        let ran = matches!(r.state, RunState::Exited(_));
                        let tail: String = {
                            let lines: Vec<&str> = r.output.lines().collect();
                            lines[lines.len().saturating_sub(40)..].join("\n")
                        };
                        let (pid_run, pid_skip, pid_keep) = (pid.clone(), pid.clone(), pid.clone());
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .px(px(10.))
                            .py(px(6.))
                            .rounded(px(8.))
                            .border_1()
                            .border_color(if waiting { c.accent } else { c.separator })
                            .bg(c.group)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(div().flex_none().text_color(color).child(icon))
                                    .child(div().flex_1().min_w_0().text_size(px(12.5)).child(label))
                                    .when(ran, |d| {
                                        d.child(
                                            button(("ai-keep", ix), "Keep Changes…", Kind::Ghost, !busy, c)
                                                .h(px(22.))
                                                .tooltip(tooltip("Choose which changes in the image to record in the recipe"))
                                                .on_click_if(!busy, cx.listener(move |this, _, w, cx| this.keep_stage_changes(&pid_keep, w, cx))),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .id(("ai-script", ix))
                                    .max_h(px(160.))
                                    .overflow_y_scroll()
                                    .px(px(8.))
                                    .py(px(5.))
                                    .rounded(px(5.))
                                    .bg(c.surface)
                                    .text_size(px(11.5))
                                    .font_family(MONO)
                                    .child(r.script.clone()),
                            )
                            .when(!tail.is_empty(), |d| {
                                d.child(
                                    div()
                                        .id(("ai-output", ix))
                                        .max_h(px(180.))
                                        .overflow_y_scroll()
                                        .px(px(8.))
                                        .py(px(5.))
                                        .rounded(px(5.))
                                        .bg(c.surface)
                                        .text_size(px(11.))
                                        .font_family(MONO)
                                        .text_color(c.muted)
                                        .child(tail),
                                )
                            })
                            .when(waiting, |d| {
                                d.child(
                                    div()
                                        .flex()
                                        .justify_end()
                                        .gap(px(6.))
                                        .child(
                                            button(("ai-skip", ix), "Don't Run", Kind::Normal, true, c)
                                                .tooltip(tooltip("Tell the assistant you declined"))
                                                .on_click(cx.listener(move |this, _, _, cx| this.answer_command(pid_skip.clone(), cid, false, cx))),
                                        )
                                        .child(
                                            button(("ai-run", ix), "Run", Kind::Primary, true, c)
                                                .tooltip(tooltip("Run as root in the project's stage, like the Shell"))
                                                .on_click(cx.listener(move |this, _, _, cx| this.answer_command(pid_run.clone(), cid, true, cx))),
                                        ),
                                )
                            })
                    }
                    Item::Tool(t) => {
                        let (icon, color) = match (&t.result, t.undone) {
                            (Err(_), _) => ("✗", c.danger),
                            (_, true) => ("↶", c.faint),
                            _ => ("✓", c.success),
                        };
                        let detail = t.result.as_ref().err().cloned();
                        let can_toggle = t.change.is_some() && !busy;
                        let pid = pid.clone();
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .px(px(10.))
                            .py(px(6.))
                            .rounded(px(8.))
                            .border_1()
                            .border_color(c.separator)
                            .bg(c.group)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(div().flex_none().text_color(color).child(icon))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_size(px(12.5))
                                            .font_family(MONO)
                                            .when(t.undone, |d| d.line_through().text_color(c.muted))
                                            .child(t.summary.clone()),
                                    )
                                    .when(t.change.is_some(), |d| {
                                        d.child(
                                            button(("ai-undo", ix), if t.undone { "Redo" } else { "Undo" }, Kind::Ghost, can_toggle, c)
                                                .h(px(22.))
                                                .tooltip(tooltip(if t.undone {
                                                    "Reapply this change (and the ones before it)"
                                                } else {
                                                    "Restore the recipe to before this change (later changes are undone too)"
                                                }))
                                                .on_click_if(can_toggle, cx.listener(move |this, _, w, cx| this.toggle_tool(pid.clone(), cid, ix, w, cx))),
                                        )
                                    }),
                            )
                            .children(detail.map(|d| div().pl(px(20.)).text_size(px(11.5)).text_color(c.danger).child(d)))
                    }
                };
                list = list.child(el);
            }
            if let Some(t) = &live_thinking {
                list = list.child(self.thinking_box(cid, usize::MAX, t, live_secs.is_none(), live_secs, c, cx));
            }
            let on_command = matches!(chat.items.last(), Some(Item::Command(r)) if matches!(r.state, RunState::Pending | RunState::Running));
            match chat.streaming.as_deref().filter(|s| !s.is_empty()) {
                Some(s) => list = list.child(div().text_size(px(13.)).line_height(px(19.)).child(format!("{s}▍"))),
                None if busy && live_thinking.is_none() && !on_command => list = list.child(div().text_size(px(12.5)).text_color(c.muted).child("Thinking…")),
                None => {}
            }
        }

        let can_send = ready && pid.is_some() && !busy;
        let hover = c.hover;
        let history_popover = (self.assistant.history_open && pid.is_some()).then(|| {
            let pid = pid.clone().unwrap_or_default();
            let mut col = div().flex().flex_col().p(px(4.));
            if history.is_empty() {
                col = col.child(div().p(px(8.)).text_size(px(12.)).text_color(c.muted).child("No conversations yet."));
            }
            for (i, (cid, title, created, current, busy)) in history.into_iter().enumerate() {
                let (pid_open, pid_del) = (pid.clone(), pid.clone());
                let danger = c.danger;
                col = col.child(
                    div()
                        .id(("chat-hist", i))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(8.))
                        .py(px(5.))
                        .rounded(px(5.))
                        .cursor_pointer()
                        .when(current, |d| d.bg(c.selected))
                        .when(!current, move |d| d.hover(move |s| s.bg(hover)))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .child(div().text_size(px(12.5)).overflow_hidden().whitespace_nowrap().text_ellipsis().child(title))
                                .child(div().text_size(px(10.5)).text_color(c.muted).child(created)),
                        )
                        .when(!busy, |d| {
                            d.child(
                                div()
                                    .id(("chat-del", i))
                                    .flex_none()
                                    .px(px(4.))
                                    .text_color(c.muted)
                                    .hover(move |s| s.text_color(danger))
                                    .tooltip(tooltip("Delete this conversation. Recipe changes are kept."))
                                    .child("×")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.delete_chat(&pid_del, cid, cx);
                                    })),
                            )
                        })
                        .on_click(cx.listener(move |this, _, _, cx| this.open_chat(&pid_open, cid, cx))),
                );
            }
            deferred(
                anchored().snap_to_window_with_margin(px(8.)).child(
                    div()
                        .id("chat-history")
                        .occlude()
                        .mt(px(4.))
                        .w(px(300.))
                        .max_h(px(380.))
                        .overflow_y_scroll()
                        .rounded(px(8.))
                        .bg(c.surface)
                        .border_1()
                        .border_color(c.border)
                        .shadow_lg()
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.assistant.history_open = false;
                            cx.notify();
                        }))
                        .child(col),
                ),
            )
            .with_priority(3)
        });
        div()
            .id("assistant")
            .flex()
            .flex_col()
            .flex_none()
            .w(px(340.))
            .h_full()
            .bg(c.sidebar)
            .border_l_1()
            .border_color(c.border)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(6.))
                    .px(px(10.))
                    .h(px(38.))
                    .border_b_1()
                    .border_color(c.border)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .id("chat-title")
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .h(px(26.))
                                    .px(px(6.))
                                    .rounded(px(6.))
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(hover))
                                    .tooltip(tooltip("Conversations for this project"))
                                    .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().font_weight(gpui::FontWeight::SEMIBOLD).child(title))
                                    .child(div().flex_none().text_size(px(10.)).text_color(c.muted).child("▾"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.assistant.history_open = !this.assistant.history_open;
                                        cx.notify();
                                    })),
                            )
                            .children(history_popover),
                    )
                    .child(
                        button("ai-new", "+", Kind::Ghost, pid.is_some() && has_items, c)
                            .w(px(26.))
                            .text_size(px(16.))
                            .tooltip(tooltip("New chat"))
                            .on_click_if(pid.is_some() && has_items, cx.listener(|this, _, w, cx| this.new_chat(w, cx))),
                    ),
            )
            .child(div().id("assistant-scroll").flex_1().min_h_0().overflow_y_scroll().track_scroll(&self.assistant.scroll).child(list))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .gap(px(6.))
                    .p(px(10.))
                    .border_t_1()
                    .border_color(c.border)
                    .child(self.assistant.input.clone())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(div().flex_1().min_w_0().child(self.assistant.model_picker.clone()))
                            .child(if busy {
                                button("ai-stop", "Stop", Kind::Normal, true, c).on_click(cx.listener(|this, _, _, cx| this.stop_chat(cx)))
                            } else {
                                button("ai-send", "Send", Kind::Primary, can_send, c)
                                    .tooltip(tooltip("Send (Return). Shift-Return adds a line."))
                                    .on_click_if(can_send, cx.listener(|this, _, _, cx| this.send_chat(cx)))
                            }),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use imagesmith_core::project::Project;
    use imagesmith_core::recipe::{starter, Arch, PartitionRef};

    fn project() -> (tempfile::TempDir, ProjectState) {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("recipe.toml"), starter("x", "base.img", Arch::Arm64, PartitionRef::Auto)).unwrap();
        let p = ProjectState::load(Project { path: d.path().to_path_buf(), id: "t".into() });
        (d, p)
    }

    #[test]
    fn wrapped_script_runs_whole() {
        let mut child = Command::new("/bin/bash").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(wrap_script("cat\necho one >&2\nread x || echo two\n").as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "one\ntwo\n");
    }

    #[test]
    #[ignore = "needs LM Studio serving IMAGESMITH_LMSTUDIO_MODEL on localhost:1234"]
    fn lmstudio_streams_tool_calls() {
        let model = std::env::var("IMAGESMITH_LMSTUDIO_MODEL").unwrap();
        let t = Target { endpoint: "http://localhost:1234/v1/chat/completions".into(), key: None, label: "LM Studio", local: true };
        let body = json!({ "model": model, "stream": true, "tools": tools(false),
            "messages": [{ "role": "user", "content": "Use add_to_list to add htop to packages.install." }] });
        let msg = stream_chat(&t, &body, &CurlSlot::default(), &|_| {}).unwrap();
        let call = &msg["tool_calls"][0];
        assert_eq!(call["function"]["name"], "add_to_list");
        let args: Value = serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["item"], "htop");
        assert!(!call["id"].as_str().unwrap().is_empty());

        let thoughts = std::cell::Cell::new(0);
        let body = json!({ "model": body["model"], "stream": true, "messages": [{ "role": "user", "content": "What is 17*23? Think it through." }] });
        let msg = stream_chat(&t, &body, &CurlSlot::default(), &|m| {
            if matches!(m, StreamMsg::Reasoning(_)) {
                thoughts.set(thoughts.get() + 1);
            }
        })
        .unwrap();
        assert!(thoughts.get() > 0 && !msg["reasoning"].as_str().unwrap().is_empty());
        assert!(msg["content"].as_str().unwrap().contains("391"));
    }

    #[test]
    fn tools_edit_and_report() {
        let (_d, mut p) = project();
        assert!(tools(false).as_array().unwrap().iter().any(|t| t["function"]["name"] == "identify_image"));
        let (_, image_result) = run_tool(&mut p, "identify_image", &json!({}));
        assert!(image_result.unwrap_err().contains("no cached probe"));

        let (s, r) = run_tool(&mut p, "add_to_list", &json!({ "list": "packages.install", "item": "htop" }));
        assert_eq!(s, "Added htop to packages.install");
        assert!(r.is_ok());
        assert_eq!(p.recipe.as_ref().unwrap().recipe.packages.install, vec!["htop"]);

        let (_, r) = run_tool(&mut p, "set_field", &json!({ "section": "system", "key": "hostname", "value": "kiosk" }));
        assert!(r.is_ok());
        assert_eq!(p.recipe.as_ref().unwrap().recipe.system.hostname.as_deref(), Some("kiosk"));

        let repo = json!({ "kind": "repos", "fields": { "name": "docker", "url": "https://download.docker.com/linux/debian", "components": ["stable"] } });
        let (s, r) = run_tool(&mut p, "add_entry", &repo);
        assert_eq!(s, "Added repository docker");
        assert!(r.is_ok());
        assert_eq!(p.recipe.as_ref().unwrap().recipe.repos[0].components, vec!["stable"]);

        let (_, r) = run_tool(&mut p, "add_entry", &json!({ "kind": "steps", "fields": { "name": "a", "run": "echo 1\necho 2\n", "bogus": 1 } }));
        assert!(r.is_err(), "unknown fields are rejected");
        assert!(p.recipe.as_ref().unwrap().recipe.steps.is_empty());

        let (_, r) = run_tool(&mut p, "remove_from_list", &json!({ "list": "packages.install", "item": "nope" }));
        assert!(r.is_err());
        let (_, r) = run_tool(&mut p, "remove_entry", &json!({ "kind": "users", "index": 0 }));
        assert!(r.unwrap_err().contains("does not exist"));

        let (_, r) = run_tool(&mut p, "set_field", &json!({ "section": "system", "key": "hostname", "value": null }));
        assert!(r.is_ok());
        assert!(p.recipe.as_ref().unwrap().recipe.system.hostname.is_none());

        let (_, r) = run_tool(&mut p, "replace_recipe", &json!({ "toml": "not toml [" }));
        assert!(r.is_err());
        let (_, r) = run_tool(&mut p, "get_recipe", &json!({}));
        assert!(r.unwrap().contains("[[repos]]"));
    }
}
