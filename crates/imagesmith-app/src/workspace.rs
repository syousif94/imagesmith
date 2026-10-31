//! Root view of the main window: sidebar, toolbar, tabs, sheets, and the
//! single live build.

use crate::text_input::{InputEvent, TextInput};
use crate::theme::{settings, Colors, UI_FONT};
use crate::widgets::{self, button, fmt_ms, pill, tooltip, ClickIf, Kind};
use futures::channel::mpsc::unbounded;
use futures::StreamExt;
use gpui::prelude::*;
use gpui::{
    actions, anchored, deferred, div, px, AnyElement, App, Context, Entity, ExternalPaths, FocusHandle,
    Focusable, MouseButton, MouseDownEvent, MouseMoveEvent, PathPromptOptions, Pixels, Point, PromptLevel,
    SharedString, Subscription, Task, UniformListScrollHandle, Window,
};
use imagesmith_core::build::{self, CancelToken, Options, Outcome, Update};
use imagesmith_core::events::Line;
use imagesmith_core::probe::Probe;
use imagesmith_core::project::{BuildSummary, Project, ProjectIndex};
use imagesmith_core::recipe::{Arch, PartitionRef, RecipeFile};
use imagesmith_core::runner::{Runtime, RuntimeStatus, STORE, STORE_MOUNT};
use imagesmith_core::size::human;
use imagesmith_core::tracker::{BuildState, LogLine, Tracker};
use imagesmith_core::validate::{has_errors, resolve_project_path, Diagnostic, Severity};
use imagesmith_core::Error;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime};

actions!(
    imagesmith,
    [
        NewProject,
        OpenProject,
        StartBuild,
        CancelBuild,
        ShowRecipe,
        ShowBuild,
        ShowOutput,
        ShowFiles,
        ToggleToml,
        ToggleAssistant,
        SearchLog,
        OpenShell,
        CloseShell,
        OpenSettings,
        Quit,
        CloseWindow,
        Minimize,
        ShowMainWindow,
        Zoom,
        HideApp,
        HideOthers,
        ShowAll,
    ]
);

pub const IDLE_STOP: Duration = Duration::from_secs(600);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Recipe,
    Build,
    Output,
    Files,
}

pub fn runtime(cx: &App) -> Runtime {
    Runtime::from_settings(settings(cx))
}

// ---------------------------------------------------------------- projects

pub struct ProjectState {
    pub project: Project,
    pub recipe: Option<RecipeFile>,
    pub parse_error: Option<(usize, String)>,
    pub text: String,
    pub diagnostics: Vec<Diagnostic>,
    pub builds: Vec<BuildSummary>,
    pub probe: Option<Probe>,
    pub mtime: Option<SystemTime>,
    pub has_stage: bool,
}

impl ProjectState {
    pub fn load(project: Project) -> ProjectState {
        let mut p = ProjectState {
            project,
            recipe: None,
            parse_error: None,
            text: String::new(),
            diagnostics: Vec::new(),
            builds: Vec::new(),
            probe: None,
            mtime: None,
            has_stage: false,
        };
        p.reload();
        p.has_stage = !p.builds.is_empty();
        p
    }

    pub fn id(&self) -> &str {
        &self.project.id
    }

    pub fn recipe_mtime(&self) -> Option<SystemTime> {
        std::fs::metadata(self.project.recipe_path()).and_then(|m| m.modified()).ok()
    }

    pub fn reload(&mut self) {
        self.mtime = self.recipe_mtime();
        self.text = std::fs::read_to_string(self.project.recipe_path()).unwrap_or_default();
        match self.project.load_recipe() {
            Ok(rf) => {
                self.recipe = Some(rf);
                self.parse_error = None;
            }
            Err(Error::RecipeParse { line, message }) => {
                self.parse_error = Some((line, message));
            }
            Err(e) => self.parse_error = Some((0, e.to_string())),
        }
        self.builds = self.project.builds();
        self.revalidate();
    }

    pub fn revalidate(&mut self) {
        if let Some(rf) = &self.recipe {
            let base = resolve_project_path(&self.project.path, &rf.recipe.image.base);
            self.probe = imagesmith_core::hashing::cached_sha256(&base).and_then(|s| Probe::load_cached(&s));
            self.diagnostics = if self.parse_error.is_some() { Vec::new() } else { build::validate_project(&self.project, &rf.recipe) };
        }
    }

    /// Applies a document edit, saves, and re-validates.
    pub fn edit(&mut self, f: impl FnOnce(&mut toml_edit::DocumentMut)) -> Result<(), String> {
        let rf = self.recipe.as_mut().ok_or("recipe has a parse error")?;
        rf.update(f).map_err(|e| e.to_string())?;
        self.text = rf.doc.to_string();
        self.mtime = self.recipe_mtime();
        self.revalidate();
        Ok(())
    }

    pub fn name(&self) -> String {
        self.recipe
            .as_ref()
            .map(|r| r.recipe.image.name.clone())
            .unwrap_or_else(|| self.project.path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default())
    }

    pub fn arch(&self) -> &'static str {
        self.recipe.as_ref().map(|r| r.recipe.image.arch.as_str()).unwrap_or("?")
    }

    pub fn blocking(&self) -> Vec<&Diagnostic> {
        self.diagnostics.iter().filter(|d| d.severity == Severity::Error).collect()
    }

    pub fn can_build(&self) -> bool {
        self.recipe.is_some() && self.parse_error.is_none() && !has_errors(&self.diagnostics)
    }
}

// ---------------------------------------------------------------- builds

/// What the Build and Output tabs show: the live build or a past one.
pub struct BuildView {
    pub project_id: String,
    pub build_id: Option<String>,
    pub state: BuildState,
    pub tracker: Tracker,
    pub live: bool,
    pub extract_only: bool,
    pub started: Instant,
    pub run_started: Option<Instant>,
    pub duration_ms: Option<u64>,
    pub note: Option<String>,
    pub prep: Option<(String, u64, u64)>,
    pub error: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
    pub dir: Option<PathBuf>,
    pub cancel: Option<CancelToken>,
}

impl BuildView {
    pub fn elapsed_ms(&self) -> u64 {
        match (self.live, self.duration_ms) {
            (false, Some(d)) => d,
            (true, _) => self.started.elapsed().as_millis() as u64,
            _ => self.tracker.duration_ms.unwrap_or(0),
        }
    }
}

enum Msg {
    State(BuildState),
    Note(String),
    Prep(String, u64, u64),
    Diagnostics(Vec<Diagnostic>),
    Line(Line),
    Done(Result<Outcome, String>),
}

pub struct LogState {
    pub filter_stage: Option<String>,
    pub errors_only: bool,
    pub follow: bool,
    pub search: Entity<TextInput>,
    pub query: String,
    pub hit: usize,
    pub scroll: UniformListScrollHandle,
    /// (key, absolute line indices) for the current filter.
    pub cache: Option<((u64, u64, String, Option<String>, bool), std::rc::Rc<Vec<u64>>)>,
}

// ---------------------------------------------------------------- sheets

pub struct NewProjectSheet {
    pub name: Entity<TextInput>,
    pub parent: PathBuf,
    pub base: Option<PathBuf>,
    pub probing: bool,
    pub progress: Option<String>,
    pub probe: Option<Result<Probe, String>>,
    pub error: Option<String>,
}

pub struct PasswordSheet {
    pub project_id: String,
    pub fields: Vec<(String, Entity<TextInput>, Entity<TextInput>)>,
    pub error: Option<String>,
}

pub enum Sheet {
    NewProject(NewProjectSheet),
    Passwords(PasswordSheet),
    Onboarding { busy: bool, lines: Vec<String>, error: Option<String> },
    Shell { project_id: String, term: Entity<crate::terminal::TerminalView> },
    Step { kind: &'static str, index: Option<usize>, when: imagesmith_core::recipe::StepWhen, name: Entity<TextInput>, area: Entity<crate::editor::TextArea>, _sub: Subscription },
    HostScripts { project_id: String, scripts: Vec<imagesmith_core::recipe::Step> },
    Repo(RepoSheet),
    Flash(FlashSheet),
}

/// Add or edit an extra apt repository.
pub struct RepoSheet {
    pub index: Option<usize>,
    pub name: Entity<TextInput>,
    pub url: Entity<TextInput>,
    pub suite: Entity<TextInput>,
    pub components: Entity<TextInput>,
    pub key: Entity<TextInput>,
    pub error: Option<String>,
}

/// State of the "Write to SD Card" sheet.
pub struct FlashSheet {
    pub image: PathBuf,
    pub source: Result<imagesmith_core::flash::Source, String>,
    pub devices: Vec<imagesmith_core::flash::Device>,
    pub selected: Option<String>,
    pub show_all: bool,
    pub verify: bool,
    pub eject: bool,
    pub busy: bool,
    pub stage: Option<String>,
    /// phase, bytes done, bytes expected
    pub progress: Option<(String, u64, Option<u64>)>,
    pub error: Option<String>,
    pub done: Option<String>,
    pub started: Option<Instant>,
    pub cancel: Option<CancelToken>,
}

impl FlashSheet {
    pub fn device(&self) -> Option<&imagesmith_core::flash::Device> {
        self.selected.as_ref().and_then(|id| self.devices.iter().find(|d| &d.id == id))
    }

