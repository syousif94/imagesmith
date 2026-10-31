//! Build tab: stage list on the left, virtualized log on the right.

use crate::theme::{Colors, MONO};
use crate::widgets::{button, fmt_ms, progress_bar, strip_ansi, ClickIf, Kind};
use crate::workspace::{Tab, Workspace};
use gpui::prelude::*;
use gpui::{div, px, uniform_list, AnyElement, Context, ScrollStrategy, ScrollWheelEvent, SharedString, Window};
use imagesmith_core::size::human;
use imagesmith_core::tracker::{looks_like_error, BuildState, StageStatus, STAGES};
use std::collections::HashSet;
use std::rc::Rc;

fn spinner_frame() -> &'static str {
    const F: [&str; 4] = ["◐", "◓", "◑", "◒"];
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    F[(ms / 150 % 4) as usize]
}

impl Workspace {
    /// Absolute line indices when a stage or error filter is active.
    pub fn filtered_log(&mut self) -> Option<Rc<Vec<u64>>> {
        let filter = self.log.filter_stage.clone();
        let errors = self.log.errors_only;
        if filter.is_none() && !errors {
            return None;
        }
        let computed = {
            let view = self.current_view()?;
            let log = &view.tracker.log;
            let key = (log.first, log.next_index(), String::new(), filter.clone(), errors);
            if let Some((k, v)) = &self.log.cache {
                if *k == key {
                    return Some(v.clone());
                }
            }
            let ids: HashSet<&str> = match &filter {
                Some(f) => view
                    .tracker
                    .stages
                    .iter()
                    .filter(|s| s.id == *f || s.parent.as_deref() == Some(f.as_str()))
                    .map(|s| s.id.as_str())
                    .collect(),
                None => HashSet::new(),
            };
            let v: Vec<u64> = log
                .lines
                .iter()
                .enumerate()
                .filter(|(_, l)| filter.is_none() || l.stage.as_deref().is_some_and(|s| ids.contains(s)))
                .filter(|(_, l)| !errors || looks_like_error(&l.text))
                .map(|(i, _)| log.first + i as u64)
                .collect();
            (key, Rc::new(v))
        };
        let v = computed.1.clone();
        self.log.cache = Some(computed);
        Some(v)
    }

    pub fn visible_log_len(&mut self) -> usize {
        match self.filtered_log() {
            Some(v) => v.len(),
            None => self.current_view().map(|v| v.tracker.log.lines.len()).unwrap_or(0),
        }
    }

    pub fn visible_index_of(&mut self, abs: u64) -> usize {
        match self.filtered_log() {
            Some(v) => v.partition_point(|&x| x < abs),
            None => self.current_view().map(|v| abs.saturating_sub(v.tracker.log.first) as usize).unwrap_or(0),
        }
    }

    fn line_at(&mut self, ix: usize) -> Option<(u64, String, Option<String>)> {
        let filtered = self.filtered_log();
        let view = self.current_view()?;
        let log = &view.tracker.log;
        let abs = match &filtered {
            Some(v) => *v.get(ix)?,
            None => log.first + ix as u64,
        };
        let l = log.lines.get((abs - log.first) as usize)?;
        Some((abs, l.text.clone(), l.stage.clone()))
    }

    pub fn next_search_hit(&mut self, cx: &mut Context<Self>) {
        self.step_search(1, cx)
    }

    pub fn step_search(&mut self, dir: i64, cx: &mut Context<Self>) {
        let q = self.log.query.to_lowercase();
        if q.is_empty() {
            return;
        }
        let n = self.visible_log_len();
        if n == 0 {
            return;
        }
        let start = self.log.hit as i64;
        for k in 1..=n as i64 {
            let ix = (start + dir * k).rem_euclid(n as i64) as usize;
            if let Some((_, text, _)) = self.line_at(ix) {
                if text.to_lowercase().contains(&q) {
                    self.log.hit = ix;
                    self.log.follow = false;
                    self.log.scroll.scroll_to_item(ix, ScrollStrategy::Center);
                    cx.notify();
                    return;
                }
            }
        }
    }

