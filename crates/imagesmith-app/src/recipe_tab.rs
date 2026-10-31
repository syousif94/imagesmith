//! Recipe tab: one grouped section per recipe table, in execution order, or
//! the raw TOML. Every edit goes through toml_edit so hand-written comments survive.

use crate::edits;
use crate::editor::{TextArea, TextAreaEvent};
use crate::text_input::{InputEvent, TextInput};
use crate::theme::{Colors, MONO};
use crate::widgets::{button, group, segmented, setting_row, stepper, switch, token, tooltip, ClickIf, Kind};
use crate::workspace::{ProjectState, Workspace};
use gpui::prelude::*;
use gpui::{div, px, AnyElement, Context, Entity, ExternalPaths, Focusable, PathPromptOptions, ScrollHandle, SharedString, Subscription, Task, Window};
use imagesmith_core::recipe::{Arch, Checksum, Compress, PartitionRef, PasswordMode, SizeMode, StepWhen};
use imagesmith_core::size::human;
use imagesmith_core::validate::{resolve_project_path, Diagnostic, Severity};
use std::path::Path;
use std::time::{Duration, Instant};
use toml_edit::{DocumentMut, Value};

pub const CARDS: [(&str, &str); 9] = [
    ("image", "Base Image"),
    ("size", "Disk Size"),
    ("packages", "Software"),
    ("users", "Users"),
    ("system", "System"),
    ("files", "Files"),
    ("services", "Services"),
    ("steps", "Custom Steps"),
    ("output", "Export"),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    Name,
    Base,
    Sha,
    Headroom,
    Fixed,
    Grow,
    Hostname,
    Timezone,
    Locale,
}

impl Field {
    const ALL: [Field; 9] =
        [Field::Name, Field::Base, Field::Sha, Field::Headroom, Field::Fixed, Field::Grow, Field::Hostname, Field::Timezone, Field::Locale];

    fn key(self) -> (&'static str, &'static str) {
        match self {
            Field::Name => ("image", "name"),
            Field::Base => ("image", "base"),
            Field::Sha => ("image", "base_sha256"),
            Field::Headroom => ("size", "headroom"),
            Field::Fixed => ("size", "fixed"),
            Field::Grow => ("size", "grow"),
            Field::Hostname => ("system", "hostname"),
            Field::Timezone => ("system", "timezone"),
            Field::Locale => ("system", "locale"),
        }
    }