    /// Why flashing can't start right now, if anything.
    pub fn blocker(&self) -> Option<String> {
        if let Err(e) = &self.source {
            return Some(e.clone());
        }
        let d = self.device()?;
        if d.is_system() {
            return Some(format!("{} holds the running system", d.id));
        }
        if !d.writable {
            return Some(format!("{} is write protected", d.id));
        }
        if d.is_protected() {
            return Some(format!("{} is an internal disk", d.id));
        }
        let want = self.source.as_ref().ok()?.uncompressed_bytes?;
        (want > d.size_bytes && d.size_bytes > 0)
            .then(|| format!("the image needs {} but {} holds {}", human(want), d.id, human(d.size_bytes)))
    }
}

#[derive(Debug)]
pub enum FlashMsg {
    Stage(String),
    Progress(String, u64, Option<u64>),
    Note(String),
    Done(Result<u64, String>),
}

#[derive(Clone)]
pub enum MenuAction {
    RevealProject(usize),
    DuplicateProject(usize),
    RemoveProject(usize),
    RevealBuild(PathBuf),
    DeleteBuild(usize, String),
    RebuildWith(usize, PathBuf),
}

pub struct ContextMenu {
    pub pos: Point<Pixels>,
    pub items: Vec<(&'static str, MenuAction)>,
}

// ---------------------------------------------------------------- workspace

pub struct Workspace {
    pub focus: FocusHandle,
    pub projects: Vec<ProjectState>,
    pub active: Option<usize>,
    pub tab: Tab,
    pub runtime: RuntimeStatus,
    pub runtime_text: Option<String>,
    pub runtime_popover: bool,
    pub live: Option<BuildView>,
    pub shown: HashMap<String, BuildView>,
    pub selected_build: HashMap<String, String>,
    pub queue: VecDeque<(String, Options)>,
    pub sheet: Option<Sheet>,
    pub menu: Option<ContextMenu>,
    pub toast: Option<(SharedString, Instant)>,
    pub log: LogState,
    pub recipe_ui: crate::recipe_tab::RecipeUi,
    pub assistant: crate::assistant::AssistantUi,
    pub files: HashMap<String, Entity<crate::files_tab::FilesView>>,
    pub sidebar_width: Pixels,
    pub resizing_sidebar: bool,
    pub validating: bool,
    pub compare_with: Option<String>,
    pub output_cache: Option<(PathBuf, Option<PathBuf>, std::rc::Rc<crate::output_tab::OutputData>)>,
    pub output_open: std::collections::HashSet<String>,
    pub last_store_use: Instant,
    pub host_ok: Result<(), String>,
    _subs: Vec<Subscription>,
    _tasks: Vec<Task<()>>,
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn host_check() -> Result<(), String> {
    if std::env::consts::ARCH != "aarch64" {
        return Err("ImageSmith needs a Mac with Apple silicon to build images.".into());
    }
    let v = std::process::Command::new("sw_vers").arg("-productVersion").output().ok();
    let major: u32 = v
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().split('.').next().and_then(|m| m.parse().ok()))
        .unwrap_or(0);
    if major < 26 {
        return Err(format!("ImageSmith needs macOS 26 or later to build images (this Mac runs {major})."));
    }
    Ok(())
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Workspace {
        let search = cx.new(|cx| TextInput::new(cx, "Search log"));
        let sub = cx.subscribe(&search, |this: &mut Workspace, input, e: &InputEvent, cx| {
            this.log.query = input.read(cx).text();
            this.log.hit = 0;
            if matches!(e, InputEvent::Submit) {
                this.next_search_hit(cx);
            }
            cx.notify();
        });
        let appearance = cx.observe_window_appearance(window, |_, _, cx| cx.notify());
        let mut projects = Vec::new();
        for e in ProjectIndex::load().projects.into_iter().filter(|e| !e.hidden) {
            if e.path.join(imagesmith_core::project::RECIPE).is_file() {
                if let Ok(p) = Project::open(&e.path) {
                    projects.push(ProjectState::load(p));
                }
            }
        }
        let mut ws = Workspace {
            focus: cx.focus_handle(),
            active: if projects.is_empty() { None } else { Some(0) },
            projects,
            tab: Tab::Recipe,
            runtime: RuntimeStatus::Starting,
            runtime_text: None,
            runtime_popover: false,
            live: None,
            shown: HashMap::new(),
            selected_build: HashMap::new(),
            queue: VecDeque::new(),
            sheet: None,
            menu: None,
            toast: None,
            log: LogState {
                filter_stage: None,
                errors_only: false,
                follow: true,
                search,
                query: String::new(),
                hit: 0,
                scroll: UniformListScrollHandle::new(),
                cache: None,
            },
            recipe_ui: crate::recipe_tab::RecipeUi::default(),
            assistant: crate::assistant::AssistantUi::new(cx),
            files: HashMap::new(),
            sidebar_width: px(240.),
            resizing_sidebar: false,
            validating: false,
            compare_with: None,
            output_cache: None,
            output_open: std::collections::HashSet::new(),
            last_store_use: Instant::now(),
            host_ok: host_check(),
            _subs: vec![sub, appearance],
            _tasks: Vec::new(),
        };
        ws.activate(ws.active, window, cx);
        ws.check_runtime(true, cx);
        ws.start_poller(cx);
        ws
    }

    pub fn active(&self) -> Option<&ProjectState> {
        self.active.and_then(|i| self.projects.get(i))
    }

    pub fn active_mut(&mut self) -> Option<&mut ProjectState> {
        self.active.and_then(|i| self.projects.get_mut(i))
    }