    fn render_stage_list(&mut self, c: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let filter = self.log.filter_stage.clone();
        let Some(view) = self.current_view() else { return div().into_any_element() };
        let mut col = div().flex().flex_col().gap(px(1.)).p(px(8.));
        for (i, s) in view.tracker.stages.iter().enumerate() {
            let (icon, color) = match s.status {
                StageStatus::Pending => ("○", c.faint),
                StageStatus::Running => (spinner_frame(), c.accent),
                StageStatus::Done => ("✓", c.success),
                StageStatus::Warn => ("▲", c.warning),
                StageStatus::Failed => ("✗", c.danger),
                StageStatus::Skipped => ("–", c.faint),
                StageStatus::Cached => ("⚡", c.warning),
            };
            let dur = match (s.status, s.duration_ms, s.started_ms, view.run_started) {
                (StageStatus::Cached, _, _, _) => "cached".to_string(),
                (_, Some(d), _, _) => fmt_ms(d),
                (StageStatus::Running, None, Some(st), Some(rs)) if view.live => {
                    fmt_ms((rs.elapsed().as_millis() as u64).saturating_sub(st))
                }
                _ => String::new(),
            };
            let selected = filter.as_deref() == Some(s.id.as_str());
            let id = s.id.clone();
            let progress = s.progress.as_ref().filter(|_| s.status == StageStatus::Running).and_then(|p| {
                let t = p.total.filter(|t| *t > 0)?;
                let label = if p.unit == "bytes" { format!("{} / {}", human(p.done), human(t)) } else { format!("{} / {} {}", p.done, t, p.unit) };
                Some((p.done as f64 / t as f64, label))
            });
            let hover = c.hover;
            col = col.child(
                div()
                    .id(("stage", i))
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .px(px(8.))
                    .py(px(5.))
                    .when(s.parent.is_some(), |d| d.pl(px(30.)))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(c.selected))
                    .when(!selected, move |d| d.hover(move |st| st.bg(hover)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(div().w(px(14.)).flex().justify_center().text_color(color).child(icon))
                            .child(
                                div()
                                    .flex_1()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .when(s.status == StageStatus::Running, |d| d.font_weight(gpui::FontWeight::SEMIBOLD))
                                    .when(s.status == StageStatus::Pending, |d| d.text_color(c.muted))
                                    .child(s.title.clone()),
                            )
                            .child(div().text_size(px(11.5)).text_color(c.muted).child(dur)),
                    )
                    .children(progress.map(|(f, label)| {
                        div()
                            .pl(px(22.))
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(progress_bar(f, c))
                            .child(div().text_size(px(10.5)).text_color(c.muted).child(label))
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.log.filter_stage = if this.log.filter_stage.as_deref() == Some(id.as_str()) { None } else { Some(id.clone()) };
                        this.log.cache = None;
                        this.log.follow = this.log.filter_stage.is_none() && this.live.is_some();
                        if let Some(start) = this.current_view().and_then(|v| v.tracker.stages.iter().find(|s| s.id == id)).map(|s| s.log_range.0) {
                            let ix = this.visible_index_of(start);
                            this.log.scroll.scroll_to_item(ix, ScrollStrategy::Top);
                        }
                        cx.notify();
                    })),
            );
        }
        div().id("stages").w(px(280.)).flex_none().h_full().border_r_1().border_color(c.border).overflow_y_scroll().child(col).into_any_element()
    }

