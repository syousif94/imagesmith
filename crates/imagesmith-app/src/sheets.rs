//! Modal sheets: New Project, password prompt, runtime onboarding.

use crate::theme::{Colors, MONO};
use crate::widgets::{button, checkbox, field_row, segmented, tooltip, ClickIf, Kind};
use crate::workspace::{Sheet, Workspace};
use gpui::prelude::*;
use gpui::{div, px, AnyElement, Context, ExternalPaths, Window};
use imagesmith_core::recipe::StepWhen;
use imagesmith_core::size::human;

const CONTAINER_URL: &str = "https://github.com/apple/container/releases";

fn open_full_disk_access() {
    let _ = std::process::Command::new("/usr/bin/open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
        .spawn();
}

impl Workspace {
    pub fn render_sheet(&mut self, c: &Colors, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let large = matches!(self.sheet, Some(Sheet::Shell { .. } | Sheet::Step { .. }));
        let body: AnyElement = match self.sheet.as_ref()? {
            Sheet::Shell { project_id, term } => {
                let t = term.read(cx);
                let status = match t.exited {
                    Some(Some(0)) | Some(None) => Some("Shell exited".to_string()),
                    Some(Some(code)) => Some(format!("Shell exited with status {code}")),
                    None => None,
                };
                let title = t.title.clone().unwrap_or_else(|| format!("chroot {}/projects/{project_id}/stage", imagesmith_core::runner::STORE_MOUNT));
                let name = self.projects.iter().find(|p| p.id() == project_id).map(|p| p.name()).unwrap_or_default();
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().text_size(px(15.)).font_weight(gpui::FontWeight::SEMIBOLD).child(format!("Shell · {name}")))
                                    .child(
                                        div()
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .text_size(px(11.5))
                                            .text_color(c.muted)
                                            .font_family(MONO)
                                            .child(title),
                                    ),
                            )
                            .children(status.map(|s| div().flex_none().text_size(px(12.)).text_color(c.warning).child(s)))
                            .child(
                                button("shell-capture", "Keep Changes…", Kind::Normal, true, c)
                                    .tooltip(tooltip("Close the shell and choose which changes to record in the recipe"))
                                    .on_click(cx.listener(|this, _, w, cx| this.close_shell(true, w, cx))),
                            )
                            .child(
                                button("shell-close", "Done", Kind::Primary, true, c)
                                    .tooltip(tooltip("Close the shell (⌘W). Changes stay in the stage until you keep them."))
                                    .on_click(cx.listener(|this, _, w, cx| this.close_shell(false, w, cx))),
                            ),
                    )
                    .child(div().flex_1().min_h_0().rounded(px(8.)).overflow_hidden().border_1().border_color(c.border).child(term.clone()))
                    .into_any_element()
            }
            Sheet::Step { kind, index, when, name, area, .. } => {
                let host = *kind == "host_scripts";
                let title = match (host, index.is_some()) {
                    (true, true) => "Edit Mac Script",
                    (true, false) => "New Mac Script",
                    (false, true) => "Edit Step",
                    (false, false) => "New Step",
                };
                let hint = match (host, *when == StepWhen::First) {
                    (true, true) => "Runs with bash on this Mac in the project folder, before the build.",
                    (true, false) => "Runs with bash on this Mac in the project folder, after the image is exported.",
                    (false, true) => "Runs with bash in the chroot before repositories, packages and everything else.",
                    (false, false) => "Runs with bash in the chroot after packages, users, files and services.",
                };
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(div().text_size(px(15.)).font_weight(gpui::FontWeight::SEMIBOLD).child(title))
                            .child(div().w(px(320.)).child(name.clone()))
                            .child(segmented("step-when", if host { &["Before Build", "After Build"] } else { &["Run First", "Run Last"] }, if *when == StepWhen::First { 0 } else { 1 }, c, {
                                let ws = cx.entity().downgrade();
                                move |i, _, cx| {
                                    let _ = ws.update(cx, |this, cx| {
                                        if let Some(Sheet::Step { when, .. }) = &mut this.sheet {
                                            *when = if i == 0 { StepWhen::First } else { StepWhen::Last };
                                        }
                                        cx.notify();
                                    });
                                }
                            }))
                            .child(div().flex_1())
                            .child(div().text_size(px(12.)).text_color(c.muted).child(hint))
                            .child(button("step-cancel", "Cancel", Kind::Normal, true, c).on_click(cx.listener(|this, _, w, cx| this.close_sheet(w, cx))))
                            .child(button("step-save", "Save", Kind::Primary, true, c).tooltip(tooltip("Save (⌘S)")).on_click(cx.listener(|this, _, w, cx| this.save_step(w, cx)))),
                    )
                    .child(div().flex_1().min_h_0().rounded(px(8.)).overflow_hidden().border_1().border_color(c.border).child(area.clone()))
                    .into_any_element()
            }
            Sheet::HostScripts { scripts, .. } => div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .w(px(640.))
                .child(div().text_size(px(17.)).font_weight(gpui::FontWeight::SEMIBOLD).child("Run scripts on this Mac?"))
                .child(div().text_size(px(12.5)).text_color(c.muted).child(
                    "This recipe runs these scripts with your user account, outside the container. They can read and change anything you can. You are asked again whenever a script changes.",
                ))
                .child(
                    div().id("host-scripts").flex().flex_col().gap(px(10.)).max_h(px(380.)).overflow_y_scroll().children(scripts.iter().map(|s| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(format!(
                                "{} · {}",
                                s.name,
                                if s.when == StepWhen::First { "before the build" } else { "after export" }
                            )))
                            .child(div().p(px(8.)).rounded(px(6.)).bg(c.log_bg).font_family(MONO).text_size(px(12.)).children(s.run.trim_end().lines().map(|l| div().child(l.to_string()))))
                    })),
                )
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(button("host-cancel", "Cancel", Kind::Normal, true, c).on_click(cx.listener(|this, _, w, cx| this.close_sheet(w, cx))))
                        .child(button("host-run", "Run and Build", Kind::Primary, true, c).on_click(cx.listener(|this, _, w, cx| this.approve_host_scripts(w, cx)))),
                )
                .into_any_element(),
            Sheet::Repo(s) => div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .w(px(600.))
                .child(div().text_size(px(17.)).font_weight(gpui::FontWeight::SEMIBOLD).child(if s.index.is_some() { "Edit Repository" } else { "Add Repository" }))
                .child(div().text_size(px(12.5)).text_color(c.muted).child("Written to /etc/apt/sources.list.d before packages install. Copy these from the vendor's install instructions."))
                .child(field_row("Name", c).child(div().flex_1().child(s.name.clone())))
                .child(field_row("URL", c).child(div().flex_1().child(s.url.clone())))
                .child(field_row("Suite", c).child(div().flex_1().child(s.suite.clone())))
                .child(field_row("Components", c).child(div().flex_1().child(s.components.clone())))
                .child(
                    field_row("Signing key", c)
                        .child(div().flex_1().child(s.key.clone()))
                        .child(button("repo-key", "Choose…", Kind::Normal, true, c).on_click(cx.listener(|this, _, _, cx| this.choose_repo_key(cx)))),
                )
                .children(s.error.clone().map(|e| div().text_color(c.danger).child(e)))
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(button("repo-cancel", "Cancel", Kind::Normal, true, c).on_click(cx.listener(|this, _, w, cx| this.close_sheet(w, cx))))
                        .child(button("repo-save", if s.index.is_some() { "Save" } else { "Add" }, Kind::Primary, true, c).on_click(cx.listener(|this, _, w, cx| this.save_repo(w, cx)))),
                )
                .into_any_element(),
            Sheet::NewProject(s) => {
                let base = s.base.as_ref().map(|b| b.display().to_string());
                let can_create = s.base.is_some() && !s.probing;
                let probe = match &s.probe {
                    None if s.probing => div().text_color(c.muted).child(s.progress.clone().unwrap_or_else(|| "Probing …".into())).into_any_element(),
                    None => div().text_color(c.faint).child("Choose or drop a base image (.img, .img.xz, .img.zst, .img.gz, .7z).").into_any_element(),
                    Some(Err(e)) => div().text_color(c.warning).child(e.clone()).into_any_element(),
                    Some(Ok(p)) => {
                        let root = p.rootfs.as_ref().map(|r| r.partition);
                        let mut rows = div().flex().flex_col().rounded(px(6.)).border_1().border_color(c.border);
                        for (i, part) in p.partitions.iter().enumerate() {
                            let is_root = Some(part.number) == root;
                            rows = rows.child(
                                div()
                                    .flex()
                                    .gap(px(12.))
                                    .px(px(10.))
                                    .h(px(24.))
                                    .items_center()
                                    .text_size(px(12.5))
                                    .when(i > 0, |d| d.border_t_1().border_color(c.border))
                                    .when(is_root, |d| d.bg(c.selected).font_weight(gpui::FontWeight::SEMIBOLD))
                                    .child(div().w(px(20.)).child(part.number.to_string()))
                                    .child(div().w(px(110.)).child(part.label.clone().unwrap_or_else(|| "-".into())))
                                    .child(div().w(px(60.)).child(part.fstype.clone().unwrap_or_else(|| "-".into())))
                                    .child(div().w(px(80.)).child(human(part.size_bytes)))
                                    .child(div().flex_1())
                                    .when(is_root, |d| d.child(div().text_color(c.muted).text_size(px(11.5)).child("root partition"))),
                            );
                        }
                        let resize = p.rootfs.as_ref().and_then(|r| r.resize_service.clone());
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(p.summary()))
                            .child(rows)
                            .children(resize.map(|r| div().text_size(px(12.)).text_color(c.muted).child(format!("Existing resize service: {r}"))))
                            .into_any_element()
                    }
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .w(px(600.))
                    .child(div().text_size(px(17.)).font_weight(gpui::FontWeight::SEMIBOLD).child("New Project"))
                    .child(field_row("Name", c).child(div().flex_1().child(s.name.clone())))
                    .child(
                        field_row("Location", c)
                            .child(div().flex_1().overflow_hidden().whitespace_nowrap().text_ellipsis().font_family(MONO).text_size(px(12.)).child(s.parent.display().to_string()))
                            .child(button("np-parent", "Choose…", Kind::Normal, true, c).on_click(cx.listener(|this, _, _, cx| this.choose_new_project_parent(cx)))),
                    )
                    .child(
                        field_row("Base image", c)
                            .child(
                                div()
                                    .flex_1()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .font_family(MONO)
                                    .text_size(px(12.))
                                    .text_color(if base.is_some() { c.text } else { c.faint })
                                    .child(base.unwrap_or_else(|| "none".into())),
                            )
                            .child(button("np-base", "Choose…", Kind::Normal, !s.probing, c).on_click_if(!s.probing, cx.listener(|this, _, _, cx| this.choose_new_project_base(cx)))),
                    )
                    .child(
                        div()
                            .id("np-drop")
                            .p(px(12.))
                            .min_h(px(80.))
                            .rounded(px(8.))
                            .border_1()
                            .border_dashed()
                            .border_color(c.border)
                            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                                cx.stop_propagation();
                                if let Some(p) = paths.paths().first().cloned() {
                                    this.set_new_project_base(p, cx);
                                }
                            }))
                            .child(probe),
                    )
                    .children(s.error.clone().map(|e| div().text_color(c.danger).child(e)))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.))
                            .child(button("np-cancel", "Cancel", Kind::Normal, true, c).on_click(cx.listener(|this, _, w, cx| this.close_sheet(w, cx))))
                            .child(button("np-create", "Create", Kind::Primary, can_create, c).on_click_if(can_create, cx.listener(|this, _, w, cx| this.create_new_project(w, cx)))),
                    )
                    .into_any_element()
            }
            Sheet::Flash(s) => {
                let size_line = match &s.source {
                    Ok(src) => format!(
                        "{} compressed → {}",
                        human(src.compressed_bytes),
                        src.uncompressed_bytes.map(human).unwrap_or_else(|| "unknown size".into())
                    ),
                    Err(e) => e.clone(),
                };
                let blocker = s.blocker();
                let can_start = !s.busy && s.selected.is_some() && blocker.is_none();
                let mut rows = div().flex().flex_col().rounded(px(6.)).border_1().border_color(c.border).overflow_hidden();
                if s.devices.is_empty() {
                    rows = rows.child(
                        div()
                            .px(px(10.))
                            .py(px(14.))
                            .text_color(c.faint)
                            .child(if s.show_all { "No disks found." } else { "No removable devices. Insert an SD card or USB stick, then Refresh." }),
                    );
                }
                for (i, d) in s.devices.iter().enumerate() {
                    let sel = s.selected.as_deref() == Some(d.id.as_str());
                    let protected = d.is_protected();
                    let id = d.id.clone();
                    let busy = s.busy;
                    rows = rows.child(
                        div()
                            .id(("flash-dev", i))
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .px(px(10.))
                            .h(px(32.))
                            .text_size(px(12.5))
                            .when(i > 0, |d| d.border_t_1().border_color(c.border))
                            .when(sel, |d| d.bg(c.selected))
                            .when(protected, |d| d.text_color(c.muted))
                            .when(!busy && !protected, |d| d.cursor_pointer())
                            .child(div().w(px(14.)).text_color(c.accent).child(if sel { "●" } else { "○" }))
                            .child(div().w(px(110.)).font_family(MONO).child(d.node.display().to_string()))
                            .child(div().flex_1().overflow_hidden().whitespace_nowrap().text_ellipsis().child(d.name.clone()))
                            .child(div().w(px(80.)).child(human(d.size_bytes)))
                            .child(
                                div()
                                    .w(px(120.))
                                    .text_size(px(11.5))
                                    .text_color(if protected { c.danger } else { c.muted })
                                    .child(if protected { "protected".to_string() } else { d.bus.clone().unwrap_or_default() }),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if busy || protected {
                                    return;
                                }
                                if let Some(Sheet::Flash(f)) = &mut this.sheet {
                                    f.selected = Some(id.clone());
                                    f.error = None;
                                    f.done = None;
                                }
                                cx.notify();
                            })),
                    );
                }
                let bar = s.progress.as_ref().map(|(phase, done, total)| {
                    let frac = total.filter(|t| *t > 0).map(|t| (*done as f32 / t as f32).clamp(0., 1.));
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .flex()
                                .justify_between()
                                .text_size(px(12.))
                                .text_color(c.muted)
                                .child(match phase.as_str() {
                                    "verify" => "Verifying".to_string(),
                                    _ => "Writing".to_string(),
                                })
                                .child(match total {
                                    Some(t) => format!("{} / {}", human(*done), human(*t)),
                                    None => human(*done),
                                }),
                        )
                        .child(
                            div().h(px(6.)).w_full().rounded(px(3.)).bg(c.border).child(
                                div()
                                    .h_full()
                                    .rounded(px(3.))
                                    .bg(c.accent)
                                    .w(gpui::relative(frac.unwrap_or(0.))),
                            ),
                        )
                });
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .w(px(620.))
                    .child(div().text_size(px(17.)).font_weight(gpui::FontWeight::SEMIBOLD).child("Write to SD Card"))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .font_family(MONO)
                                    .text_size(px(12.))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(s.image.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
                            )
                            .child(div().text_size(px(12.)).text_color(c.muted).child(size_line)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child("Device"))
                            .child(div().flex_1())
                            .child(
                                checkbox("flash-all", s.show_all, "Show all disks", c)
                                    .on_click_if(!s.busy, cx.listener(|this, _, _, cx| this.toggle_flash_show_all(cx))),
                            )
                            .child(
                                button("flash-refresh", "Refresh", Kind::Normal, !s.busy, c)
                                    .on_click_if(!s.busy, cx.listener(|this, _, _, cx| this.refresh_flash_devices(cx))),
                            ),
                    )
                    .child(rows)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(16.))
                            .child(checkbox("flash-verify", s.verify, "Verify after writing", c).on_click_if(!s.busy, cx.listener(|this, _, _, cx| {
                                if let Some(Sheet::Flash(f)) = &mut this.sheet {
                                    f.verify = !f.verify;
                                }
                                cx.notify();
                            })))
                            .child(checkbox("flash-eject", s.eject, "Eject when finished", c).on_click_if(!s.busy, cx.listener(|this, _, _, cx| {
                                if let Some(Sheet::Flash(f)) = &mut this.sheet {
                                    f.eject = !f.eject;
                                }
                                cx.notify();
                            }))),
                    )
                    .children(s.stage.clone().map(|t| div().text_size(px(12.5)).text_color(c.muted).child(t)))
                    .children(bar)
                    .children(blocker.filter(|_| !s.busy).map(|b| div().text_size(px(12.5)).text_color(c.warning).child(b)))
                    .children(s.error.clone().map(|e| div().text_color(c.danger).child(e)))
                    .when(s.error.as_deref().is_some_and(|e| e.contains(imagesmith_core::flash::PRIVACY_BLOCKED)), |d| {
                        d.child(
                            div().flex().child(
                                button("flash-open-fda", "Open Full Disk Access", Kind::Normal, true, c)
                                    .on_click(|_, _, _| open_full_disk_access()),
                            ),
                        )
                    })
                    .children(s.done.clone().map(|d| div().text_color(c.accent).child(d)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(11.5))
                                    .text_color(c.muted)
                                    .child("Everything on the selected device is erased. Administrator approval is required."),
                            )
                            .when(s.busy, |d| {
                                d.child(button("flash-cancel-run", "Stop", Kind::Danger, true, c).on_click(cx.listener(|this, _, _, cx| this.cancel_flash(cx))))
                            })
                            .when(!s.busy, |d| {
                                d.child(button("flash-close", "Close", Kind::Normal, true, c).on_click(cx.listener(|this, _, w, cx| this.close_sheet(w, cx)))).child(
                                    button("flash-start", if s.done.is_some() { "Write Again" } else { "Write" }, Kind::Primary, can_start, c)
                                        .on_click_if(can_start, cx.listener(|this, _, _, cx| this.start_flash(cx))),
                                )
                            }),
                    )
                    .into_any_element()
            }
            Sheet::Rename(s) => {
                let busy = s.busy;
                let path = self.projects.iter().find(|p| p.id() == s.project_id).map(|p| p.project.path.display().to_string()).unwrap_or_default();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .w(px(460.))
                    .child(div().text_size(px(17.)).font_weight(gpui::FontWeight::SEMIBOLD).child("Rename Project"))
                    .child(div().text_size(px(12.5)).text_color(c.muted).child("Sets [image].name, which also names the built image files."))
                    .child(field_row("Name", c).child(div().flex_1().child(s.name.clone())))
                    .child(
                        checkbox("rename-folder", s.rename_folder, "Rename the folder too", c).on_click(cx.listener(|this, _, _, cx| {
                            if let Some(Sheet::Rename(s)) = &mut this.sheet {
                                s.rename_folder = !s.rename_folder;
                            }
                            cx.notify();
                        })),
                    )
                    .child(div().text_size(px(11.5)).text_color(c.faint).font_family(MONO).child(path))
                    .children(s.error.clone().map(|e| div().text_color(c.danger).child(e)))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.))
                            .child(button("rename-cancel", "Cancel", Kind::Normal, !busy, c).on_click_if(!busy, cx.listener(|this, _, w, cx| this.close_sheet(w, cx))))
                            .child(
                                button("rename-save", if busy { "Renaming…" } else { "Rename" }, Kind::Primary, !busy, c)
                                    .on_click_if(!busy, cx.listener(|this, _, w, cx| this.save_rename(w, cx))),
                            ),
                    )
                    .into_any_element()
            }
            Sheet::Passwords(s) => {
                let mut col = div().flex().flex_col().gap(px(10.));
                for (u, a, b) in &s.fields {
                    col = col.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(u.clone()))
                            .child(field_row("Password", c).child(div().flex_1().child(a.clone())))
                            .child(field_row("Confirm", c).child(div().flex_1().child(b.clone()))),
                    );
                }
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .w(px(460.))
                    .child(div().text_size(px(17.)).font_weight(gpui::FontWeight::SEMIBOLD).child("Passwords"))
                    .child(div().text_size(px(12.5)).text_color(c.muted).child("Sent to the builder for this build only; never written to the recipe, the store, or the log."))
                    .child(col)
                    .children(s.error.clone().map(|e| div().text_color(c.danger).child(e)))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.))
                            .child(button("pw-cancel", "Cancel", Kind::Normal, true, c).on_click(cx.listener(|this, _, w, cx| this.close_sheet(w, cx))))
                            .child(button("pw-build", "Build", Kind::Primary, true, c).on_click(cx.listener(|this, _, w, cx| this.submit_passwords(w, cx)))),
                    )
                    .into_any_element()
            }
            Sheet::Onboarding { busy, lines, error } => {
                let busy = *busy;
                let host_err = self.host_ok.clone().err();
                let ok_host = host_err.is_none();
                let tail: Vec<String> = lines.iter().rev().take(12).rev().cloned().collect();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .w(px(560.))
                    .child(div().text_size(px(17.)).font_weight(gpui::FontWeight::SEMIBOLD).child("Set up the container runtime"))
                    .child(div().text_color(c.muted).child(
                        "ImageSmith runs every Linux operation inside Apple's container runtime. It needs a Mac with Apple silicon, macOS 26 or later, and the container tool installed and started.",
                    ))
                    .children(host_err.map(|e| div().text_color(c.danger).child(format!("{e} You can still edit recipes."))))
                    .child(div().text_size(px(12.5)).text_color(c.muted).child(
                        "Set Up builds the imagesmith-builder image and creates the store volume and container. The first run downloads base layers and takes a few minutes.",
                    ))
                    .when(!tail.is_empty(), |d| {
                        d.child(
                            div()
                                .p(px(8.))
                                .rounded(px(6.))
                                .bg(c.log_bg)
                                .border_1()
                                .border_color(c.border)
                                .font_family(MONO)
                                .text_size(px(11.5))
                                .children(tail.into_iter().map(|l| div().whitespace_nowrap().overflow_hidden().child(crate::widgets::strip_ansi(&l)))),
                        )
                    })
                    .children(error.clone().map(|e| div().text_color(c.danger).child(e)))
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(button("ob-install", "Get container…", Kind::Normal, true, c).on_click(|_, _, cx| cx.open_url(CONTAINER_URL)))
                            .child(div().flex_1())
                            .child(button("ob-later", "Not Now", Kind::Normal, !busy, c).on_click_if(!busy, cx.listener(|this, _, w, cx| this.close_sheet(w, cx))))
                            .child(button("ob-check", "Check Again", Kind::Normal, !busy && ok_host, c).on_click_if(!busy && ok_host, cx.listener(|this, _, _, cx| this.check_runtime(false, cx))))
                            .child(button("ob-setup", if busy { "Setting Up…" } else { "Set Up" }, Kind::Primary, !busy && ok_host, c).on_click_if(!busy && ok_host, cx.listener(|this, _, _, cx| this.setup_runtime(cx)))),
                    )
                    .into_any_element()
            }
        };
        let _ = window;
        let overlay = div().id("sheet-overlay").absolute().inset_0().bg(c.overlay).occlude();
        if large {
            return Some(
                overlay
                    .child(
                        div()
                            .id("sheet")
                            .absolute()
                            .top(px(24.))
                            .bottom(px(24.))
                            .left(px(28.))
                            .right(px(28.))
                            .p(px(16.))
                            .rounded(px(12.))
                            .bg(c.surface)
                            .border_1()
                            .border_color(c.border)
                            .shadow_2xl()
                            .child(body),
                    )
                    .into_any_element(),
            );
        }
        Some(
            overlay
                .flex()
                .justify_center()
                .items_start()
                .pt(px(70.))
                .child(div().id("sheet").p(px(20.)).rounded(px(12.)).bg(c.surface).border_1().border_color(c.border).shadow_2xl().child(body))
                .into_any_element(),
        )
    }
}