    pub fn toast(&mut self, msg: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast = Some((msg.into(), Instant::now()));
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(3200)).await;
            let _ = this.update(cx, |this, cx| {
                if this.toast.as_ref().is_some_and(|(_, t)| t.elapsed() >= Duration::from_secs(3)) {
                    this.toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    pub fn alert(&mut self, title: &str, detail: &str, window: &mut Window, cx: &mut Context<Self>) {
        drop(window.prompt(PromptLevel::Warning, title, Some(detail), &["OK"], cx));
    }

    pub fn activate(&mut self, i: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.active = i;
        self.menu = None;
        self.log.filter_stage = None;
        self.log.cache = None;
        self.compare_with = None;
        let Some(i) = i else {
            window.set_window_title("ImageSmith");
            return;
        };
        let p = &mut self.projects[i];
        p.reload();
        window.set_window_title(&format!("{} — ImageSmith", p.name()));
        self.recipe_ui.sync(&self.projects[i], window, cx);
        let id = self.projects[i].id().to_string();
        if !self.shown.contains_key(&id) && self.live.as_ref().is_none_or(|l| l.project_id != id) {
            let last = self.projects[i].builds.first().map(|b| b.record.id.clone());
            if let Some(b) = last {
                self.select_build(i, &b, cx);
            }
        }
        cx.notify();
    }

    // ------------------------------------------------------------ runtime

    pub fn check_runtime(&mut self, at_launch: bool, cx: &mut Context<Self>) {
        let rt = runtime(cx);
        let start = at_launch && settings(cx).start_runtime_at_launch;
        if self.host_ok.is_err() {
            self.runtime = RuntimeStatus::Unavailable(self.host_ok.clone().unwrap_err());
            if at_launch {
                self.sheet = Some(Sheet::Onboarding { busy: false, lines: Vec::new(), error: None });
            }
            return;
        }
        self.runtime = RuntimeStatus::Starting;
        cx.notify();
        let task = cx.background_spawn(async move {
            let mut s = rt.status();
            if start && s != RuntimeStatus::Ready && rt.bin.exists() {
                let _ = rt.start_system();
                s = rt.status();
            }
            (s, rt.status_text())
        });
        cx.spawn(async move |this, cx| {
            let (s, text) = task.await;
            let _ = this.update(cx, |this, cx| {
                if at_launch && matches!(s, RuntimeStatus::Unavailable(_)) {
                    this.sheet = Some(Sheet::Onboarding { busy: false, lines: Vec::new(), error: None });
                }
                if !at_launch && s == RuntimeStatus::Ready {
                    if let Some(Sheet::Onboarding { .. }) = this.sheet {
                        this.sheet = None;
                    }
                }
                this.runtime = s;
                this.runtime_text = Some(text);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn start_runtime(&mut self, cx: &mut Context<Self>) {
        let rt = runtime(cx);
        self.runtime = RuntimeStatus::Starting;
        cx.notify();
        let task = cx.background_spawn(async move {
            let r = rt.start_system();
            (r.err().map(|e| e.to_string()), rt.status(), rt.status_text())
        });
        cx.spawn(async move |this, cx| {
            let (err, s, text) = task.await;
            let _ = this.update(cx, |this, cx| {
                this.runtime = s;
                this.runtime_text = Some(err.unwrap_or(text));
                cx.notify();
            });
        })
        .detach();
    }

    /// Onboarding: builds the builder image and creates the volume and store.
    pub fn setup_runtime(&mut self, cx: &mut Context<Self>) {
        let rt = runtime(cx);
        let (tx, mut rx) = unbounded::<Result<String, Option<String>>>();
        if let Some(Sheet::Onboarding { busy, lines, error }) = &mut self.sheet {
            *busy = true;
            lines.clear();
            *error = None;
        }
        std::thread::spawn(move || {
            if rt.status() != RuntimeStatus::Ready {
                let _ = tx.unbounded_send(Ok("Starting container system …".into()));
                if let Err(e) = rt.start_system() {
                    let _ = tx.unbounded_send(Err(Some(e.to_string())));
                    return;
                }
            }
            let r = rt.ensure_store(&mut |l| {
                let _ = tx.unbounded_send(Ok(l.to_string()));
            });
            let _ = tx.unbounded_send(Err(r.err().map(|e| e.to_string())));
        });
        cx.spawn(async move |this, cx| {
            while let Some(m) = rx.next().await {
                let done = m.is_err();
                let ok = this.update(cx, |this, cx| {
                    if let Some(Sheet::Onboarding { busy, lines, error }) = &mut this.sheet {
                        match m {
                            Ok(l) => {
                                lines.push(l);
                                if lines.len() > 400 {
                                    lines.drain(..100);
                                }
                            }
                            Err(e) => {
                                *busy = false;
                                *error = e.clone();
                                if e.is_none() {
                                    this.sheet = None;
                                    this.toast("Runtime ready", cx);
                                }
                            }
                        }
                    }
                    if done {
                        this.check_runtime(false, cx);
                    }
                    cx.notify();
                });
                if ok.is_err() || done {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn start_poller(&mut self, cx: &mut Context<Self>) {
        let t = cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(1000)).await;
            let r = this.update(cx, |this, cx| this.poll(cx));
            if r.is_err() {
                break;
            }
        });
        self._tasks.push(t);
    }

    /// Picks up hand edits to recipe.toml and stops the idle store container.
    fn poll(&mut self, cx: &mut Context<Self>) {
        if let Some(i) = self.active {
            let p = &mut self.projects[i];
            let m = p.recipe_mtime();
            if m != p.mtime {
                p.reload();
                self.recipe_ui.dirty = true;
                cx.notify();
            }
        }
        let files_active = self.tab == Tab::Files;
        if self.live.is_none() && !files_active && self.runtime == RuntimeStatus::Ready && self.last_store_use.elapsed() > IDLE_STOP {
            self.last_store_use = Instant::now() + Duration::from_secs(365 * 86400);
            let rt = runtime(cx);
            cx.background_spawn(async move {
                let _ = rt.stop_store();
            })
            .detach();
        }
        if files_active {
            self.last_store_use = Instant::now();
        }
    }

    pub fn touch_store(&mut self) {
        self.last_store_use = Instant::now();
    }

    // ------------------------------------------------------------ builds

    pub fn current_view(&self) -> Option<&BuildView> {
        let id = self.active()?.id();
        match &self.live {
            Some(l) if l.project_id == id => Some(l),
            _ => self.shown.get(id),
        }
    }

    pub fn current_view_mut(&mut self) -> Option<&mut BuildView> {
        let id = self.active()?.id().to_string();
        match &mut self.live {
            Some(l) if l.project_id == id => Some(l),
            _ => self.shown.get_mut(&id),
        }
    }

    pub fn is_building(&self, project_id: &str) -> bool {
        self.live.as_ref().is_some_and(|l| l.project_id == project_id)
    }

    pub fn select_build(&mut self, i: usize, build_id: &str, cx: &mut Context<Self>) {
        let p = &self.projects[i];
        let Some(b) = p.builds.iter().find(|b| b.record.id == build_id) else { return };
        let pid = p.id().to_string();
        let dir = b.dir.clone();
        let record = b.record.clone();
        self.selected_build.insert(pid.clone(), build_id.to_string());
        self.log.filter_stage = None;
        self.log.cache = None;
        self.compare_with = None;
        let events = dir.join("events.jsonl");
        let task = cx.background_spawn(async move { build::replay(&events, None).map_err(|e| e.to_string()) });
        cx.spawn(async move |this, cx| {
            let tracker = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.selected_build.get(&pid).map(String::as_str) != Some(record.id.as_str()) {
                    return;
                }
                let (tracker, error) = match tracker {
                    Ok(t) => (t, None),
                    Err(e) => (Tracker::new(None), Some(format!("cannot read events.jsonl: {e}"))),
                };
                let error = error.or_else(|| tracker.failure.as_ref().map(|f| f.message.clone()));
                this.shown.insert(
                    pid.clone(),
                    BuildView {
                        project_id: pid,
                        build_id: Some(record.id.clone()),
                        state: record.result,
                        tracker,
                        live: false,
                        extract_only: false,
                        started: Instant::now(),
                        run_started: None,
                        duration_ms: Some(record.duration_ms),
                        note: None,
                        prep: None,
                        error,
                        diagnostics: Vec::new(),
                        dir: Some(dir),
                        cancel: None,
                    },
                );
                this.log.cache = None;
                this.log.follow = false;
                this.log.scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
                cx.notify();
            });
        })
        .detach();
    }

    pub fn request_build(&mut self, extract_only: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.active() else { return };
        if !p.can_build() {
            let list: Vec<String> = p.blocking().iter().map(|d| format!("• {}", d.message)).collect();
            let detail = if p.parse_error.is_some() { "recipe.toml has a parse error".to_string() } else { list.join("\n") };
            self.alert("The recipe has blocking errors", &detail, window, cx);
            return;
        }
        if self.is_building(p.id()) {
            return;
        }
        let id = p.id().to_string();
        let pending: Vec<_> = match &p.recipe {
            Some(r) if !extract_only => imagesmith_core::host::unapproved(&p.project, &r.recipe).into_iter().cloned().collect(),
            _ => vec![],
        };
        if !pending.is_empty() {
            self.sheet = Some(Sheet::HostScripts { project_id: id, scripts: pending });
            cx.notify();
            return;
        }
        let users = p.recipe.as_ref().map(|r| build::prompt_users(&r.recipe)).unwrap_or_default();
        if !users.is_empty() && !extract_only {
            let fields = users
                .into_iter()
                .map(|u| {
                    let a = cx.new(|cx| TextInput::new(cx, "Password").masked());
                    let b = cx.new(|cx| TextInput::new(cx, "Confirm").masked());
                    (u, a, b)
                })
                .collect::<Vec<_>>();
            if let Some((_, a, _)) = fields.first() {
                window.focus(&a.focus_handle(cx));
            }
            self.sheet = Some(Sheet::Passwords(PasswordSheet { project_id: id, fields, error: None }));
            cx.notify();
            return;
        }
        self.enqueue(id, Options { extract_only, ..Default::default() }, cx);
    }

    pub fn approve_host_scripts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Sheet::HostScripts { project_id, scripts }) = self.sheet.take() else { return };
        window.focus(&self.focus);
        let Some(p) = self.projects.iter().find(|p| p.id() == project_id) else { return };
        if let Err(e) = imagesmith_core::host::approve(&p.project, &scripts) {
            self.alert("Could not save the approval", &e.to_string(), window, cx);
            return;
        }
        if self.active().is_some_and(|p| p.id() == project_id) {
            self.request_build(false, window, cx);
        }
        cx.notify();
    }

    pub fn submit_passwords(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Sheet::Passwords(s)) = &mut self.sheet else { return };
        let mut passwords = HashMap::new();
        for (u, a, b) in &s.fields {
            let (a, b) = (a.read(cx).text(), b.read(cx).text());
            if a.is_empty() {
                s.error = Some(format!("Enter a password for {u}"));
                cx.notify();
                return;
            }
            if a != b {
                s.error = Some(format!("Passwords for {u} do not match"));
                cx.notify();
                return;
            }
            passwords.insert(u.clone(), a);
        }
        let id = s.project_id.clone();
        self.sheet = None;
        window.focus(&self.focus);
        self.enqueue(id, Options { passwords, ..Default::default() }, cx);
    }

    fn enqueue(&mut self, project_id: String, opts: Options, cx: &mut Context<Self>) {
        if self.live.is_some() {
            if !self.queue.iter().any(|(p, _)| *p == project_id) {
                self.queue.push_back((project_id, opts));
                self.toast("Build queued; it starts when the current build finishes", cx);
            }
            return;
        }
        self.start(project_id, opts, cx);
    }

    fn start(&mut self, project_id: String, mut opts: Options, cx: &mut Context<Self>) {
        let Some(p) = self.projects.iter().find(|p| p.id() == project_id) else { return };
        let Some(rf) = &p.recipe else { return };
        let project = Project { path: p.project.path.clone(), id: p.project.id.clone() };
        opts.cache_cap_bytes = Some(settings(cx).cache_cap_gb << 30);
        let extract_only = opts.extract_only;
        let cancel = CancelToken::default();
        self.live = Some(BuildView {
            project_id: project_id.clone(),
            build_id: None,
            state: BuildState::Queued,
            tracker: Tracker::new(Some(&rf.recipe)),
            live: true,
            extract_only,
            started: Instant::now(),
            run_started: None,
            duration_ms: None,
            note: Some("Starting …".into()),
            prep: None,
            error: None,
            diagnostics: Vec::new(),
            dir: None,
            cancel: Some(cancel.clone()),
        });
        self.selected_build.remove(&project_id);
        self.log.filter_stage = None;
        self.log.follow = true;
        self.log.cache = None;
        if self.active().is_some_and(|p| p.id() == project_id) && self.tab != Tab::Files {
            self.tab = Tab::Build;
        }
        self.touch_store();

        let rt = runtime(cx);
        let (tx, mut rx) = unbounded::<Msg>();
        std::thread::spawn(move || {
            let send = |m: Msg| {
                let _ = tx.unbounded_send(m);
            };
            let mut last_pct = (u64::MAX, String::new());
            let r = build::run(&rt, &project, &opts, &cancel, &mut |u| match u {
                Update::State(s) => send(Msg::State(s)),
                Update::Note(n) => send(Msg::Note(n.to_string())),
                Update::Hashing(d, t) | Update::Importing(d, t) => {
                    let label = if matches!(u, Update::Hashing(..)) { "Hashing base" } else { "Importing base" };
                    let pct = if t > 0 { d * 200 / t } else { 0 };
                    if (pct, label.to_string()) != last_pct {
                        last_pct = (pct, label.to_string());
                        send(Msg::Prep(label.to_string(), d, t));
                    }
                }
                Update::Diagnostics(d) => send(Msg::Diagnostics(d.to_vec())),
                Update::Line(l, _) => send(Msg::Line(l.clone())),
            });
            drop(opts);
            send(Msg::Done(r.map_err(|e| match e {
                Error::Invalid(n) => format!("the recipe has {n} blocking error(s)"),
                e => e.to_string(),
            })));
        });
        cx.spawn(async move |this, cx| {
            while let Some(first) = rx.next().await {
                let mut batch = vec![first];
                while let Ok(m) = rx.try_recv() {
                    batch.push(m);
                    if batch.len() >= 4096 {
                        break;
                    }
                }
                if this.update(cx, |this, cx| this.apply_msgs(batch, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(200)).await;
            let alive = this.update(cx, |this, cx| {
                cx.notify();
                this.live.is_some()
            });
            if !matches!(alive, Ok(true)) {
                break;
            }
        })
        .detach();
        cx.notify();
    }

    fn apply_msgs(&mut self, batch: Vec<Msg>, cx: &mut Context<Self>) {
        let mut done = None;
        let mut new_lines = false;
        if let Some(live) = &mut self.live {
            for m in batch {
                match m {
                    Msg::State(s) => {
                        live.state = s;
                        if s == BuildState::Running {
                            live.run_started = Some(Instant::now());
                            live.note = None;
                            live.prep = None;
                        }
                    }
                    Msg::Note(n) => {
                        live.tracker.log.push(LogLine { stage: None, text: format!("· {n}") });
                        live.note = Some(n);
                        new_lines = true;
                    }
                    Msg::Prep(l, d, t) => live.prep = Some((l, d, t)),
                    Msg::Diagnostics(d) => live.diagnostics = d,
                    Msg::Line(l) => {
                        let t = live.run_started.map(|s| s.elapsed().as_millis() as u64).unwrap_or(0);
                        live.tracker.apply(&l, t);
                        new_lines = true;
                    }
                    Msg::Done(r) => done = Some(r),
                }
            }
        }
        if new_lines {
            self.log.cache = None;
            if self.log.follow {
                let n = self.visible_log_len();
                if n > 0 {
                    self.log.scroll.scroll_to_item(n - 1, gpui::ScrollStrategy::Bottom);
                }
            }
        }
        if let Some(r) = done {
            self.finish(r, cx);
        }
        self.touch_store();
        cx.notify();
    }

    fn finish(&mut self, r: Result<Outcome, String>, cx: &mut Context<Self>) {
        let Some(mut live) = self.live.take() else { return };
        let cancelled = live.cancel.take().is_some_and(|c| c.is_cancelled());
        live.live = false;
        live.note = None;
        live.prep = None;
        live.duration_ms = Some(live.started.elapsed().as_millis() as u64);
        match r {
            Ok(o) => {
                live.state = o.state;
                live.tracker = o.tracker;
                live.error = o.error;
                live.build_id = o.dir.as_ref().map(|_| o.build_id.clone());
                live.dir = o.dir;
                if let Some(d) = live.tracker.duration_ms {
                    live.duration_ms = Some(d);
                }
            }
            Err(e) => {
                live.state = if cancelled { BuildState::Cancelled } else { BuildState::Failed };
                live.tracker.log.push(LogLine { stage: None, text: format!("error: {e}") });
                live.error = Some(e);
            }
        }
        let pid = live.project_id.clone();
        let failed_line = live.tracker.failure.as_ref().map(|f| (f.stage.clone(), f.log_line));
        let state = live.state;
        let extract_only = live.extract_only;
        if let Some(b) = &live.build_id {
            self.selected_build.insert(pid.clone(), b.clone());
        }
        if let Some(p) = self.projects.iter_mut().find(|p| p.id() == pid) {
            if !live.diagnostics.is_empty() {
                p.diagnostics = live.diagnostics.clone();
            }
            p.builds = p.project.builds();
            if matches!(state, BuildState::Succeeded) || live.tracker.stages.iter().any(|s| s.id == "snapshot" && s.status != imagesmith_core::tracker::StageStatus::Pending) {
                p.has_stage = true;
            }
            p.revalidate();
        }
        self.shown.insert(pid.clone(), live);
        self.log.cache = None;
        if let Some((_, line)) = failed_line {
            self.log.follow = false;
            self.log.filter_stage = None;
            let ix = self.visible_index_of(line);
            self.log.scroll.scroll_to_item(ix, gpui::ScrollStrategy::Center);
        }
        if let Some(f) = self.files.get(&pid) {
            f.update(cx, |f, cx| f.refresh(cx));
        }
        let msg = match state {
            BuildState::Succeeded if extract_only => "Base extracted; browse it in the Files tab".to_string(),
            BuildState::Succeeded => "Build succeeded".to_string(),
            BuildState::Cancelled => "Build cancelled".to_string(),
            _ => "Build failed".to_string(),
        };
        self.toast(msg, cx);
        if let Some((next, opts)) = self.queue.pop_front() {
            self.start(next, opts, cx);
        }
    }

    pub fn cancel_build(&mut self, cx: &mut Context<Self>) {
        if let Some(l) = &mut self.live {
            if let Some(c) = &l.cancel {
                c.cancel();
                l.note = Some("Cancelling …".into());
            }
        }
        cx.notify();
    }

    // ------------------------------------------------------------ actions

    pub fn new_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| TextInput::new(cx, "rk3588-edge"));
        window.focus(&name.focus_handle(cx));
        let parent = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
        self.sheet = Some(Sheet::NewProject(NewProjectSheet {
            name,
            parent,
            base: None,
            probing: false,
            progress: None,
            probe: None,
            error: None,
        }));
        cx.notify();
    }

    pub fn open_project_dialog(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open Project".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(p) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, cx| this.open_path(&p, cx));
                }
            }
        })
        .detach();
    }

    pub fn open_path(&mut self, path: &Path, cx: &mut Context<Self>) {
        match Project::open(path) {
            Ok(p) => {
                let i = match self.projects.iter().position(|x| x.project.path == p.path) {
                    Some(i) => i,
                    None => {
                        self.projects.push(ProjectState::load(p));
                        self.projects.len() - 1
                    }
                };
                self.defer_activate(i, cx);
            }
            Err(e) => self.toast(format!("Cannot open project: {e}"), cx),
        }
    }

    /// Activation needs the window; defer it from contexts that lack one.
    pub fn defer_activate(&mut self, i: usize, cx: &mut Context<Self>) {
        self.active = Some(i);
        self.recipe_ui.dirty = true;
        self.recipe_ui.pending_activate = true;
        cx.notify();
    }

    pub fn open_shell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.active() else { return };
        if matches!(self.sheet, Some(Sheet::Shell { .. })) {
            return;
        }
        let id = p.id().to_string();
        let rt = runtime(cx);
        self.touch_store();
        self.toast("Starting shell …", cx);
        let task = cx.background_spawn({
            let rt = rt.clone();
            let stage = format!("{STORE_MOUNT}/projects/{id}/stage");
            async move {
                rt.ensure_store(&mut |_| {}).map_err(|e| e.to_string())?;
                let exists = rt.exec(false, &["test", "-d", &stage]).stdin(Stdio::null()).status().is_ok_and(|s| s.success());
                if exists { Ok(()) } else { Err("nothing to open yet. Build, or extract the base in the Files tab first".to_string()) }
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let r = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if let Err(e) = r {
                    this.toast(format!("Shell failed: {e}"), cx);
                    return;
                }
                let args = ["exec", "-it", STORE, "/usr/local/lib/imagesmith/shell.sh", id.as_str()].map(String::from).to_vec();
                let bin = rt.bin.to_string_lossy().into_owned();
                match crate::terminal::TerminalView::spawn(bin, args) {
                    Ok(s) => {
                        let term = cx.new(|cx| crate::terminal::TerminalView::new(s, cx));
                        cx.subscribe_in(&term, window, |this, term, _: &crate::terminal::TerminalEvent, window, cx| {
                            // A clean `exit` leaves the shell; a failure stays open so its status is visible.
                            if term.read(cx).exited == Some(Some(0)) {
                                this.close_shell(true, window, cx);
                            }
                            cx.notify();
                        })
                        .detach();
                        window.focus(&term.focus_handle(cx));
                        this.sheet = Some(Sheet::Shell { project_id: id.clone(), term });
                        this.toast = None;
                        cx.notify();
                    }
                    Err(e) => this.toast(format!("Shell failed: {e}"), cx),
                }
            });
        })
        .detach();
    }