    fn render_log(&mut self, c: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let n = self.visible_log_len();
        let colors = *c;
        let follow = self.log.follow;
        let errors_only = self.log.errors_only;
        let toggle = |id: &'static str, on: bool, label: &'static str, c: &Colors| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap(px(5.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(if on { c.text } else { c.muted })
                .child(div().size(px(8.)).rounded_full().bg(if on { c.accent } else { c.border }))
                .child(label)
        };
        let has_query = !self.log.query.is_empty();
        let controls = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .px(px(10.))
            .h(px(40.))
            .border_b_1()
            .border_color(c.border)
            .child(div().id("log-search").w(px(220.)).tooltip(crate::widgets::tooltip("Search the log (⌘F)")).child(self.log.search.clone()))
            .child(button("search-prev", "↑", Kind::Ghost, has_query, c).on_click_if(has_query, cx.listener(|this, _, _, cx| this.step_search(-1, cx))))
            .child(button("search-next", "↓", Kind::Ghost, has_query, c).on_click_if(has_query, cx.listener(|this, _, _, cx| this.step_search(1, cx))))
            .child(div().flex_1())
            .when_some(self.log.filter_stage.clone(), |d, f| {
                d.child(
                    div()
                        .id("clear-filter")
                        .cursor_pointer()
                        .px(px(8.))
                        .rounded(px(9.))
                        .bg(c.info_soft)
                        .text_size(px(11.5))
                        .child(format!("stage: {f}  ×"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.log.filter_stage = None;
                            this.log.cache = None;
                            cx.notify();
                        })),
                )
            })
            .child(toggle("errors-only", errors_only, "Errors only", c).on_click(cx.listener(|this, _, _, cx| {
                this.log.errors_only = !this.log.errors_only;
                this.log.cache = None;
                cx.notify();
            })))
            .child(toggle("follow", follow, "Follow", c).on_click(cx.listener(|this, _, _, cx| {
                this.log.follow = !this.log.follow;
                if this.log.follow {
                    let n = this.visible_log_len();
                    if n > 0 {
                        this.log.scroll.scroll_to_item(n - 1, ScrollStrategy::Bottom);
                    }
                }
                cx.notify();
            })))
            .child(button("copy-log", "Copy", Kind::Ghost, n > 0, c).on_click_if(n > 0, cx.listener(|this, _, _, cx| {
                let text = this.log_text();
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                this.toast("Log copied", cx);
            })))
            .child(button("save-log", "Save…", Kind::Ghost, n > 0, c).on_click_if(n > 0, cx.listener(|this, _, _, cx| this.save_log(cx))));

        let query = self.log.query.to_lowercase();
        let hit = self.log.hit;
        let list = uniform_list(
            "log",
            n,
            cx.processor(move |this: &mut Workspace, range: std::ops::Range<usize>, _window, _cx| {
                let c = colors;
                range
                    .filter_map(|ix| {
                        let (_, raw, _) = this.line_at(ix)?;
                        let text = strip_ansi(&raw);
                        let is_sep = text.starts_with("── ") && text.ends_with(" ──");
                        let color = if text.starts_with("warning:") {
                            c.warning
                        } else if looks_like_error(&text) {
                            c.danger
                        } else if text.starts_with("· ") || text.starts_with("+ ") {
                            c.muted
                        } else {
                            c.text
                        };
                        let matched = !query.is_empty() && text.to_lowercase().contains(&query);
                        let el = if is_sep {
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .h(px(18.))
                                .px(px(10.))
                                .text_color(c.muted)
                                .child(div().w(px(16.)).h(px(1.)).bg(c.border))
                                .child(SharedString::from(text.trim_matches(|ch| ch == '─' || ch == ' ').to_string()))
                                .child(div().flex_1().h(px(1.)).bg(c.border))
                        } else {
                            div()
                                .h(px(18.))
                                .px(px(10.))
                                .whitespace_nowrap()
                                .text_color(color)
                                .when(matched, |d| d.bg(if ix == hit { c.warning_soft } else { c.info_soft }))
                                .child(SharedString::from(text))
                        };
                        Some(el)
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(self.log.scroll.clone())
        .font_family(MONO)
        .text_size(px(12.))
        .line_height(px(18.))
        .flex_1()
        .bg(c.log_bg)
        .on_scroll_wheel(cx.listener(|this, e: &ScrollWheelEvent, window, cx| {
            let dy = e.delta.pixel_delta(window.line_height()).y;
            let base = this.log.scroll.0.borrow().base_handle.clone();
            let at_bottom = base.offset().y <= -base.max_offset().height + px(4.);
            if dy > px(0.) && this.log.follow {
                this.log.follow = false;
                cx.notify();
            } else if dy < px(0.) && at_bottom && !this.log.follow && this.live.is_some() {
                this.log.follow = true;
                cx.notify();
            }
        }));

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(controls)
            .child(if n == 0 {
                div().flex_1().flex().items_center().justify_center().text_color(c.faint).bg(c.log_bg).child("No log output").into_any_element()
            } else {
                list.into_any_element()
            })
            .into_any_element()
    }

    pub fn log_text(&mut self) -> String {
        let n = self.visible_log_len();
        (0..n).filter_map(|i| self.line_at(i).map(|(_, t, _)| strip_ansi(&t))).collect::<Vec<_>>().join("\n")
    }

    fn save_log(&mut self, cx: &mut Context<Self>) {
        let text = self.log_text();
        let dir = self.active().map(|p| p.project.path.clone()).unwrap_or_default();
        let rx = cx.prompt_for_new_path(&dir, Some("build.log"));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(p))) = rx.await {
                let r = std::fs::write(&p, text);
                let _ = this.update(cx, |this, cx| match r {
                    Ok(()) => this.toast(format!("Saved {}", p.display()), cx),
                    Err(e) => this.toast(format!("Save failed: {e}"), cx),
                });
            }
        })
        .detach();
    }

    fn render_header(&mut self, c: &Colors) -> AnyElement {
        let Some(v) = self.current_view() else { return div().into_any_element() };
        let top = v.tracker.stages.iter().filter(|s| s.parent.is_none()).count();
        let idx = v
            .tracker
            .current_stage()
            .and_then(|s| STAGES.iter().position(|(id, _)| *id == s.id || Some(*id) == s.parent.as_deref()))
            .map(|i| i + 1);
        let label = match (v.live, v.state) {
            (true, BuildState::Queued) | (true, BuildState::StartingRuntime) => {
                v.note.clone().unwrap_or_else(|| "Starting runtime …".into())
            }
            (true, _) => match idx {
                Some(i) => format!("Running · stage {i} of {top}"),
                None => "Running".into(),
            },
            (false, BuildState::Succeeded) if v.extract_only => "Extracted".into(),
            (false, BuildState::Succeeded) => "Succeeded".into(),
            (false, BuildState::Failed) => "Failed".into(),
            (false, BuildState::Cancelled) => "Cancelled".into(),
            (false, s) => format!("{s:?}"),
        };
        let frac = if v.state == BuildState::Succeeded { 1.0 } else { v.tracker.overall_progress() };
        let prep = v.prep.as_ref().filter(|_| v.live && v.run_started.is_none()).map(|(l, d, t)| {
            let f = if *t > 0 { *d as f64 / *t as f64 } else { 0.0 };
            (f, format!("{l} · {} / {}", human(*d), human(*t)))
        });
        let color = match v.state {
            BuildState::Failed => c.danger,
            BuildState::Succeeded => c.success,
            _ => c.text,
        };
        let elapsed = fmt_ms(v.elapsed_ms());
        let title = v.build_id.clone();
        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(8.))
            .px(px(20.))
            .py(px(12.))
            .border_b_1()
            .border_color(c.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(div().font_weight(gpui::FontWeight::SEMIBOLD).text_color(color).child(label))
                    .when_some(title, |d, t| d.child(div().ml(px(10.)).text_size(px(12.)).text_color(c.muted).child(t)))
                    .child(div().flex_1())
                    .child(div().text_color(c.muted).text_size(px(12.5)).child(format!("{elapsed} elapsed"))),
            )
            .child(progress_bar(prep.as_ref().map(|p| p.0).unwrap_or(frac), c))
            .children(prep.map(|(_, l)| div().text_size(px(11.5)).text_color(c.muted).child(l)))
            .into_any_element()
    }