    fn current(self, p: &ProjectState) -> String {
        let Some(rf) = &p.recipe else { return String::new() };
        let r = &rf.recipe;
        match self {
            Field::Name => r.image.name.clone(),
            Field::Base => r.image.base.clone(),
            Field::Sha => r.image.base_sha256.clone().unwrap_or_default(),
            Field::Headroom => r.size.headroom.clone(),
            Field::Fixed => r.size.fixed.clone().unwrap_or_default(),
            Field::Grow => r.size.grow.clone().unwrap_or_default(),
            Field::Hostname => r.system.hostname.clone().unwrap_or_default(),
            Field::Timezone => r.system.timezone.clone().unwrap_or_default(),
            Field::Locale => r.system.locale.clone().unwrap_or_default(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Adder {
    Install,
    Remove,
    Enable,
    Disable,
    User,
}

pub struct Inputs {
    fields: Vec<(Field, Entity<TextInput>)>,
    adders: Vec<(Adder, Entity<TextInput>)>,
    file_dst: Entity<TextInput>,
    _subs: Vec<Subscription>,
}

impl Inputs {
    fn field(&self, f: Field) -> Entity<TextInput> {
        self.fields.iter().find(|(x, _)| *x == f).unwrap().1.clone()
    }
    fn adder(&self, a: Adder) -> Entity<TextInput> {
        self.adders.iter().find(|(x, _)| *x == a).unwrap().1.clone()
    }
}

pub struct RecipeUi {
    pub inputs: Option<Inputs>,
    pub project_id: Option<String>,
    pub toml_view: bool,
    pub toml_area: Option<(String, Entity<TextArea>, Subscription)>,
    pub diag_open: bool,
    pub scroll: ScrollHandle,
    pub scroll_to: Option<&'static str>,
    pub highlight: Option<(&'static str, Instant)>,
    pub dirty: bool,
    pub pending_activate: bool,
    pub group_inputs: Vec<(Entity<TextInput>, Subscription)>,
    pub pending: Option<Task<()>>,
    pub hashing: Option<String>,
}

impl Default for RecipeUi {
    fn default() -> Self {
        RecipeUi {
            inputs: None,
            project_id: None,
            toml_view: false,
            toml_area: None,
            diag_open: false,
            scroll: ScrollHandle::new(),
            scroll_to: None,
            highlight: None,
            dirty: false,
            pending_activate: false,
            group_inputs: Vec::new(),
            pending: None,
            hashing: None,
        }
    }
}

fn card_of(field: &str) -> &'static str {
    let head = field.split(['.', '[']).next().unwrap_or("");
    if head == "repos" {
        return "packages";
    }
    CARDS.iter().find(|(id, _)| *id == head).map(|(id, _)| *id).unwrap_or("image")
}

impl RecipeUi {
    /// Creates inputs on first use and refreshes their text from the recipe
    /// (except a field the user is typing in).
    pub fn sync(&mut self, p: &ProjectState, window: &mut Window, cx: &mut Context<Workspace>) {
        if self.inputs.is_none() {
            let mut subs = Vec::new();
            let mut fields = Vec::new();
            for f in Field::ALL {
                let placeholder = match f {
                    Field::Name => "my-image",
                    Field::Base => "bases/image.img.xz",
                    Field::Sha => "Optional",
                    Field::Headroom => "15%",
                    Field::Fixed => "8G",
                    Field::Grow => "2G",
                    Field::Hostname => "raspberrypi",
                    Field::Timezone => "America/Los_Angeles",
                    Field::Locale => "en_US.UTF-8",
                };
                let mono = matches!(f, Field::Sha | Field::Base);
                let e = cx.new(|cx| {
                    let t = TextInput::new(cx, placeholder);
                    if mono {
                        t.mono()
                    } else {
                        t
                    }
                });
                subs.push(cx.subscribe(&e, |this: &mut Workspace, _, ev: &InputEvent, cx| match ev {
                    InputEvent::Changed => this.schedule_commit(cx),
                    InputEvent::Submit => this.commit_fields(cx),
                }));
                fields.push((f, e));
            }
            let mut adders = Vec::new();
            for (a, ph) in [
                (Adder::Install, "Add package…"),
                (Adder::Remove, "Add package…"),
                (Adder::Enable, "Add unit…"),
                (Adder::Disable, "Add unit…"),
                (Adder::User, "New user name"),
            ] {
                let e = cx.new(|cx| TextInput::new(cx, ph));
                subs.push(cx.subscribe(&e, move |this: &mut Workspace, input, ev: &InputEvent, cx| {
                    if matches!(ev, InputEvent::Submit) {
                        let v = input.read(cx).text().trim().to_string();
                        if !v.is_empty() {
                            this.add_token(a, &v, cx);
                            input.update(cx, |i, cx| i.set_text("", cx));
                        }
                    }
                }));
                adders.push((a, e));
            }
            let file_dst = cx.new(|cx| TextInput::new(cx, "/opt/").mono());
            self.inputs = Some(Inputs { fields, adders, file_dst, _subs: subs });
        }
        let switched = self.project_id.as_deref() != Some(p.id());
        self.project_id = Some(p.id().to_string());
        let inputs = self.inputs.as_ref().unwrap();
        for (f, e) in &inputs.fields {
            if switched || !e.read(cx).is_focused(window) {
                let v = f.current(p);
                e.update(cx, |i, cx| i.set_text(v, cx));
            }
        }
        if switched {
            for (_, e) in &inputs.adders {
                e.update(cx, |i, cx| i.set_text("", cx));
            }
        }
        let users = p.recipe.as_ref().map(|r| r.recipe.users.len()).unwrap_or(0);
        while self.group_inputs.len() < users {
            let i = self.group_inputs.len();
            let e = cx.new(|cx| TextInput::new(cx, "Add group…"));
            let sub = cx.subscribe(&e, move |this: &mut Workspace, input, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Submit) {
                    let v = input.read(cx).text().trim().to_string();
                    if !v.is_empty() {
                        this.edit_active(
                            move |doc| {
                                if let Some(t) = edits::aot_entry(doc, "users", i) {
                                    let a = t.entry("groups").or_insert(toml_edit::value(toml_edit::Array::new()));
                                    if let Some(a) = a.as_array_mut() {
                                        if !a.iter().any(|x| x.as_str() == Some(v.as_str())) {
                                            a.push(v.as_str());
                                        }
                                    }
                                }
                            },
                            cx,
                        );
                        input.update(cx, |i, cx| i.set_text("", cx));
                    }
                }
            });
            self.group_inputs.push((e, sub));
        }
        self.mark_invalid(&p.diagnostics, cx);
    }

    pub fn mark_invalid(&self, diags: &[Diagnostic], cx: &mut Context<Workspace>) {
        let Some(inputs) = &self.inputs else { return };
        for (f, e) in &inputs.fields {
            let (s, k) = f.key();
            let name = format!("{s}.{k}");
            let bad = diags.iter().any(|d| d.severity == Severity::Error && d.field == name);
            e.update(cx, |i, cx| {
                if i.invalid != bad {
                    i.invalid = bad;
                    cx.notify();
                }
            });
        }
    }
}

impl Workspace {
    pub fn edit_active(&mut self, f: impl FnOnce(&mut DocumentMut), cx: &mut Context<Self>) {
        let Some(i) = self.active else { return };
        let r = self.projects[i].edit(f);
        if let Err(e) = r {
            self.toast(format!("Edit not applied: {e}"), cx);
        }
        self.recipe_ui.mark_invalid(&self.projects[i].diagnostics, cx);
        cx.notify();
    }

    fn schedule_commit(&mut self, cx: &mut Context<Self>) {
        self.recipe_ui.pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(300)).await;
            let _ = this.update(cx, |this, cx| this.commit_fields(cx));
        }));
    }

    /// Writes every text field whose content differs from the recipe.
    pub fn commit_fields(&mut self, cx: &mut Context<Self>) {
        self.recipe_ui.pending = None;
        let (Some(i), Some(inputs)) = (self.active, &self.recipe_ui.inputs) else { return };
        if self.recipe_ui.project_id.as_deref() != Some(self.projects[i].id()) {
            return;
        }
        let p = &self.projects[i];
        let changes: Vec<(Field, String)> = inputs
            .fields
            .iter()
            .map(|(f, e)| (*f, e.read(cx).text()))
            .filter(|(f, v)| f.current(p) != *v)
            .collect();
        if changes.is_empty() {
            return;
        }
        if changes.iter().any(|(f, v)| matches!(f, Field::Name | Field::Base | Field::Headroom) && v.trim().is_empty()) {
            return;
        }
        self.edit_active(
            move |doc| {
                for (f, v) in &changes {
                    let (s, k) = f.key();
                    edits::set_str(doc, s, k, v.trim());
                }
            },
            cx,
        );
    }

    fn add_token(&mut self, a: Adder, v: &str, cx: &mut Context<Self>) {
        let v = v.to_string();
        match a {
            Adder::Install => self.edit_active(move |d| edits::push_str(d, "packages", "install", &v), cx),
            Adder::Remove => self.edit_active(move |d| edits::push_str(d, "packages", "remove", &v), cx),
            Adder::Enable => self.edit_active(move |d| edits::push_str(d, "services", "enable", &v), cx),
            Adder::Disable => self.edit_active(move |d| edits::push_str(d, "services", "disable", &v), cx),
            Adder::User => self.edit_active(
                move |d| {
                    edits::aot_push(
                        d,
                        "users",
                        edits::table_from(&[
                            ("name", Value::from(v.as_str())),
                            ("groups", edits::str_array(&["sudo"])),
                            ("ssh_keys", edits::str_array(&[])),
                            ("password", Value::from("locked")),
                        ]),
                    )
                },
                cx,
            ),
        }
        self.recipe_ui.dirty = true;
    }

    fn rel_to_project(&self, path: &Path) -> String {
        let Some(p) = self.active() else { return path.to_string_lossy().into_owned() };
        match path.strip_prefix(&p.project.path) {
            Ok(rel) => rel.to_string_lossy().into_owned(),
            Err(_) => collapse_home(path),
        }
    }

    fn choose_base(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Choose Base Image".into()) });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(p) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, cx| this.set_base(&p, cx));
                }
            }
        })
        .detach();
    }

    fn set_base(&mut self, p: &Path, cx: &mut Context<Self>) {
        let rel = self.rel_to_project(p);
        self.edit_active(move |d| edits::set_str(d, "image", "base", &rel), cx);
        self.recipe_ui.dirty = true;
    }

    fn compute_checksum(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.active() else { return };
        let Some(rf) = &p.recipe else { return };
        let base = resolve_project_path(&p.project.path, &rf.recipe.image.base);
        self.recipe_ui.hashing = Some("Hashing …".into());
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<Result<u64, Result<String, String>>>();
        std::thread::spawn(move || {
            let mut last = u64::MAX;
            let r = imagesmith_core::hashing::sha256_cached(&base, |d, t| {
                let pct = if t > 0 { d * 100 / t } else { 0 };
                if pct != last {
                    last = pct;
                    let _ = tx.unbounded_send(Ok(pct));
                }
            });
            let _ = tx.unbounded_send(Err(r.map_err(|e| e.to_string())));
        });
        cx.spawn(async move |this, cx| {
            use futures::StreamExt;
            while let Some(m) = rx.next().await {
                let done = m.is_err();
                let _ = this.update(cx, |this, cx| {
                    match m {
                        Ok(pct) => this.recipe_ui.hashing = Some(format!("Hashing … {pct}%")),
                        Err(Ok(sha)) => {
                            this.recipe_ui.hashing = None;
                            this.edit_active(move |d| edits::set_str(d, "image", "base_sha256", &sha), cx);
                            this.recipe_ui.dirty = true;
                        }
                        Err(Err(e)) => {
                            this.recipe_ui.hashing = None;
                            this.toast(format!("Hashing failed: {e}"), cx);
                        }
                    }
                    cx.notify();
                });
                if done {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn add_files(&mut self, paths: Vec<std::path::PathBuf>, cx: &mut Context<Self>) {
        let Some(p) = self.active() else { return };
        let root = p.project.path.clone();
        let dst_input = self.recipe_ui.inputs.as_ref().map(|i| i.file_dst.read(cx).text()).unwrap_or_default();
        let mut entries = Vec::new();
        for src in paths.into_iter().filter(|p| p.is_file()) {
            let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let rel = match src.strip_prefix(&root) {
                Ok(r) => r.to_string_lossy().into_owned(),
                Err(_) => {
                    let dest = root.join("files").join(&name);
                    if let Err(e) = std::fs::create_dir_all(root.join("files")).and_then(|_| std::fs::copy(&src, &dest)) {
                        self.toast(format!("Copy failed: {e}"), cx);
                        return;
                    }
                    format!("files/{name}")
                }
            };
            let dst = if dst_input.starts_with('/') {
                if dst_input.ends_with('/') {
                    format!("{dst_input}{name}")
                } else {
                    dst_input.clone()
                }
            } else {
                format!("/opt/{name}")
            };
            let mode = if is_executable(&src) { "0755" } else { "0644" };
            entries.push((rel, dst, mode));
        }
        if entries.is_empty() {
            return;
        }
        self.edit_active(
            move |d| {
                for (src, dst, mode) in &entries {
                    edits::aot_push(
                        d,
                        "files",
                        edits::table_from(&[
                            ("src", src.as_str().into()),
                            ("dst", dst.as_str().into()),
                            ("mode", (*mode).into()),
                            ("owner", "root:root".into()),
                        ]),
                    );
                }
            },
            cx,
        );
        if let Some(i) = &self.recipe_ui.inputs {
            i.file_dst.update(cx, |i, cx| i.set_text("", cx));
        }
    }

    /// Opens the repository sheet for repo `index`, or a new one.
    pub fn open_repo_sheet(&mut self, index: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let repo = index.and_then(|i| self.active()?.recipe.as_ref()?.recipe.repos.get(i).cloned());
        let input = |ph: &str, v: String, mono: bool, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut t = TextInput::new(cx, ph.to_string());
                t.set_text(v, cx);
                if mono {
                    t.mono()
                } else {
                    t
                }
            })
        };
        let name = input("docker", repo.as_ref().map(|r| r.name.clone()).unwrap_or_default(), false, cx);
        let url = input("https://download.docker.com/linux/debian", repo.as_ref().map(|r| r.url.clone()).unwrap_or_default(), true, cx);
        let suite = input("The image's release, e.g. bookworm", repo.as_ref().and_then(|r| r.suite.clone()).unwrap_or_default(), false, cx);
        let components = input("main", repo.as_ref().map(|r| r.components.join(" ")).unwrap_or_else(|| "main".into()), false, cx);
        let key = input("https://…/gpg or a file", repo.as_ref().and_then(|r| r.key.clone()).unwrap_or_default(), true, cx);
        window.focus(&name.focus_handle(cx));
        self.sheet = Some(crate::workspace::Sheet::Repo(crate::workspace::RepoSheet { index, name, url, suite, components, key, error: None }));
        cx.notify();
    }

    pub fn choose_repo_key(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Choose Signing Key".into()) });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(p) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, cx| {
                        let rel = this.rel_to_project(&p);
                        if let Some(crate::workspace::Sheet::Repo(s)) = &this.sheet {
                            s.key.update(cx, |i, cx| i.set_text(rel, cx));
                        }
                    });
                }
            }
        })
        .detach();
    }

    pub fn save_repo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(crate::workspace::Sheet::Repo(s)) = &mut self.sheet else { return };
        let text = |e: &Entity<TextInput>| e.read(cx).text().trim().to_string();
        let (name, url, suite, comps, key) = (text(&s.name), text(&s.url), text(&s.suite), text(&s.components), text(&s.key));
        let index = s.index;
        if name.is_empty() || url.is_empty() {
            s.error = Some("Enter a name and a URL".into());
            cx.notify();
            return;
        }
        let taken = self.active().and_then(|p| p.recipe.as_ref()).is_some_and(|r| r.recipe.repos.iter().enumerate().any(|(i, x)| x.name == name && Some(i) != index));
        if taken {
            if let Some(crate::workspace::Sheet::Repo(s)) = &mut self.sheet {
                s.error = Some(format!("A repository named {name} already exists"));
            }
            cx.notify();
            return;
        }
        let comps: Vec<&str> = comps.split([' ', ',']).filter(|c| !c.is_empty()).collect();
        let mut pairs: Vec<(&str, Value)> = vec![("name", name.as_str().into()), ("url", url.as_str().into())];
        if !suite.is_empty() {
            pairs.push(("suite", suite.as_str().into()));
        }
        pairs.push(("components", edits::str_array(&comps)));
        if !key.is_empty() {
            pairs.push(("key", key.as_str().into()));
        }
        let table = edits::table_from(&pairs);
        self.edit_active(
            move |d| match index.and_then(|i| edits::aot_entry(d, "repos", i)) {
                Some(t) => {
                    for k in ["suite", "key"] {
                        t.remove(k);
                    }
                    for (k, v) in table.iter() {
                        t.insert(k, v.clone());
                    }
                }
                None => edits::aot_push(d, "repos", table),
            },
            cx,
        );
        self.recipe_ui.dirty = true;
        self.close_sheet(window, cx);
    }

    fn add_ssh_key(&mut self, user: usize, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Add Public Key".into()) });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                let _ = this.update(cx, |this, cx| {
                    let keys: Vec<String> = paths.iter().map(|p| this.rel_to_project(p)).collect();
                    this.edit_active(
                        move |d| {
                            if let Some(t) = edits::aot_entry(d, "users", user) {
                                let a = t.entry("ssh_keys").or_insert(toml_edit::value(toml_edit::Array::new()));
                                if let Some(a) = a.as_array_mut() {
                                    for k in &keys {
                                        if !a.iter().any(|x| x.as_str() == Some(k)) {
                                            a.push(k.as_str());
                                        }
                                    }
                                }
                            }
                        },
                        cx,
                    );
                });
            }
        })
        .detach();
    }

    // ------------------------------------------------------------ render

    /// A titled section: heading, one-line explanation, then its groups and any problems.
    fn section(&self, id: &'static str, subtitle: &'static str, body: Vec<AnyElement>, c: &Colors) -> AnyElement {
        let title = CARDS.iter().find(|(i, _)| *i == id).map(|(_, t)| *t).unwrap_or(id);
        let diags: Vec<&Diagnostic> = self.active().map(|p| p.diagnostics.iter().filter(|d| card_of(&d.field) == id).collect()).unwrap_or_default();
        let hl = self.recipe_ui.highlight.is_some_and(|(h, t)| h == id && t.elapsed() < Duration::from_millis(1500));
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .w_full()
            .max_w(px(760.))
            .p(px(6.))
            .rounded(px(14.))
            .when(hl, |d| d.bg(c.accent_soft))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .px(px(6.))
                    .child(div().text_size(px(16.)).font_weight(gpui::FontWeight::SEMIBOLD).child(title))
                    .child(div().text_size(px(12.5)).text_color(c.muted).child(subtitle)),
            )
            .children(body)
            .children(diags.iter().map(|x| {
                let err = x.severity == Severity::Error;
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(6.))
                    .text_size(px(12.5))
                    .child(div().flex_none().size(px(7.)).rounded_full().bg(if err { c.danger } else { c.warning }))
                    .child(div().text_color(if err { c.danger } else { c.text }).child(x.message.clone()))
            }))
            .into_any_element()
    }

    fn ensure_toml_area(&mut self, pi: usize, cx: &mut Context<Self>) {
        let p = &self.projects[pi];
        let stale = match &self.recipe_ui.toml_area {
            Some((id, a, _)) => {
                let a = a.read(cx);
                id != p.id() || (!a.dirty && a.text() != p.text)
            }
            None => true,
        };
        if stale {
            let (id, text) = (p.id().to_string(), p.text.clone());
            let area = cx.new(|cx| TextArea::new(text, cx));
            let sub = cx.subscribe(&area, |this: &mut Workspace, _, e: &TextAreaEvent, cx| match e {
                TextAreaEvent::Save => this.save_toml(cx),
                TextAreaEvent::Changed => cx.notify(),
            });
            self.recipe_ui.toml_area = Some((id, area, sub));
        }
    }

    pub fn save_toml(&mut self, cx: &mut Context<Self>) {
        let Some(i) = self.active else { return };
        let Some((id, area, _)) = &self.recipe_ui.toml_area else { return };
        if id != self.projects[i].id() {
            return;
        }
        let area = area.clone();
        let text = area.read(cx).text().to_string();
        let p = &mut self.projects[i];
        if let Err(e) = std::fs::write(p.project.recipe_path(), &text) {
            self.toast(format!("Could not save recipe.toml: {e}"), cx);
            return;
        }
        p.reload();
        area.update(cx, |a, cx| {
            a.dirty = false;
            cx.notify();
        });
        self.recipe_ui.dirty = true;
        cx.notify();
    }

    /// The active recipe's parse error or validation problems as plain text.
    pub fn recipe_problem_report(&self) -> Option<String> {
        let p = self.active()?;
        if let Some((line, m)) = &p.parse_error {
            return Some(format!("recipe.toml does not parse: line {line}: {m}\n"));
        }
        if p.diagnostics.is_empty() {
            return None;
        }
        let mut s = String::from("The recipe has these problems:\n");
        for d in &p.diagnostics {
            let sev = if d.severity == Severity::Error { "error" } else { "warning" };
            s.push_str(&format!("- {sev} {}: {}", d.field, d.message));
            if let Some(h) = &d.hint {
                s.push_str(&format!(" ({h})"));
            }
            s.push('\n');
        }
        Some(s)
    }

    pub fn render_recipe_tab(&mut self, c: &Colors, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(pi) = self.active else { return div().into_any_element() };
        if self.recipe_ui.inputs.is_none() {
            let (ui, p) = (&mut self.recipe_ui, &self.projects[pi]);
            ui.sync(p, window, cx);
        }
        if let Some(target) = self.recipe_ui.scroll_to.take() {
            if let Some(ix) = CARDS.iter().position(|(id, _)| *id == target) {
                self.recipe_ui.scroll.scroll_to_top_of_item(ix);
                self.recipe_ui.highlight = Some((target, Instant::now()));
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(Duration::from_millis(1600)).await;
                    let _ = this.update(cx, |_, cx| cx.notify());
                })
                .detach();
            }
        }
        let parse_error = self.projects[pi].parse_error.clone();
        let toml = self.recipe_ui.toml_view || parse_error.is_some();
        if toml {
            self.ensure_toml_area(pi, cx);
        }
        let toml_dirty = toml && self.recipe_ui.toml_area.as_ref().is_some_and(|(_, a, _)| a.read(cx).dirty);

        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .px(px(24.))
            .py(px(12.))
            .child(
                div().id("view-toggle").tooltip(tooltip("Switch between the form and the recipe.toml source (⌘⇧T)")).child(segmented(
                    "view",
                    &["Form", "TOML"],
                    if toml { 1 } else { 0 },
                    c,
                    {
                        let ws = cx.entity().downgrade();
                        move |i, _, cx| {
                            let _ = ws.update(cx, |this, cx| {
                                this.recipe_ui.toml_view = i == 1;
                                cx.notify();
                            });
                        }
                    },
                )),
            )
            .child(div().flex_1())
            .when(toml, |d| {
                d.child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(px(12.))
                        .text_color(c.muted)
                        .child(collapse_home(&self.projects[pi].project.recipe_path())),
                )
                .when(toml_dirty, |d| d.child(div().text_size(px(12.)).text_color(c.warning).child("Edited")))
                .child(button("toml-revert", "Revert", Kind::Normal, toml_dirty, c).tooltip(tooltip("Discard changes since the last save")).on_click_if(
                    toml_dirty,
                    cx.listener(|this, _, _, cx| {
                        this.recipe_ui.toml_area = None;
                        cx.notify();
                    }),
                ))
                .child(
                    button("toml-save", "Save", Kind::Primary, toml_dirty, c)
                        .tooltip(tooltip("Save recipe.toml (⌘S)"))
                        .on_click_if(toml_dirty, cx.listener(|this, _, _, cx| this.save_toml(cx))),
                )
            });

        let body: AnyElement = if toml {
            let area = self.recipe_ui.toml_area.as_ref().map(|(_, a, _)| a.clone());
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .gap(px(8.))
                .mx(px(24.))
                .mb(px(12.))
                .children(parse_error.map(|(l, m)| {
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .px(px(12.))
                        .py(px(8.))
                        .rounded(px(8.))
                        .bg(c.danger_soft)
                        .text_color(c.danger)
                        .text_size(px(12.5))
                        .child(div().flex_1().child(format!("Line {l}: {m}. Fix it here and save; the form returns once the recipe is valid.")))
                        .child(button("parse-fix", "Fix with Assistant", Kind::Normal, true, c).on_click(cx.listener(|this, _, w, cx| {
                            if let Some(r) = this.recipe_problem_report() {
                                this.fix_with_assistant(r, w, cx);
                            }
                        })))
                }))
                .child(div().flex_1().min_h_0().rounded(px(8.)).overflow_hidden().border_1().border_color(c.border).children(area))
                .into_any_element()
        } else {
            let cards = self.render_cards(pi, c, cx);
            div()
                .id("recipe-cards")
                .flex()
                .flex_col()
                .items_center()
                .gap(px(22.))
                .flex_1()
                .min_h_0()
                .px(px(18.))
                .pb(px(32.))
                .overflow_y_scroll()
                .track_scroll(&self.recipe_ui.scroll)
                .children(cards)
                .into_any_element()
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(c.page)
            .child(header)
            .child(body)
            .child(self.render_diag_bar(pi, c, cx))
            .into_any_element()
    }

    fn render_diag_bar(&mut self, pi: usize, c: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let p = &self.projects[pi];
        let errors = p.diagnostics.iter().filter(|d| d.severity == Severity::Error).count();
        let warnings = p.diagnostics.len() - errors;
        let first = p.diagnostics.first().map(|d| d.message.clone());
        let open = self.recipe_ui.diag_open && !p.diagnostics.is_empty();
        let mut list = div().flex().flex_col().gap(px(2.)).px(px(16.)).pb(px(8.));
        if open {
            for (i, d) in p.diagnostics.iter().enumerate() {
                let card = card_of(&d.field);
                let hover = c.hover;
                list = list.child(
                    div()
                        .id(("diag", i))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(6.))
                        .py(px(3.))
                        .rounded(px(4.))
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .child(div().text_color(if d.severity == Severity::Error { c.danger } else { c.warning }).child(if d.severity == Severity::Error { "✗" } else { "▲" }))
                        .child(div().text_color(c.muted).text_size(px(12.)).child(d.field.clone()))
                        .child(div().flex_1().text_size(px(12.5)).child(d.message.clone()))
                        .when_some(d.hint.clone(), |e, h| e.child(div().text_size(px(12.)).text_color(c.muted).child(h)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.recipe_ui.toml_view = false;
                            this.recipe_ui.scroll_to = Some(card);
                            cx.notify();
                        })),
                );
            }
        }
        let summary = match (errors, warnings) {
            (0, 0) => "No problems".to_string(),
            _ => format!(
                "{errors} error{} · {warnings} warning{}{}",
                if errors == 1 { "" } else { "s" },
                if warnings == 1 { "" } else { "s" },
                first.map(|m| format!(": {m}")).unwrap_or_default()
            ),
        };
        div()
            .flex()
            .flex_col()
            .flex_none()
            .border_t_1()
            .border_color(c.border)
            .bg(c.sidebar)
            .child(
                div()
                    .id("diag-bar")
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(16.))
                    .h(px(34.))
                    .cursor_pointer()
                    .child(div().text_color(if errors > 0 { c.danger } else if warnings > 0 { c.warning } else { c.success }).child(if errors > 0 {
                        "✗"
                    } else if warnings > 0 {
                        "▲"
                    } else {
                        "✓"
                    }))
                    .child(div().flex_1().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.5)).child(summary))
                    .when(!p.diagnostics.is_empty(), |d| {
                        d.child(button("diag-copy", "Copy", Kind::Ghost, true, c).h(px(22.)).tooltip(tooltip("Copy every problem")).on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            if let Some(r) = this.recipe_problem_report() {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(r));
                                this.toast("Problems copied", cx);
                            }
                        })))
                        .child(button("diag-fix", "Fix with Assistant", Kind::Normal, true, c).h(px(22.)).tooltip(tooltip("Start a new assistant chat about these problems")).on_click(
                            cx.listener(|this, _, w, cx| {
                                cx.stop_propagation();
                                if let Some(r) = this.recipe_problem_report() {
                                    this.fix_with_assistant(r, w, cx);
                                }
                            }),
                        ))
                    })
                    .when(!p.diagnostics.is_empty(), |d| d.child(div().text_color(c.muted).text_size(px(11.)).child(if open { "▾" } else { "▴" })))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.recipe_ui.diag_open = !this.recipe_ui.diag_open;
                        cx.notify();
                    })),
            )
            .when(open, |d| d.child(div().id("diag-list").max_h(px(200.)).overflow_y_scroll().child(list)))
            .into_any_element()
    }

    fn render_cards(&mut self, pi: usize, c: &Colors, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let p = &self.projects[pi];
        let Some(rf) = &p.recipe else { return Vec::new() };
        let r = rf.recipe.clone();
        let probe = p.probe.clone();
        let root = p.project.path.clone();
        let inputs = self.recipe_ui.inputs.as_ref().unwrap();
        let fi = |f: Field| inputs.field(f);
        let ad = |a: Adder| inputs.adder(a);
        let file_dst = inputs.file_dst.clone();
        let groups: Vec<Entity<TextInput>> = self.recipe_ui.group_inputs.iter().map(|(e, _)| e.clone()).collect();
        let hashing = self.recipe_ui.hashing.clone();
        let mut out = Vec::new();
        let c2 = *c;

        // ---------------------------------------------------------- Image
        let resolved = probe.as_ref().and_then(|pr| pr.resolve(r.image.rootfs_partition)).map(|x| x.number);
        let auto = r.image.rootfs_partition == PartitionRef::Auto;
        let has_probe = probe.is_some();
        let info: SharedString = probe
            .as_ref()
            .map(|p| p.summary())
            .unwrap_or_else(|| "Not inspected yet. Click Validate to read its partitions.".into())
            .into();
        let image_rows: Vec<AnyElement> = vec![
            setting_row("Name", Some("Used to name the finished image file.".into()), c).child(div().w(px(240.)).child(fi(Field::Name))).into_any_element(),
            setting_row("Base image", Some(info), c)
                .child(div().w(px(240.)).child(fi(Field::Base)))
                .child(button("choose-base", "Choose…", Kind::Normal, true, c).on_click(cx.listener(|this, _, _, cx| this.choose_base(cx))))
                .into_any_element(),
            setting_row("Architecture", Some("Must match the base image.".into()), c)
                .child(segmented("arch", &["ARM64", "x86-64"], if r.image.arch == Arch::Amd64 { 1 } else { 0 }, c, {
                    let ws = cx.entity().downgrade();
                    move |i, _, cx| {
                        let _ = ws.update(cx, |this, cx| this.edit_active(move |d| edits::set_str(d, "image", "arch", ["arm64", "amd64"][i]), cx));
                    }
                }))
                .into_any_element(),
            setting_row("Find the system partition automatically", Some("Uses the last ext4 partition on the disk.".into()), c)
                .when_some(
                    match r.image.rootfs_partition {
                        PartitionRef::Number(n) if !has_probe => Some(n),
                        _ => None,
                    },
                    |d, n| d.child(div().text_size(px(12.5)).text_color(c.muted).child(format!("Partition {n}"))),
                )
                .child(switch("auto-part", auto, c).on_click(cx.listener(move |this, _, _, cx| {
                    let v: Value = if auto {
                        this.active()
                            .and_then(|p| p.probe.as_ref())
                            .and_then(|pr| pr.resolve(PartitionRef::Auto))
                            .map(|x| Value::from(x.number as i64))
                            .unwrap_or_else(|| Value::from(2i64))
                    } else {
                        Value::from("auto")
                    };
                    this.edit_active(move |d| edits::set_value(d, "image", "rootfs_partition", v), cx);
                })))
                .into_any_element(),
            setting_row("Checksum", Some("Optional SHA-256 that the base image must match.".into()), c)
                .child(div().w(px(240.)).child(fi(Field::Sha)))
                .child(
                    button("compute-sha", hashing.clone().unwrap_or_else(|| "Compute".into()), Kind::Normal, hashing.is_none(), c)
                        .tooltip(tooltip("Hash the current base image and fill this in"))
                        .on_click_if(hashing.is_none(), cx.listener(|this, _, _, cx| this.compute_checksum(cx))),
                )
                .into_any_element(),
        ];
        let mut image_body = vec![group(image_rows, c).into_any_element()];
        if let Some(pr) = &probe {
            let mut rows = Vec::new();
            for (n, part) in pr.partitions.iter().enumerate() {
                let is_root = Some(part.number) == resolved;
                let num = part.number;
                let hover = c.hover;
                let label = part.label.clone().map(|l| format!(" · {l}")).unwrap_or_default();
                rows.push(
                    div()
                        .id(("part", n))
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .px(px(14.))
                        .py(px(7.))
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .child(div().text_size(px(13.)).child(format!("Partition {num}{label}")))
                                .child(div().text_size(px(11.5)).text_color(c.muted).child(format!(
                                    "{} · {}",
                                    part.fstype.clone().unwrap_or_else(|| "unknown".into()),
                                    human(part.size_bytes)
                                ))),
                        )
                        .when(is_root, |d| {
                            d.child(div().text_size(px(12.)).text_color(c.muted).child("System"))
                                .child(div().text_color(c.accent).font_weight(gpui::FontWeight::BOLD).child("✓"))
                        })
                        .tooltip(tooltip(if is_root { "The Linux system is installed here" } else { "Use this partition as the system partition" }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.edit_active(move |d| edits::set_value(d, "image", "rootfs_partition", num as i64), cx);
                        }))
                        .into_any_element(),
                );
            }
            image_body.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(div().px(px(14.)).text_size(px(11.5)).text_color(c.muted).child("Partitions"))
                    .child(group(rows, c))
                    .into_any_element(),
            );
        }
        out.push(self.section("image", "The disk image everything starts from.", image_body, c));

        // ---------------------------------------------------------- Size
        let mode_ix = match r.size.mode {
            SizeMode::Shrink => 0,
            SizeMode::Fixed => 1,
            SizeMode::Grow => 2,
        };
        let resize_service = probe.as_ref().and_then(|p| p.rootfs.as_ref()).and_then(|r| r.resize_service.clone());
        let size_rows: Vec<AnyElement> = vec![
            setting_row(
                "Sizing",
                Some(
                    [
                        "Shrinks the filesystem to fit its contents, plus some headroom.",
                        "The image is always exactly this size.",
                        "Adds free space on top of the base image's size.",
                    ][mode_ix]
                        .into(),
                ),
                c,
            )
            .child(segmented("size-mode", &["Shrink to Fit", "Fixed", "Add Space"], mode_ix, c, {
                let ws = cx.entity().downgrade();
                move |i, _, cx| {
                    let _ = ws.update(cx, |this, cx| {
                        this.edit_active(
                            move |d| {
                                edits::set_str(d, "size", "mode", ["shrink", "fixed", "grow"][i]);
                                let t = edits::table_mut(d, "size");
                                if i == 1 && !t.contains_key("fixed") {
                                    t.insert("fixed", toml_edit::value("8G"));
                                }
                                if i == 2 && !t.contains_key("grow") {
                                    t.insert("grow", toml_edit::value("2G"));
                                }
                            },
                            cx,
                        );
                        this.recipe_ui.dirty = true;
                    });
                }
            }))
            .into_any_element(),
            setting_row(
                ["Headroom", "Image size", "Extra space"][mode_ix],
                Some(["Free space to keep, such as 15% or 500M.", "For example 8G.", "For example 2G."][mode_ix].into()),
                c,
            )
            .child(div().w(px(110.)).child(fi([Field::Headroom, Field::Fixed, Field::Grow][mode_ix])))
            .into_any_element(),
            setting_row(
                "Expand on first boot",
                Some(
                    resize_service
                        .map(|s| format!("The base image already does this with {s}, so that is used instead."))
                        .unwrap_or_else(|| "Grows the filesystem to fill the card or disk it's written to.".into())
                        .into(),
                ),
                c,
            )
            .child(switch("expand", r.size.expand_on_boot, c).on_click(cx.listener({
                let v = !r.size.expand_on_boot;
                move |this, _, _, cx| this.edit_active(move |d| edits::set_value(d, "size", "expand_on_boot", v), cx)
            })))
            .into_any_element(),
        ];
        out.push(self.section("size", "How much space the finished image has.", vec![group(size_rows, c).into_any_element()], c));

        // ---------------------------------------------------------- Packages
        let tokens = |section: &'static str, key: &'static str, list: &[String], cx: &mut Context<Self>| {
            let mut row = div().flex().flex_wrap().items_center().gap(px(6.));
            for (i, v) in list.iter().enumerate() {
                row = row.child(token(SharedString::from(format!("{section}-{key}-{i}")), v.clone(), &c2, cx.listener(move |this, _, _, cx| {
                    this.edit_active(move |d| edits::remove_at(d, section, key, i), cx)
                })));
            }
            row
        };
        let list_row = |label: &'static str, detail: &'static str, content: gpui::Div| -> AnyElement {
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .px(px(14.))
                .py(px(10.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(1.))
                        .child(div().text_size(px(13.)).child(label))
                        .child(div().text_size(px(11.5)).text_color(c2.muted).child(detail)),
                )
                .child(content)
                .into_any_element()
        };
        let pk_rows: Vec<AnyElement> = vec![
            list_row("Install", "Installed with apt. Type a name and press Return.", tokens("packages", "install", &r.packages.install, cx).child(div().w(px(170.)).child(ad(Adder::Install)))),
            list_row("Remove", "Removed from the image with apt.", tokens("packages", "remove", &r.packages.remove, cx).child(div().w(px(170.)).child(ad(Adder::Remove)))),
            setting_row("Refresh package lists first", Some("Runs apt-get update before installing.".into()), c)
                .child(switch("apt-update", r.packages.apt_update, c).on_click(cx.listener({
                    let v = !r.packages.apt_update;
                    move |this, _, _, cx| this.edit_active(move |d| edits::set_value(d, "packages", "apt_update", v), cx)
                })))
                .into_any_element(),
        ];
        let mut repo_rows: Vec<AnyElement> = Vec::new();
        for (i, repo) in r.repos.iter().enumerate() {
            let suite = repo.suite.clone().unwrap_or_else(|| "image release".into());
            let key = match &repo.key {
                None => "no signing key".to_string(),
                Some(_) if repo.key_is_url() => "key from URL".to_string(),
                Some(k) => format!("key {k}"),
            };
            repo_rows.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(14.))
                    .py(px(8.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_size(px(13.)).font_weight(gpui::FontWeight::SEMIBOLD).child(repo.name.clone()))
                            .child(
                                div()
                                    .font_family(MONO)
                                    .text_size(px(11.5))
                                    .text_color(c.muted)
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(format!("{} {suite} {} · {key}", repo.url, repo.components.join(" "))),
                            ),
                    )
                    .child(button(("repo-edit", i), "Edit…", Kind::Normal, true, c).on_click(cx.listener(move |this, _, w, cx| this.open_repo_sheet(Some(i), w, cx))))
                    .child(button(("repo-rm", i), "Remove", Kind::Danger, true, c).on_click(cx.listener(move |this, _, _, cx| {
                        this.edit_active(move |d| edits::aot_remove(d, "repos", i), cx)
                    })))
                    .into_any_element(),
            );
        }
        repo_rows.push(
            setting_row("Extra repositories", Some("Apt sources added before packages install, such as Docker or NodeSource.".into()), c)
                .child(button("add-repo", "Add Repository…", Kind::Normal, true, c).on_click(cx.listener(|this, _, w, cx| this.open_repo_sheet(None, w, cx))))
                .into_any_element(),
        );
        out.push(self.section(
            "packages",
            "Software to add or take away.",
            vec![group(pk_rows, c).into_any_element(), group(repo_rows, c).into_any_element()],
            c,
        ));

        // ---------------------------------------------------------- Users
        let mut users_body: Vec<AnyElement> = Vec::new();
        for (ui, u) in r.users.iter().enumerate() {
            let mut keys = div().flex().flex_col().gap(px(4.));
            for (ki, k) in u.ssh_keys.iter().enumerate() {
                let desc = key_description(&resolve_project_path(&root, k));
                let hover = c.danger;
                keys = keys.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(div().font_family(MONO).text_size(px(12.)).child(k.clone()))
                        .child(div().text_size(px(11.5)).text_color(c.muted).child(desc))
                        .child(
                            div()
                                .id(("rm-key", ui * 100 + ki))
                                .cursor_pointer()
                                .text_color(c.muted)
                                .hover(move |s| s.text_color(hover))
                                .tooltip(tooltip("Remove this key"))
                                .child("×")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.edit_active(
                                        move |d| {
                                            if let Some(t) = edits::aot_entry(d, "users", ui) {
                                                if let Some(a) = t.get_mut("ssh_keys").and_then(|i| i.as_array_mut()) {
                                                    if ki < a.len() {
                                                        a.remove(ki);
                                                    }
                                                }
                                            }
                                        },
                                        cx,
                                    )
                                })),
                        ),
                );
            }
            keys = keys.child(
                div().flex().child(button(("add-key", ui), "Add Key…", Kind::Normal, true, c).on_click(cx.listener(move |this, _, _, cx| this.add_ssh_key(ui, cx)))),
            );
            let mut gtok = div().flex().flex_wrap().items_center().gap(px(6.));
            for (gi, g) in u.groups.iter().enumerate() {
                gtok = gtok.child(token(SharedString::from(format!("grp-{ui}-{gi}")), g.clone(), c, cx.listener(move |this, _, _, cx| {
                    this.edit_active(
                        move |d| {
                            if let Some(t) = edits::aot_entry(d, "users", ui) {
                                if let Some(a) = t.get_mut("groups").and_then(|i| i.as_array_mut()) {
                                    if gi < a.len() {
                                        a.remove(gi);
                                    }
                                }
                            }
                        },
                        cx,
                    )
                })));
            }
            if let Some(gin) = groups.get(ui) {
                gtok = gtok.child(div().w(px(130.)).child(gin.clone()));
            }
            let prompt = u.password == PasswordMode::Prompt;
            let admin = u.groups.iter().any(|g| g == "sudo" || g == "wheel");
            let initial = u.name.chars().next().map(|ch| ch.to_uppercase().to_string()).unwrap_or_default();
            let n_keys = u.ssh_keys.len();
            let summary = format!(
                "{}{} SSH key{}",
                if admin { "Administrator · " } else { "" },
                n_keys,
                if n_keys == 1 { "" } else { "s" }
            );
            let rows: Vec<AnyElement> = vec![
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(14.))
                    .py(px(10.))
                    .child(
                        div()
                            .flex_none()
                            .size(px(32.))
                            .rounded_full()
                            .bg(c.accent_soft)
                            .text_color(c.accent)
                            .flex()
                            .items_center()
                            .justify_center()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(initial),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .child(div().text_size(px(14.)).font_weight(gpui::FontWeight::SEMIBOLD).child(u.name.clone()))
                            .child(div().text_size(px(11.5)).text_color(c.muted).child(summary)),
                    )
                    .child(button(("rm-user", ui), "Remove User", Kind::Danger, true, c).on_click(cx.listener(move |this, _, _, cx| {
                        this.edit_active(move |d| edits::aot_remove(d, "users", ui), cx)
                    })))
                    .into_any_element(),
                setting_row(
                    "Password",
                    Some(if prompt { "You'll be asked for it each time you build." } else { "Password sign-in is off; use SSH keys." }.into()),
                    c,
                )
                .child(segmented(SharedString::from(format!("pw-{ui}")), &["SSH Keys Only", "Ask When Building"], if prompt { 1 } else { 0 }, c, {
                    let ws = cx.entity().downgrade();
                    move |i, _, cx| {
                        let _ = ws.update(cx, |this, cx| {
                            this.edit_active(
                                move |d| {
                                    if let Some(t) = edits::aot_entry(d, "users", ui) {
                                        t.insert("password", toml_edit::value(["locked", "prompt"][i]));
                                    }
                                },
                                cx,
                            )
                        });
                    }
                }))
                .into_any_element(),
                list_row("Groups", "Extra permissions. sudo makes this user an administrator.", gtok),
                list_row("SSH keys", "Public keys that may sign in as this user.", keys),
            ];
            users_body.push(group(rows, c).into_any_element());
        }
        users_body.push(
            group(
                [setting_row("Add a user", Some("Creates a new account on the image.".into()), c)
                    .child(div().w(px(180.)).child(ad(Adder::User)))
                    .child(button("add-user", "Add", Kind::Normal, true, c).on_click(cx.listener(|this, _, _, cx| {
                        if let Some(e) = this.recipe_ui.inputs.as_ref().map(|i| i.adder(Adder::User)) {
                            let v = e.read(cx).text().trim().to_string();
                            if !v.is_empty() {
                                this.add_token(Adder::User, &v, cx);
                                e.update(cx, |i, cx| i.set_text("", cx));
                            }
                        }
                    })))
                    .into_any_element()],
                c,
            )
            .into_any_element(),
        );
        out.push(self.section("users", "Accounts that can sign in to the device.", users_body, c));

        // ---------------------------------------------------------- System
        let sys_rows: Vec<AnyElement> = vec![
            setting_row("Hostname", Some("The device's name on the network.".into()), c).child(div().w(px(240.)).child(fi(Field::Hostname))).into_any_element(),
            setting_row("Time zone", Some("For example Europe/London.".into()), c).child(div().w(px(240.)).child(fi(Field::Timezone))).into_any_element(),
            setting_row("Language", Some("Locale used for text, dates and numbers.".into()), c).child(div().w(px(240.)).child(fi(Field::Locale))).into_any_element(),
            setting_row("New machine ID on first boot", Some("Each device made from this image gets its own ID.".into()), c)
                .child(switch("reset-mid", r.system.reset_machine_id, c).on_click(cx.listener({
                    let v = !r.system.reset_machine_id;
                    move |this, _, _, cx| this.edit_active(move |d| edits::set_value(d, "system", "reset_machine_id", v), cx)
                })))
                .into_any_element(),
            setting_row("New SSH host keys on first boot", Some("Each device gets its own keys, so copies can't impersonate each other.".into()), c)
                .child(switch("reset-ssh", r.system.reset_ssh_host_keys, c).on_click(cx.listener({
                    let v = !r.system.reset_ssh_host_keys;
                    move |this, _, _, cx| this.edit_active(move |d| edits::set_value(d, "system", "reset_ssh_host_keys", v), cx)
                })))
                .into_any_element(),
        ];
        out.push(self.section("system", "Name, clock and language.", vec![group(sys_rows, c).into_any_element()], c));

        // ---------------------------------------------------------- Files
        let mut file_rows: Vec<AnyElement> = Vec::new();
        for (i, f) in r.files.iter().enumerate() {
            let hover = c.danger;
            file_rows.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(14.))
                    .py(px(8.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(div().font_family(MONO).text_size(px(12.5)).overflow_hidden().whitespace_nowrap().text_ellipsis().child(f.dst.clone()))
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(c.muted)
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(format!("from {} · {} · {}", f.src, f.mode, f.owner)),
                            ),
                    )
                    .child(
                        div()
                            .id(("rm-file", i))
                            .flex_none()
                            .cursor_pointer()
                            .text_color(c.muted)
                            .hover(move |s| s.text_color(hover))
                            .tooltip(tooltip("Remove this file"))
                            .child("×")
                            .on_click(cx.listener(move |this, _, _, cx| this.edit_active(move |d| edits::aot_remove(d, "files", i), cx))),
                    )
                    .into_any_element(),
            );
        }
        if r.files.is_empty() {
            file_rows.push(
                div().px(px(14.)).py(px(12.)).text_size(px(12.5)).text_color(c.muted).child("No files yet. Drop files here or choose them below.").into_any_element(),
            );
        }
        file_rows.push(
            setting_row("Add files", Some("Copied to this folder or path in the image. Defaults to /opt/.".into()), c)
                .child(div().w(px(200.)).child(file_dst))
                .child(button("add-file", "Choose…", Kind::Normal, true, c).on_click(cx.listener(|_, _, _, cx| {
                    let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Add Files".into()) });
                    cx.spawn(async move |this, cx| {
                        if let Ok(Ok(Some(paths))) = rx.await {
                            let _ = this.update(cx, |this, cx| this.add_files(paths, cx));
                        }
                    })
                    .detach();
                })))
                .into_any_element(),
        );
        let overlay_count = count_files(&root.join("overlay"));
        let files_body = vec![
            div()
                .id("files-drop")
                .w_full()
                .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                    cx.stop_propagation();
                    this.add_files(paths.paths().to_vec(), cx);
                }))
                .child(group(file_rows, c))
                .into_any_element(),
            group(
                [setting_row(
                    "Overlay folder",
                    Some(
                        format!(
                            "{overlay_count} file{} in the project's overlay folder, copied over / as root.",
                            if overlay_count == 1 { "" } else { "s" }
                        )
                        .into(),
                    ),
                    c,
                )
                .child(button("reveal-overlay", "Show in Finder", Kind::Normal, true, c).on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(p) = this.active() {
                        let o = p.project.path.join("overlay");
                        let _ = std::fs::create_dir_all(&o);
                        cx.reveal_path(&o);
                    }
                })))
                .into_any_element()],
                c,
            )
            .into_any_element(),
        ];
        out.push(self.section("files", "Files from your Mac to place in the image.", files_body, c));

        // ---------------------------------------------------------- Services
        let sv_rows: Vec<AnyElement> = vec![
            list_row("Start at boot", "systemd units to enable, such as ssh.service.", tokens("services", "enable", &r.services.enable, cx).child(div().w(px(170.)).child(ad(Adder::Enable)))),
            list_row("Don't start at boot", "systemd units to disable.", tokens("services", "disable", &r.services.disable, cx).child(div().w(px(170.)).child(ad(Adder::Disable)))),
        ];
        out.push(self.section("services", "Background services and what runs when the device starts.", vec![group(sv_rows, c).into_any_element()], c));

        // ---------------------------------------------------------- Steps
        let mut steps_body: Vec<AnyElement> = Vec::new();
        for (when, heading, detail, empty) in [
            (StepWhen::First, "Run first", "Before repositories, packages and everything else.", "Nothing runs before the build's own work."),
            (StepWhen::Last, "Run last", "After packages, users, files and services, just before the image is assembled.", "No custom steps run at the end."),
        ] {
            let ixs: Vec<usize> = r.steps.iter().enumerate().filter(|(_, s)| s.when == when).map(|(i, _)| i).collect();
            steps_body.push(
                div()
                    .flex()
                    .items_end()
                    .gap(px(8.))
                    .px(px(6.))
                    .pt(px(4.))
                    .child(div().text_size(px(12.5)).font_weight(gpui::FontWeight::SEMIBOLD).child(heading))
                    .child(div().flex_1().text_size(px(11.5)).text_color(c.muted).child(detail))
                    .into_any_element(),
            );
            for (n, &i) in ixs.iter().enumerate() {
                let s = &r.steps[i];
                let (prev, next) = (n.checked_sub(1).map(|p| ixs[p]), ixs.get(n + 1).copied());
                let enabled = s.enabled;
                let lines: Vec<&str> = s.run.trim_end().lines().collect();
                const PREVIEW: usize = 6;
                let more = lines.len().saturating_sub(PREVIEW);
                let header = div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(14.))
                    .py(px(9.))
                    .child(switch(("step-en", i), enabled, c).tooltip(tooltip(if enabled { "Runs during the build. Click to skip it." } else { "Skipped. Click to run it again." })).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.edit_active(
                                move |d| {
                                    if let Some(t) = edits::aot_entry(d, "steps", i) {
                                        if enabled {
                                            t.insert("enabled", toml_edit::value(false));
                                        } else {
                                            t.remove("enabled");
                                        }
                                    }
                                },
                                cx,
                            )
                        }),
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_size(px(13.5)).font_weight(gpui::FontWeight::SEMIBOLD).child(s.name.clone()))
                            .child(div().text_size(px(11.5)).text_color(c.muted).child(if enabled { format!("Step {}", n + 1) } else { format!("Step {} · skipped", n + 1) })),
                    )
                    .child(button(("step-up", i), "↑", Kind::Ghost, prev.is_some(), c).tooltip(tooltip("Move up")).on_click_if(prev.is_some(), cx.listener(move |this, _, _, cx| {
                        if let Some(p) = prev {
                            this.edit_active(move |d| edits::aot_swap(d, "steps", i, p), cx)
                        }
                    })))
                    .child(button(("step-down", i), "↓", Kind::Ghost, next.is_some(), c).tooltip(tooltip("Move down")).on_click_if(next.is_some(), cx.listener(move |this, _, _, cx| {
                        if let Some(q) = next {
                            this.edit_active(move |d| edits::aot_swap(d, "steps", i, q), cx)
                        }
                    })))
                    .child(button(("step-edit", i), "Edit…", Kind::Normal, true, c).on_click(cx.listener(move |this, _, w, cx| this.edit_step(Some(i), when, w, cx))))
                    .child(button(("step-rm", i), "Remove", Kind::Danger, true, c).on_click(cx.listener(move |this, _, _, cx| {
                        this.edit_active(move |d| edits::aot_remove(d, "steps", i), cx)
                    })));
                let code = div()
                    .px(px(14.))
                    .py(px(10.))
                    .bg(c.log_bg)
                    .font_family(MONO)
                    .text_size(px(12.))
                    .line_height(px(17.))
                    .children(lines.iter().take(PREVIEW).map(|l| div().whitespace_nowrap().overflow_hidden().child(l.to_string())))
                    .when(more > 0, |d| d.child(div().text_color(c.muted).child(format!("… {more} more line{}", if more == 1 { "" } else { "s" }))));
                steps_body.push(group([header.into_any_element(), code.into_any_element()], c).when(!enabled, |d| d.opacity(0.65)).into_any_element());
            }
            if ixs.is_empty() {
                steps_body.push(group([div().px(px(14.)).py(px(10.)).text_size(px(12.5)).text_color(c.muted).child(empty).into_any_element()], c).into_any_element());
            }
            let id = if when == StepWhen::First { "add-step-first" } else { "add-step-last" };
            steps_body.push(
                div().flex().px(px(6.)).child(button(id, "Add Step…", Kind::Normal, true, c).on_click(cx.listener(move |this, _, w, cx| this.edit_step(None, when, w, cx)))).into_any_element(),
            );
        }
        out.push(self.section("steps", "Your own shell scripts, run inside the image with bash.", steps_body, c));

        // ---------------------------------------------------------- Output
        let comp_ix = match r.output.compress {
            Compress::None => 0,
            Compress::Zstd => 1,
            Compress::Xz => 2,
        };
        let level = r.output.effective_level();
        let max_level = if r.output.compress == Compress::Xz { 9 } else { 19 };
        let today = chrono_today();
        let preview = format!("{}-{today}.img{}", r.image.name, r.output.compress.extension());
        let mut out_rows: Vec<AnyElement> = vec![setting_row(
            "Compression",
            Some(["Writes an uncompressed .img file.", "Fast to compress and decompress. A good default.", "Smallest files, but slow to compress."][comp_ix].into()),
            c,
        )
        .child(segmented("compress", &["None", "zstd", "xz"], comp_ix, c, {
            let ws = cx.entity().downgrade();
            move |i, _, cx| {
                let _ = ws.update(cx, |this, cx| {
                    this.edit_active(
                        move |d| {
                            edits::set_str(d, "output", "compress", ["none", "zstd", "xz"][i]);
                            let t = edits::table_mut(d, "output");
                            let lvl = t.get("level").and_then(|v| v.as_integer());
                            let max = if i == 2 { 9 } else { 19 };
                            if lvl.is_some_and(|l| l > max) {
                                t.insert("level", toml_edit::value(max));
                            }
                        },
                        cx,
                    )
                });
            }
        }))
        .into_any_element()];
        if comp_ix > 0 {
            out_rows.push(
                setting_row("Compression level", Some(format!("1 to {max_level}. Higher makes smaller files but takes longer.").into()), c)
                    .child(stepper("level", level.to_string(), c, {
                        let ws = cx.entity().downgrade();
                        move |delta, _, cx| {
                            let v = (level as i64 + delta).clamp(1, max_level);
                            let _ = ws.update(cx, |this, cx| this.edit_active(move |d| edits::set_value(d, "output", "level", v), cx));
                        }
                    }))
                    .into_any_element(),
            );
        }
        out_rows.push(
            setting_row("Block map", Some("Writes a .bmap file so bmaptool can flash the image faster.".into()), c)
                .child(switch("bmap", r.output.bmap, c).on_click(cx.listener({
                    let v = !r.output.bmap;
                    move |this, _, _, cx| this.edit_active(move |d| edits::set_value(d, "output", "bmap", v), cx)
                })))
                .into_any_element(),
        );
        out_rows.push(
            setting_row("Checksum file", Some("Writes a SHA-256 checksum next to the image.".into()), c)
                .child(switch("checksum", r.output.checksum == Checksum::Sha256, c).on_click(cx.listener({
                    let v = if r.output.checksum == Checksum::Sha256 { "none" } else { "sha256" };
                    move |this, _, _, cx| this.edit_active(move |d| edits::set_str(d, "output", "checksum", v), cx)
                })))
                .into_any_element(),
        );
        out_rows.push(setting_row("File name", None, c).child(div().font_family(MONO).text_size(px(12.)).text_color(c.muted).child(preview)).into_any_element());
        out.push(self.section("output", "How the finished image is saved.", vec![group(out_rows, c).into_any_element()], c));
        out
    }
}

fn chrono_today() -> String {
    chrono::Local::now().format("%Y%m%d").to_string()
}

fn count_files(dir: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => count_files(&e.path()),
            Ok(_) => 1,
            Err(_) => 0,
        })
        .sum()
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

pub fn collapse_home(p: &Path) -> String {
    if let Some(h) = std::env::var_os("HOME") {
        if let Ok(rest) = p.strip_prefix(&h) {
            return format!("~/{}", rest.display());
        }
    }
    p.to_string_lossy().into_owned()
}

/// "ed25519 · user@host" from a public key file, or why it's unusable.
fn key_description(p: &Path) -> String {
    match std::fs::read_to_string(p) {
        Ok(t) => {
            let line = t.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            let mut parts = line.split_whitespace();
            let kind = parts.next().unwrap_or("?").trim_start_matches("ssh-");
            let comment = parts.nth(1).unwrap_or("");
            format!("{kind} {comment}")
        }
        Err(_) => "missing".into(),
    }
}