    pub fn validate(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.active() else { return };
        let Some(rf) = &p.recipe else { return };
        let base = resolve_project_path(&p.project.path, &rf.recipe.image.base);
        if !base.is_file() {
            if let Some(p) = self.active_mut() {
                p.revalidate();
            }
            cx.notify();
            return;
        }
        let rt = runtime(cx);
        self.validating = true;
        self.touch_store();
        cx.notify();
        let task = cx.background_spawn(async move {
            rt.ensure_store(&mut |_| {})?;
            build::prepare_base(&rt, &base, &mut |_| {})
        });
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |this, cx| {
                this.validating = false;
                if let Some(p) = this.active_mut() {
                    p.revalidate();
                }
                match r {
                    Ok((_, probe)) => this.toast(format!("Probed base: {}", probe.summary()), cx),
                    Err(e) => this.toast(format!("Probe failed: {e}"), cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn run_menu_action(&mut self, a: MenuAction, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        match a {
            MenuAction::RevealProject(i) => cx.reveal_path(&self.projects[i].project.path.join(imagesmith_core::project::RECIPE)),
            MenuAction::RevealBuild(dir) => cx.reveal_path(&dir),
            MenuAction::DuplicateProject(i) => {
                let src = self.projects[i].project.path.clone();
                let mut dst = src.with_file_name(format!("{}-copy", src.file_name().unwrap_or_default().to_string_lossy()));
                let mut n = 2;
                while dst.exists() {
                    dst = src.with_file_name(format!("{}-copy{n}", src.file_name().unwrap_or_default().to_string_lossy()));
                    n += 1;
                }
                match copy_project(&src, &dst) {
                    Ok(()) => self.open_path(&dst, cx),
                    Err(e) => self.toast(format!("Duplicate failed: {e}"), cx),
                }
            }
            MenuAction::RemoveProject(i) => {
                if self.is_building(self.projects[i].id()) {
                    self.toast("Cannot remove a project while it builds", cx);
                    return;
                }
                let p = self.projects.remove(i);
                let _ = ProjectIndex::load().set_hidden(p.id(), true);
                self.files.remove(p.id());
                self.shown.remove(p.id());
                let next = if self.projects.is_empty() { None } else { Some(i.min(self.projects.len() - 1)) };
                self.activate(next, window, cx);
            }
            MenuAction::DeleteBuild(i, id) => {
                let dir = self.projects[i].project.builds_dir().join(&id);
                let rx = window.prompt(
                    PromptLevel::Warning,
                    &format!("Delete build {id}?"),
                    Some("The image and logs in its builds/ folder are deleted."),
                    &["Delete", "Cancel"],
                    cx,
                );
                let pid = self.projects[i].id().to_string();
                cx.spawn(async move |this, cx| {
                    if rx.await.ok() == Some(0) {
                        let r = std::fs::remove_dir_all(&dir);
                        let _ = this.update(cx, |this, cx| {
                            if let Err(e) = r {
                                this.toast(format!("Delete failed: {e}"), cx);
                            }
                            if this.selected_build.get(&pid) == Some(&id) {
                                this.selected_build.remove(&pid);
                                this.shown.remove(&pid);
                            }
                            if let Some(p) = this.projects.iter_mut().find(|p| p.id() == pid) {
                                p.builds = p.project.builds();
                            }
                            cx.notify();
                        });
                    }
                })
                .detach();
            }
            MenuAction::RebuildWith(i, dir) => {
                let rx = window.prompt(
                    PromptLevel::Warning,
                    "Restore this build's recipe?",
                    Some("recipe.toml is replaced with the recipe from recipe.lock.json (comments are lost), then a build starts."),
                    &["Restore and Build", "Cancel"],
                    cx,
                );
                let pid = self.projects[i].id().to_string();
                cx.spawn_in(window, async move |this, cx| {
                    if rx.await.ok() != Some(0) {
                        return;
                    }
                    let r = restore_lock(&dir);
                    let _ = this.update_in(cx, |this, window, cx| match r {
                        Ok(text) => {
                            if let Some(i) = this.projects.iter().position(|p| p.id() == pid) {
                                let _ = std::fs::write(this.projects[i].project.recipe_path(), text);
                                this.activate(Some(i), window, cx);
                                this.request_build(false, window, cx);
                            }
                        }
                        Err(e) => this.toast(format!("Cannot restore recipe: {e}"), cx),
                    });
                })
                .detach();
            }
        }
        cx.notify();
    }

    fn on_drop_paths(&mut self, paths: &ExternalPaths, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = paths.paths().first().cloned() else { return };
        if path.is_dir() && path.join(imagesmith_core::project::RECIPE).is_file() {
            self.open_path(&path, cx);
            return;
        }
        let lower = path.to_string_lossy().to_lowercase();
        if imagesmith_core::validate::BASE_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
            if !matches!(self.sheet, Some(Sheet::NewProject(_))) {
                self.new_project(window, cx);
            }
            self.set_new_project_base(path, cx);
        }
    }

    // ------------------------------------------------------------ render

    fn render_toolbar(&mut self, c: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.active();
        let name = p.map(|p| p.name()).unwrap_or_else(|| "ImageSmith".into());
        let building_here = p.is_some_and(|p| self.is_building(p.id()));
        let can_build = p.is_some_and(|p| p.can_build());
        let has_project = p.is_some();
        let blocking: Vec<String> = p.map(|p| p.blocking().iter().map(|d| d.message.clone()).collect()).unwrap_or_default();
        let parse_err = p.is_some_and(|p| p.parse_error.is_some());
        let runtime_ready = self.runtime == RuntimeStatus::Ready;
        let (pill_text, fg, bg) = match &self.runtime {
            RuntimeStatus::Ready => ("Runtime ready", c.success, c.success_soft),
            RuntimeStatus::Starting => ("Starting…", c.muted, c.hover),
            RuntimeStatus::Unavailable(_) => ("Runtime unavailable", c.danger, c.danger_soft),
        };
        let elapsed = self.live.as_ref().filter(|_| building_here).map(|l| fmt_ms(l.elapsed_ms()));

        let build_btn = if building_here {
            button("build", format!("Cancel  {}", elapsed.unwrap_or_default()), Kind::Primary, true, c)
                .tooltip(tooltip("Cancel the build (⌘.)"))
                .on_click(cx.listener(|this, _, _, cx| this.cancel_build(cx)))
                .into_any_element()
        } else {
            let tip = if parse_err {
                "recipe.toml has a parse error".to_string()
            } else if blocking.is_empty() {
                "Build the image (⌘B)".to_string()
            } else {
                blocking.iter().map(|m| format!("• {m}")).collect::<Vec<_>>().join("\n")
            };
            button("build", "Build", Kind::Primary, can_build, c)
                .tooltip(tooltip(tip))
                .on_click_if(can_build, cx.listener(|this, _, w, cx| this.request_build(false, w, cx)))
                .into_any_element()
        };

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .h(px(46.))
            .pl(px(84.))
            .pr(px(12.))
            .border_b_1()
            .border_color(c.border)
            .bg(c.sidebar)
            .child(div().font_weight(gpui::FontWeight::SEMIBOLD).text_size(px(14.)).child(name))
            .child(
                div()
                    .id("runtime-pill")
                    .cursor_pointer()
                    .child(pill(pill_text, fg, bg))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.runtime_popover = !this.runtime_popover;
                        if this.runtime_popover {
                            this.check_runtime(false, cx);
                        }
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .child(
                button("validate", if self.validating { "Validating…" } else { "Validate" }, Kind::Normal, has_project && !self.validating, c)
                    .tooltip(tooltip("Re-run validation and probe the base image"))
                    .on_click_if(has_project && !self.validating, cx.listener(|this, _, _, cx| this.validate(cx))),
            )
            .child(build_btn)
            .child(
                button("shell", "Shell", Kind::Normal, has_project && runtime_ready, c)
                    .tooltip(tooltip("Open a shell chrooted into the project's stage (⌘⇧S)"))
                    .on_click_if(has_project && runtime_ready, cx.listener(|this, _, w, cx| this.open_shell(w, cx))),
            )
            .child(button("reveal", "Reveal", Kind::Normal, has_project, c).tooltip(tooltip("Show the project folder in Finder")).on_click_if(
                has_project,
                cx.listener(|this, _, _, cx| {
                    if let Some(p) = this.active() {
                        cx.reveal_path(&p.project.recipe_path());
                    }
                }),
            ))
            .child(
                button("assistant-toggle", "Assistant", if self.assistant.open { Kind::Primary } else { Kind::Normal }, true, c)
                    .tooltip(tooltip("Show or hide the assistant (⌘⇧A)"))
                    .on_click(cx.listener(|this, _, w, cx| this.toggle_assistant(w, cx))),
            )
    }

    fn render_sidebar(&mut self, c: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div().flex().flex_col().gap(px(1.)).px(px(8.));
        for (i, p) in self.projects.iter().enumerate() {
            let selected = Some(i) == self.active;
            let dot = if self.is_building(p.id()) {
                c.accent
            } else {
                match p.builds.first().map(|b| b.record.result) {
                    Some(BuildState::Succeeded) => c.success,
                    Some(BuildState::Failed) => c.danger,
                    _ => c.faint,
                }
            };
            let hover = c.hover;
            list = list.child(
                div()
                    .id(("project", i))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .h(px(28.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(c.selected).font_weight(gpui::FontWeight::SEMIBOLD))
                    .when(!selected, move |d| d.hover(move |s| s.bg(hover)))
                    .child(div().size(px(7.)).rounded_full().bg(dot))
                    .child(div().flex_1().overflow_hidden().text_ellipsis().whitespace_nowrap().child(p.name()))
                    .child(div().text_size(px(11.)).text_color(c.muted).child(p.arch()))
                    .on_click(cx.listener(move |this, _, w, cx| this.activate(Some(i), w, cx)))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                            this.menu = Some(ContextMenu {
                                pos: e.position,
                                items: vec![
                                    ("Reveal in Finder", MenuAction::RevealProject(i)),
                                    ("Duplicate", MenuAction::DuplicateProject(i)),
                                    ("Remove from List", MenuAction::RemoveProject(i)),
                                ],
                            });
                            cx.notify();
                        }),
                    ),
            );
        }

        let mut builds = div().flex().flex_col().gap(px(1.)).px(px(8.));
        if let Some(ai) = self.active {
            let p = &self.projects[ai];
            let pid = p.id().to_string();
            if let Some(l) = self.live.as_ref().filter(|l| l.project_id == pid) {
                let selected = !self.selected_build.contains_key(&pid);
                builds = builds.child(
                    div()
                        .id("build-live")
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(8.))
                        .h(px(26.))
                        .rounded(px(6.))
                        .cursor_pointer()
                        .when(selected, |d| d.bg(c.selected))
                        .child(div().size(px(7.)).rounded_full().bg(c.accent))
                        .child(div().flex_1().child("Now"))
                        .child(div().text_size(px(11.)).text_color(c.muted).child(if l.extract_only { "extracting" } else { "running" }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(p) = this.active() {
                                let id = p.id().to_string();
                                this.selected_build.remove(&id);
                                this.log.cache = None;
                                this.log.follow = true;
                            }
                            cx.notify();
                        })),
                );
            }
            for b in &p.builds {
                let id = b.record.id.clone();
                let selected = self.selected_build.get(&pid) == Some(&id) && !self.is_building(&pid);
                let (dot, label) = match b.record.result {
                    BuildState::Succeeded => (c.success, fmt_ms(b.record.duration_ms)),
                    BuildState::Failed => (c.danger, "failed".into()),
                    BuildState::Cancelled => (c.faint, "cancelled".into()),
                    _ => (c.faint, "?".into()),
                };
                let when = chrono_label(&b.record.id);
                let dir = b.dir.clone();
                let hover = c.hover;
                let id2 = id.clone();
                builds = builds.child(
                    div()
                        .id(SharedString::from(format!("build-{id}")))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(8.))
                        .h(px(26.))
                        .rounded(px(6.))
                        .cursor_pointer()
                        .when(selected, |d| d.bg(c.selected))
                        .when(!selected, move |d| d.hover(move |s| s.bg(hover)))
                        .child(div().size(px(7.)).rounded_full().bg(dot))
                        .child(div().flex_1().text_size(px(12.5)).child(when))
                        .child(div().text_size(px(11.)).text_color(c.muted).child(label))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_build(ai, &id, cx);
                            if this.tab == Tab::Recipe || this.tab == Tab::Files {
                                this.tab = Tab::Build;
                            }
                            cx.notify();
                        }))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                                this.menu = Some(ContextMenu {
                                    pos: e.position,
                                    items: vec![
                                        ("Reveal in Finder", MenuAction::RevealBuild(dir.clone())),
                                        ("Delete Build…", MenuAction::DeleteBuild(ai, id2.clone())),
                                        ("Rebuild with This Recipe…", MenuAction::RebuildWith(ai, dir.clone())),
                                    ],
                                });
                                cx.notify();
                            }),
                        ),
                );
            }
            if p.builds.is_empty() && !self.is_building(&pid) {
                builds = builds.child(div().px(px(8.)).text_size(px(12.)).text_color(c.faint).child("No builds yet"));
            }
        }

        div()
            .id("sidebar")
            .flex()
            .flex_col()
            .flex_none()
            .w(self.sidebar_width)
            .h_full()
            .bg(c.sidebar)
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(16.))
                    .pt(px(12.))
                    .pb(px(6.))
                    .child(widgets::section_label("PROJECTS", c))
                    .child(
                        div()
                            .id("add-project")
                            .px(px(6.))
                            .rounded(px(4.))
                            .cursor_pointer()
                            .text_color(c.muted)
                            .hover(|s| s.bg(c.hover))
                            .tooltip(tooltip("New Project (⌘N)"))
                            .child("+")
                            .on_click(cx.listener(|this, _, w, cx| this.new_project(w, cx))),
                    ),
            )
            .child(list)
            .when(self.active.is_some(), |d| {
                d.child(div().px(px(16.)).pt(px(18.)).pb(px(6.)).child(widgets::section_label("BUILDS", c))).child(builds)
            })
    }

    fn render_tabs(&mut self, c: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = div().flex().flex_none().items_end().gap(px(18.)).px(px(20.)).h(px(38.)).border_b_1().border_color(c.border);
        for (t, label, key) in [(Tab::Recipe, "Recipe", "⌘1"), (Tab::Build, "Build", "⌘2"), (Tab::Output, "Output", "⌘3"), (Tab::Files, "Files", "⌘4")] {
            let sel = self.tab == t;
            row = row.child(
                div()
                    .id(label)
                    .pb(px(7.))
                    .px(px(2.))
                    .cursor_pointer()
                    .text_size(px(13.5))
                    .border_b_2()
                    .border_color(if sel { c.accent } else { gpui::transparent_black() })
                    .text_color(if sel { c.text } else { c.muted })
                    .when(sel, |d| d.font_weight(gpui::FontWeight::SEMIBOLD))
                    .tooltip(tooltip(format!("{label} ({key})")))
                    .child(label)
                    .on_click(cx.listener(move |this, _, w, cx| this.set_tab(t, w, cx))),
            );
        }
        row
    }

    pub fn set_tab(&mut self, t: Tab, window: &mut Window, cx: &mut Context<Self>) {
        self.tab = t;
        if t == Tab::Files {
            self.ensure_files_view(window, cx);
        }
        cx.notify();
    }

    fn ensure_files_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.active() else { return };
        let id = p.id().to_string();
        if !self.files.contains_key(&id) {
            let path = p.project.path.clone();
            let ws = cx.entity().downgrade();
            let v = cx.new(|cx| crate::files_tab::FilesView::new(id.clone(), path, ws, window, cx));
            self.files.insert(id, v);
        }
        self.touch_store();
    }

    fn render_empty(&mut self, c: &Colors, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("empty")
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(14.))
            .child(div().text_size(px(20.)).font_weight(gpui::FontWeight::SEMIBOLD).child("Create a project from a base image"))
            .child(div().text_color(c.muted).child("Drop a .img, .img.xz or .img.zst here, or start from scratch."))
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(
                        button("empty-new", "New Project", Kind::Primary, true, c)
                            .tooltip(tooltip("New Project (⌘N)"))
                            .on_click(cx.listener(|this, _, w, cx| this.new_project(w, cx))),
                    )
                    .child(
                        button("empty-open", "Open Project…", Kind::Normal, true, c)
                            .tooltip(tooltip("Open Project (⌘O)"))
                            .on_click(cx.listener(|this, _, _, cx| this.open_project_dialog(cx))),
                    ),
            )
            .child(
                div()
                    .mt(px(10.))
                    .w(px(420.))
                    .h(px(120.))
                    .rounded(px(10.))
                    .border_2()
                    .border_dashed()
                    .border_color(c.border)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(c.faint)
                    .child("Drop base image"),
            )
    }

    fn render_menu(&mut self, c: &Colors, cx: &mut Context<Self>) -> Option<AnyElement> {
        let m = self.menu.as_ref()?;
        let mut col = div()
            .id("context-menu")
            .occlude()
            .flex()
            .flex_col()
            .min_w(px(200.))
            .p(px(4.))
            .rounded(px(7.))
            .bg(c.surface)
            .border_1()
            .border_color(c.border)
            .shadow_lg()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.menu = None;
                cx.notify();
            }));
        for (n, (label, action)) in m.items.iter().enumerate() {
            let a = action.clone();
            let hover = c.accent;
            let fg = c.accent_text;
            col = col.child(
                div()
                    .id(("menu-item", n))
                    .px(px(10.))
                    .py(px(4.))
                    .rounded(px(4.))
                    .text_size(px(13.))
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover).text_color(fg))
                    .child(*label)
                    .on_click(cx.listener(move |this, _, w, cx| this.run_menu_action(a.clone(), w, cx))),
            );
        }
        Some(deferred(anchored().position(m.pos).snap_to_window().child(col)).with_priority(2).into_any_element())
    }

    fn render_runtime_popover(&mut self, c: &Colors, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.runtime_popover {
            return None;
        }
        let text = self.runtime_text.clone().unwrap_or_else(|| "Checking …".into());
        let ready = self.runtime == RuntimeStatus::Ready;
        let detail = match &self.runtime {
            RuntimeStatus::Unavailable(m) => Some(m.clone()),
            _ => None,
        };
        let el = div()
            .id("runtime-popover")
            .occlude()
            .w(px(420.))
            .p(px(12.))
            .rounded(px(8.))
            .bg(c.surface)
            .border_1()
            .border_color(c.border)
            .shadow_lg()
            .flex()
            .flex_col()
            .gap(px(8.))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.runtime_popover = false;
                cx.notify();
            }))
            .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("container system status"))
            .children(detail.map(|d| div().text_color(c.danger).text_size(px(12.)).child(d)))
            .child(
                div()
                    .p(px(8.))
                    .rounded(px(5.))
                    .bg(c.log_bg)
                    .border_1()
                    .border_color(c.border)
                    .child(widgets::mono(text.trim().to_string()).text_color(c.muted)),
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .justify_end()
                    .child(button("rt-refresh", "Refresh", Kind::Normal, true, c).on_click(cx.listener(|this, _, _, cx| this.check_runtime(false, cx))))
                    .child(button("rt-setup", "Set Up Builder…", Kind::Normal, self.host_ok.is_ok(), c).on_click_if(
                        self.host_ok.is_ok(),
                        cx.listener(|this, _, _, cx| {
                            this.runtime_popover = false;
                            this.sheet = Some(Sheet::Onboarding { busy: false, lines: Vec::new(), error: None });
                            this.setup_runtime(cx);
                        }),
                    ))
                    .child(button("rt-start", "Start", Kind::Primary, !ready && self.host_ok.is_ok(), c).on_click_if(
                        !ready && self.host_ok.is_ok(),
                        cx.listener(|this, _, _, cx| this.start_runtime(cx)),
                    )),
            );
        Some(deferred(anchored().position(gpui::point(px(150.), px(42.))).snap_to_window().child(el)).with_priority(2).into_any_element())
    }

    fn render_toast(&self, c: &Colors) -> Option<AnyElement> {
        let (msg, _) = self.toast.as_ref()?;
        Some(
            div()
                .absolute()
                .bottom(px(46.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .px(px(14.))
                        .py(px(7.))
                        .rounded(px(8.))
                        .bg(if c.dark { gpui::Hsla::from(gpui::rgb(0x3a3a40)) } else { gpui::Hsla::from(gpui::rgb(0x2c2c2e)) })
                        .text_color(gpui::white())
                        .text_size(px(12.5))
                        .shadow_lg()
                        .child(msg.clone()),
                )
                .into_any_element(),
        )
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = Colors::get(window, cx);
        if self.recipe_ui.pending_activate {
            self.recipe_ui.pending_activate = false;
            let a = self.active;
            self.activate(a, window, cx);
        }
        if self.recipe_ui.dirty {
            self.recipe_ui.dirty = false;
            if let Some(i) = self.active {
                self.recipe_ui.sync(&self.projects[i], window, cx);
            }
        }
        let main: AnyElement = if self.active.is_none() {
            self.render_empty(&c, cx).into_any_element()
        } else {
            let content: AnyElement = match self.tab {
                Tab::Recipe => self.render_recipe_tab(&c, window, cx).into_any_element(),
                Tab::Build => self.render_build_tab(&c, window, cx).into_any_element(),
                Tab::Output => self.render_output_tab(&c, cx).into_any_element(),
                Tab::Files => {
                    self.ensure_files_view(window, cx);
                    let id = self.active().map(|p| p.id().to_string()).unwrap_or_default();
                    match self.files.get(&id) {
                        Some(v) => v.clone().into_any_element(),
                        None => div().into_any_element(),
                    }
                }
            };
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(self.render_tabs(&c, cx))
                .child(div().flex_1().min_h_0().child(content))
                .into_any_element()
        };
        let sheet = self.render_sheet(&c, window, cx);
        let menu = self.render_menu(&c, cx);
        let pop = self.render_runtime_popover(&c, cx);
        let toast = self.render_toast(&c);
        let assistant = self.assistant.open.then(|| self.render_assistant(&c, cx));
        let border = c.border;
        let accent = c.accent;
        let resizing = self.resizing_sidebar;

        div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(c.bg)
            .text_color(c.text)
            .font_family(UI_FONT)
            .text_size(px(13.))
            .on_action(cx.listener(|this, _: &NewProject, w, cx| this.new_project(w, cx)))
            .on_action(cx.listener(|this, _: &OpenProject, _, cx| this.open_project_dialog(cx)))
            .on_action(cx.listener(|this, _: &StartBuild, w, cx| {
                if !this.active().is_some_and(|p| this.is_building(p.id())) {
                    this.request_build(false, w, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &CancelBuild, _, cx| this.cancel_build(cx)))
            .on_action(cx.listener(|this, _: &ShowRecipe, w, cx| this.set_tab(Tab::Recipe, w, cx)))
            .on_action(cx.listener(|this, _: &ShowBuild, w, cx| this.set_tab(Tab::Build, w, cx)))
            .on_action(cx.listener(|this, _: &ShowOutput, w, cx| this.set_tab(Tab::Output, w, cx)))
            .on_action(cx.listener(|this, _: &ShowFiles, w, cx| this.set_tab(Tab::Files, w, cx)))
            .on_action(cx.listener(|this, _: &ToggleToml, w, cx| {
                this.recipe_ui.toml_view = !this.recipe_ui.toml_view;
                this.set_tab(Tab::Recipe, w, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleAssistant, w, cx| this.toggle_assistant(w, cx)))
            .on_action(cx.listener(|this, _: &SearchLog, w, cx| {
                this.set_tab(Tab::Build, w, cx);
                let fh = this.log.search.focus_handle(cx);
                w.focus(&fh);
            }))
            .on_action(cx.listener(|this, _: &OpenShell, w, cx| {
                if this.active().is_some() && this.runtime == RuntimeStatus::Ready {
                    this.open_shell(w, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &CloseShell, w, cx| this.close_shell(false, w, cx)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, w, cx| this.on_drop_paths(paths, w, cx)))
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                if this.resizing_sidebar {
                    this.sidebar_width = e.position.x.clamp(px(200.), px(360.));
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.resizing_sidebar {
                        this.resizing_sidebar = false;
                        cx.notify();
                    }
                }),
            )
            .child(self.render_toolbar(&c, cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(&c, cx))
                    .child(
                        div()
                            .id("sidebar-resize")
                            .w(px(5.))
                            .h_full()
                            .flex_none()
                            .cursor_col_resize()
                            .flex()
                            .justify_center()
                            .child(div().w(px(1.)).h_full().bg(if resizing { accent } else { border }))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| {
                                    this.resizing_sidebar = true;
                                    cx.notify();
                                }),
                            ),
                    )
                    .child(main)
                    .children(assistant),
            )
            .children(toast)
            .children(sheet)
            .children(menu)
            .children(pop)
    }
}

// ---------------------------------------------------------------- helpers

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// "Oct 30 14:12" from a build ID like "2026-10-30T1412-a1b2".
fn chrono_label(id: &str) -> String {
    let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let parse = || -> Option<String> {
        let (date, rest) = id.split_once('T')?;
        let mut d = date.split('-');
        let (_, m, day) = (d.next()?, d.next()?.parse::<usize>().ok()?, d.next()?.parse::<u32>().ok()?);
        let hm = rest.get(..4)?;
        Some(format!("{} {} {}:{}", months.get(m.checked_sub(1)?)?, day, &hm[..2], &hm[2..]))
    };
    parse().unwrap_or_else(|| id.to_string())
}

fn copy_project(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let name = e.file_name();
        if name == "builds" || name == "bases" || name == ".git" {
            std::fs::create_dir_all(dst.join(&name))?;
            continue;
        }
        copy_tree(&e.path(), &dst.join(&name))?;
    }
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    let m = std::fs::symlink_metadata(src)?;
    if m.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
    } else if m.file_type().is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)?;
    } else {
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

fn restore_lock(dir: &Path) -> Result<String, String> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("recipe.lock.json")).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let r: imagesmith_core::recipe::Recipe = serde_json::from_value(v["recipe"].clone()).map_err(|e| e.to_string())?;
    let text = toml_edit::ser::to_string_pretty(&r).map_err(|e| e.to_string())?;
    imagesmith_core::recipe::parse(&text).map_err(|e| e.to_string())?;
    Ok(text)
}