    /// The failure of the shown build as plain text: message, hint, recipe errors and the log tail.
    pub fn failure_report(&mut self) -> Option<String> {
        let v = self.current_view()?;
        if !matches!(v.state, BuildState::Failed) {
            return None;
        }
        let mut s = String::new();
        let stage = match &v.tracker.failure {
            Some(f) => {
                s.push_str(&format!("Build failed in stage \"{}\": {}\n", f.stage, f.message));
                if let Some(h) = &f.hint {
                    s.push_str(&format!("Hint: {h}\n"));
                }
                Some(f.stage.clone())
            }
            None => {
                s.push_str(&format!("Build failed: {}\n", v.error.clone().unwrap_or_else(|| "unknown error".into())));
                None
            }
        };
        for d in v.diagnostics.iter().filter(|d| d.severity == imagesmith_core::validate::Severity::Error) {
            s.push_str(&format!("Recipe error {}: {}\n", d.field, d.message));
        }
        let log = &v.tracker.log.lines;
        let in_stage: Vec<&str> = log.iter().filter(|l| stage.is_some() && l.stage == stage).map(|l| l.text.as_str()).collect();
        let lines: Vec<&str> = if in_stage.is_empty() { log.iter().map(|l| l.text.as_str()).collect() } else { in_stage };
        let tail: Vec<String> = lines[lines.len().saturating_sub(60)..].iter().map(|l| strip_ansi(l)).collect();
        if !tail.is_empty() {
            s.push_str(&format!("\nLast log lines:\n{}\n", tail.join("\n")));
        }
        Some(s)
    }

    fn render_failure(&mut self, c: &Colors, cx: &mut Context<Self>) -> Option<AnyElement> {
        let v = self.current_view()?;
        if v.live || !matches!(v.state, BuildState::Failed) {
            return None;
        }
        let (message, hint, stage) = match &v.tracker.failure {
            Some(f) => (f.message.clone(), f.hint.clone(), Some(f.stage.clone())),
            None => (v.error.clone().unwrap_or_else(|| "build failed".into()), None, None),
        };
        let diags: Vec<String> = v.diagnostics.iter().filter(|d| d.severity == imagesmith_core::validate::Severity::Error).map(|d| format!("{}: {}", d.field, d.message)).collect();
        let can_shell = self.active().is_some_and(|p| p.has_stage) && stage.is_some();
        let step = stage.as_ref().and_then(|s| s.strip_prefix("step-")).and_then(|n| n.parse::<usize>().ok());
        let card = match stage.as_deref() {
            Some("packages") => "packages",
            Some("configure") => "system",
            Some("files") => "files",
            Some("assemble") => "size",
            Some(s) if s.starts_with("step") || s.starts_with("first") => "steps",
            _ => "image",
        };
        Some(
            div()
                .flex()
                .items_start()
                .gap(px(10.))
                .mx(px(12.))
                .mt(px(10.))
                .p(px(10.))
                .rounded(px(7.))
                .border_1()
                .border_color(c.danger.opacity(0.5))
                .bg(c.danger_soft)
                .child(div().text_color(c.danger).child("✗"))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .gap(px(3.))
                        .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(message))
                        .children(hint.map(|h| div().text_size(px(12.)).text_color(c.muted).child(h)))
                        .children(diags.into_iter().map(|d| div().text_size(px(12.)).child(format!("• {d}")))),
                )
                .child(button("fail-copy", "Copy", Kind::Normal, true, c).tooltip(crate::widgets::tooltip("Copy the error and the end of the log")).on_click(cx.listener(|this, _, _, cx| {
                    if let Some(r) = this.failure_report() {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(r));
                        this.toast("Error copied", cx);
                    }
                })))
                .child(button("fail-fix", "Fix with Assistant", Kind::Primary, true, c).tooltip(crate::widgets::tooltip("Start a new assistant chat about this failure")).on_click(cx.listener(
                    |this, _, w, cx| {
                        if let Some(r) = this.failure_report() {
                            this.fix_with_assistant(r, w, cx);
                        }
                    },
                )))
                .child(button("fail-shell", "Open Shell", Kind::Normal, can_shell, c).on_click_if(can_shell, cx.listener(|this, _, w, cx| this.open_shell(w, cx))))
                .child(button("fail-edit", if step.is_some() { "Edit Step" } else { "Edit Recipe" }, Kind::Normal, true, c).on_click(cx.listener(
                    move |this, _, w, cx| {
                        this.recipe_ui.toml_view = false;
                        this.recipe_ui.scroll_to = Some(card);
                        this.set_tab(Tab::Recipe, w, cx);
                    },
                )))
                .into_any_element(),
        )
    }

    pub fn render_build_tab(&mut self, c: &Colors, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.current_view().is_none() {
            let p = self.active();
            let can = p.is_some_and(|p| p.can_build());
            let mut stages = div().flex().flex_col().gap(px(6.)).p(px(16.)).w(px(280.)).border_r_1().border_color(c.border).h_full();
            for (_, title) in STAGES {
                stages = stages.child(div().flex().gap(px(8.)).text_color(c.muted).child(div().text_color(c.faint).child("○")).child(title));
            }
            return div()
                .flex()
                .size_full()
                .child(stages)
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap(px(10.))
                        .child(div().text_color(c.muted).child("No builds yet"))
                        .child(button("idle-build", "Build", Kind::Primary, can, c).tooltip(crate::widgets::tooltip("Build the image (⌘B)")).on_click_if(can, cx.listener(|this, _, w, cx| this.request_build(false, w, cx)))),
                )
                .into_any_element();
        }
        let header = self.render_header(c);
        let failure = self.render_failure(c, cx);
        let stages = self.render_stage_list(c, cx);
        let log = self.render_log(c, cx);
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(header)
            .children(failure)
            .child(div().flex().flex_1().min_h_0().child(stages).child(log))
            .into_any_element()
    }
}