impl Workspace {
    // New Project sheet logic lives here; rendering is in sheets.rs.
    pub fn choose_new_project_base(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Choose Base Image".into()) });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(p) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, cx| this.set_new_project_base(p, cx));
                }
            }
        })
        .detach();
    }

    pub fn choose_new_project_parent(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Choose Folder".into()) });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(p) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, cx| {
                        if let Some(Sheet::NewProject(s)) = &mut this.sheet {
                            s.parent = p;
                        }
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    pub fn set_new_project_base(&mut self, base: PathBuf, cx: &mut Context<Self>) {
        let Some(Sheet::NewProject(s)) = &mut self.sheet else { return };
        if s.name.read(cx).text().is_empty() {
            let stem = base_stem(&base);
            s.name.update(cx, |i, cx| i.set_text(stem, cx));
        }
        s.base = Some(base.clone());
        s.probe = None;
        s.error = None;
        if self.runtime != RuntimeStatus::Ready {
            s.probe = Some(Err("Runtime unavailable: the partition table is detected on first build".into()));
            cx.notify();
            return;
        }
        s.probing = true;
        s.progress = Some("Starting runtime …".into());
        self.touch_store();
        let rt = runtime(cx);
        let (tx, mut rx) = unbounded::<Result<String, Result<Probe, String>>>();
        std::thread::spawn(move || {
            let send = |m| {
                let _ = tx.unbounded_send(m);
            };
            let r = (|| {
                rt.ensure_store(&mut |l| send(Ok(l.to_string())))?;
                let mut last = u64::MAX;
                build::prepare_base(&rt, &base, &mut |u| {
                    let (label, d, t) = match u {
                        Update::Hashing(d, t) => ("Hashing", d, t),
                        Update::Importing(d, t) => ("Importing into store", d, t),
                        Update::Note(n) => {
                            send(Ok(n.to_string()));
                            return;
                        }
                        _ => return,
                    };
                    let pct = if t > 0 { d * 100 / t } else { 0 };
                    if pct != last {
                        last = pct;
                        send(Ok(format!("{label} … {pct}%")));
                    }
                })
            })();
            send(Err(r.map(|(_, p)| p).map_err(|e: Error| e.to_string())));
        });
        cx.spawn(async move |this, cx| {
            while let Some(m) = rx.next().await {
                let done = m.is_err();
                let r = this.update(cx, |this, cx| {
                    if let Some(Sheet::NewProject(s)) = &mut this.sheet {
                        match m {
                            Ok(l) => s.progress = Some(l),
                            Err(r) => {
                                s.probing = false;
                                s.progress = None;
                                s.probe = Some(r);
                            }
                        }
                    }
                    cx.notify();
                });
                if r.is_err() || done {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    pub fn create_new_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Sheet::NewProject(s)) = &mut self.sheet else { return };
        let name = s.name.read(cx).text().trim().to_string();
        let Some(base) = s.base.clone() else {
            s.error = Some("Choose a base image".into());
            cx.notify();
            return;
        };
        if name.is_empty() || name.contains('/') || name.starts_with('.') {
            s.error = Some("Enter a folder name".into());
            cx.notify();
            return;
        }
        let dir = s.parent.join(&name);
        let (mut arch, mut part) = (Arch::Arm64, PartitionRef::Auto);
        if let Some(Ok(p)) = &s.probe {
            if let Some(r) = &p.rootfs {
                part = PartitionRef::Number(r.partition);
                if r.arch.as_deref() == Some("amd64") {
                    arch = Arch::Amd64;
                }
            }
        }
        let stem: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' }).collect();
        let r = std::fs::create_dir_all(&dir)
            .map_err(Error::from)
            .and_then(|_| Project::create(&dir, &stem, &base.to_string_lossy(), arch, part));
        match r {
            Ok(p) => {
                self.sheet = None;
                let mut st = ProjectState::load(p);
                let d = settings(cx).clone();
                let _ = st.edit(|doc| {
                    use crate::edits::{set_str, set_value};
                    let comp = match d.compress {
                        imagesmith_core::recipe::Compress::None => "none",
                        imagesmith_core::recipe::Compress::Zstd => "zstd",
                        imagesmith_core::recipe::Compress::Xz => "xz",
                    };
                    set_str(doc, "output", "compress", comp);
                    let level = d.level.unwrap_or(if comp == "xz" { 6 } else { 19 });
                    set_value(doc, "output", "level", level as i64);
                    set_value(doc, "output", "bmap", d.bmap);
                    set_str(doc, "output", "checksum", if d.checksum { "sha256" } else { "none" });
                    match d.size_mode {
                        imagesmith_core::recipe::SizeMode::Shrink => {}
                        imagesmith_core::recipe::SizeMode::Fixed => {
                            set_str(doc, "size", "mode", "fixed");
                            set_str(doc, "size", "fixed", "8G");
                        }
                        imagesmith_core::recipe::SizeMode::Grow => {
                            set_str(doc, "size", "mode", "grow");
                            set_str(doc, "size", "grow", "2G");
                        }
                    }
                });
                self.projects.push(st);
                let i = self.projects.len() - 1;
                self.tab = Tab::Recipe;
                window.focus(&self.focus);
                self.activate(Some(i), window, cx);
            }
            Err(e) => {
                s.error = Some(e.to_string());
                cx.notify();
            }
        }
    }

    pub fn close_sheet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A flash in flight owns the card; make the user stop it deliberately.
        if matches!(&self.sheet, Some(Sheet::Flash(f)) if f.busy) {
            return;
        }
        self.sheet = None;
        window.focus(&self.focus);
        cx.notify();
    }

    /// Opens the sheet that writes `image` to a removable device.
    pub fn open_flash(&mut self, image: PathBuf, cx: &mut Context<Self>) {
        let source = imagesmith_core::flash::Source::detect(&image).map_err(|e| e.to_string());
        self.sheet = Some(Sheet::Flash(FlashSheet {
            image,
            source,
            devices: Vec::new(),
            selected: None,
            show_all: false,
            verify: true,
            eject: true,
            busy: false,
            stage: None,
            progress: None,
            error: None,
            done: None,
            started: None,
            cancel: None,
        }));
        self.refresh_flash_devices(cx);
    }

    pub fn refresh_flash_devices(&mut self, cx: &mut Context<Self>) {
        let Some(Sheet::Flash(f)) = &self.sheet else { return };
        let show_all = f.show_all;
        let devices = imagesmith_core::flash::list_devices(show_all);
        let Some(Sheet::Flash(f)) = &mut self.sheet else { return };
        match devices {
            Ok(d) => {
                // Keep the selection only while that device is still present.
                if f.selected.as_ref().is_some_and(|s| !d.iter().any(|x| &x.id == s)) {
                    f.selected = None;
                }
                if f.selected.is_none() {
                    f.selected = d.iter().find(|x| !x.is_protected()).map(|x| x.id.clone());
                }
                f.devices = d;
            }
            Err(e) => {
                f.devices.clear();
                f.error = Some(e.to_string());
            }
        }
        cx.notify();
    }

    pub fn toggle_flash_show_all(&mut self, cx: &mut Context<Self>) {
        if let Some(Sheet::Flash(f)) = &mut self.sheet {
            f.show_all = !f.show_all;
        }
        self.refresh_flash_devices(cx);
    }

    pub fn start_flash(&mut self, cx: &mut Context<Self>) {
        let Some(Sheet::Flash(f)) = &mut self.sheet else { return };
        if f.busy || f.blocker().is_some() {
            return;
        }
        let Some(dev) = f.device().map(|d| d.node.clone()) else { return };
        let cancel = CancelToken::default();
        let req = imagesmith_core::flash::Request { image: f.image.clone(), device: dev, verify: f.verify, eject: f.eject, force: false };
        f.busy = true;
        f.error = None;
        f.done = None;
        f.stage = Some("Starting …".into());
        f.progress = None;
        f.started = Some(Instant::now());
        f.cancel = Some(cancel.clone());

        let (tx, mut rx) = unbounded::<FlashMsg>();
        std::thread::spawn(move || {
            let send = |m: FlashMsg| {
                let _ = tx.unbounded_send(m);
            };
            let r = imagesmith_core::flash::flash(&req, imagesmith_core::flash::gui_elevation(), &cancel, &mut |u| match u {
                imagesmith_core::flash::Progress::Stage(t) => send(FlashMsg::Stage(t.to_string())),
                imagesmith_core::flash::Progress::Note(n) => send(FlashMsg::Note(n.to_string())),
                imagesmith_core::flash::Progress::Bytes { phase, done, total } => send(FlashMsg::Progress(phase.to_string(), done, total)),
            });
            send(FlashMsg::Done(r.map_err(|e| e.to_string())));
        });
        cx.spawn(async move |this, cx| {
            while let Some(m) = rx.next().await {
                if this.update(cx, |this, cx| this.apply_flash_msg(m, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn apply_flash_msg(&mut self, m: FlashMsg, cx: &mut Context<Self>) {
        let Some(Sheet::Flash(f)) = &mut self.sheet else { return };
        match m {
            FlashMsg::Stage(t) => {
                f.stage = Some(t);
                f.progress = None;
            }
            FlashMsg::Note(n) => f.stage = Some(n),
            FlashMsg::Progress(phase, done, total) => f.progress = Some((phase, done, total)),
            FlashMsg::Done(r) => {
                f.busy = false;
                f.cancel = None;
                f.stage = None;
                match r {
                    Ok(bytes) => {
                        let secs = f.started.map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.).max(0.001);
                        f.done = Some(format!("Wrote {} in {} ({}/s)", human(bytes), fmt_ms((secs * 1000.) as u64), human((bytes as f64 / secs) as u64)));
                        f.progress = None;
                        self.toast("Image written", cx);
                    }
                    Err(e) => f.error = Some(e),
                }
            }
        }
        cx.notify();
    }

    /// True while a card is being written; quitting then would leave it unusable.
    pub fn is_flashing(&self) -> bool {
        matches!(&self.sheet, Some(Sheet::Flash(f)) if f.busy)
    }

    pub fn cancel_flash(&mut self, cx: &mut Context<Self>) {        if let Some(Sheet::Flash(f)) = &mut self.sheet {
            if let Some(c) = &f.cancel {
                c.cancel();
                f.stage = Some("Cancelling …".into());
            }
        }
        cx.notify();
    }

    /// Ends the shell session; with `capture`, offers to record its changes in the Files tab.
    pub fn close_shell(&mut self, capture: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.sheet, Some(Sheet::Shell { .. })) {
            return;
        }
        let Some(Sheet::Shell { project_id, .. }) = self.sheet.take() else { return };
        window.focus(&self.focus);
        if capture && self.active().is_some_and(|p| p.id() == project_id) {
            self.set_tab(Tab::Files, window, cx);
            if let Some(f) = self.files.get(&project_id) {
                f.update(cx, |f, cx| f.request_capture(cx));
            }
        }
        cx.notify();
    }

    /// Opens the editor for entry `index` of `kind` ("steps" or "host_scripts"), or a new one that runs `when`.
    pub fn edit_step(&mut self, kind: &'static str, index: Option<usize>, when: imagesmith_core::recipe::StepWhen, window: &mut Window, cx: &mut Context<Self>) {
        let step = index.and_then(|i| {
            let r = &self.active()?.recipe.as_ref()?.recipe;
            if kind == "host_scripts" { &r.host_scripts } else { &r.steps }.get(i).cloned()
        });
        let when = step.as_ref().map(|s| s.when).unwrap_or(when);
        let (n, r) = step.map(|s| (s.name, s.run)).unwrap_or_default();
        let name = cx.new(|cx| {
            let mut t = TextInput::new(cx, "Step name");
            t.set_text(n, cx);
            t
        });
        let area = cx.new(|cx| crate::editor::TextArea::new(r, cx));
        let sub = cx.subscribe_in(&area, window, |this: &mut Workspace, _, e: &crate::editor::TextAreaEvent, window, cx| {
            if matches!(e, crate::editor::TextAreaEvent::Save) {
                this.save_step(window, cx);
            }
        });
        window.focus(&area.focus_handle(cx));
        self.sheet = Some(Sheet::Step { kind, index, when, name, area, _sub: sub });
        cx.notify();
    }

    pub fn save_step(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Sheet::Step { kind, index, when, name, area, .. }) = &self.sheet else { return };
        let (kind, index, first) = (*kind, *index, *when == imagesmith_core::recipe::StepWhen::First);
        let name = name.read(cx).text().trim().to_string();
        let mut run = area.read(cx).text().trim_end().to_string();
        if run.trim().is_empty() {
            self.toast("The step has no commands", cx);
            return;
        }
        run.push('\n');
        let name = if name.is_empty() { run.split_whitespace().next().unwrap_or("Step").to_string() } else { name };
        self.edit_active(
            move |d| {
                let run_v = crate::edits::multiline(&run);
                match index.and_then(|i| crate::edits::aot_entry(d, kind, i)) {
                    Some(t) => {
                        t.insert("name", toml_edit::value(name.as_str()));
                        t.insert("run", toml_edit::Item::Value(run_v));
                        if first {
                            t.insert("when", toml_edit::value("first"));
                        } else {
                            t.remove("when");
                        }
                    }
                    None => {
                        let mut pairs = vec![("name", name.as_str().into()), ("run", run_v)];
                        if first {
                            pairs.push(("when", "first".into()));
                        }
                        crate::edits::aot_push(d, kind, crate::edits::table_from(&pairs))
                    }
                }
            },
            cx,
        );
        self.close_sheet(window, cx);
    }

}

pub fn base_stem(p: &Path) -> String {
    let n = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut s = n.as_str();
    for suf in [".xz", ".zst", ".gz", ".7z"] {
        s = s.strip_suffix(suf).unwrap_or(s);
    }
    for suf in [".img", ".raw", ".bin"] {
        s = s.strip_suffix(suf).unwrap_or(s);
    }
    s.to_string()
}
