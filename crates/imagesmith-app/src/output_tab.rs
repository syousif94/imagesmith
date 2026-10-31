//! Output tab: artifacts, summary, and changes vs. base / another build.

use crate::theme::{Colors, MONO};
use crate::widgets::{button, fmt_ms, Kind};
use crate::workspace::Workspace;
use gpui::prelude::*;
use gpui::{div, px, AnyElement, ClipboardItem, Context, SharedString};
use imagesmith_core::size::human;
use imagesmith_core::tracker::BuildState;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

#[derive(Default)]
pub struct OutputData {
    pub summary: Option<serde_json::Value>,
    pub base: Option<String>,
    pub builder: Option<String>,
    pub added: Vec<(String, String)>,
    pub removed: Vec<(String, String)>,
    pub upgraded: Vec<(String, String, String)>,
    pub etc: Vec<(String, String)>,
    pub compare: Option<(String, Vec<(String, String)>, Vec<(String, String)>, Vec<(String, String, String)>)>,
}

fn find_with_suffix(dir: &Path, suffix: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| p.to_string_lossy().ends_with(suffix))
}

fn read_pkgs(p: Option<PathBuf>) -> BTreeMap<String, String> {
    p.and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| {
            t.lines()
                .filter_map(|l| {
                    let mut it = l.split('\t');
                    Some((it.next()?.to_string(), it.next().unwrap_or("").to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

type PkgDiff = (Vec<(String, String)>, Vec<(String, String)>, Vec<(String, String, String)>);

fn pkg_diff(a: &BTreeMap<String, String>, b: &BTreeMap<String, String>) -> PkgDiff {
    let added = b.iter().filter(|(k, _)| !a.contains_key(*k)).map(|(k, v)| (k.clone(), v.clone())).collect();
    let removed = a.iter().filter(|(k, _)| !b.contains_key(*k)).map(|(k, v)| (k.clone(), v.clone())).collect();
    let upgraded = b
        .iter()
        .filter_map(|(k, v)| a.get(k).filter(|old| *old != v).map(|old| (k.clone(), old.clone(), v.clone())))
        .collect();
    (added, removed, upgraded)
}

pub fn load(dir: &Path, other: Option<&Path>) -> OutputData {
    let mut d = OutputData {
        summary: find_with_suffix(dir, ".summary.json").and_then(|p| serde_json::from_slice(&std::fs::read(p).ok()?).ok()),
        ..Default::default()
    };
    if let Some(lock) = std::fs::read(dir.join("recipe.lock.json")).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) {
        d.base = lock["recipe"]["image"]["base"].as_str().map(String::from);
        d.builder = lock["builder_version"].as_str().map(String::from);
    }
    let base = read_pkgs(find_with_suffix(dir, ".packages.base.tsv"));
    let post = read_pkgs(find_with_suffix(dir, ".packages.post.tsv"));
    (d.added, d.removed, d.upgraded) = pkg_diff(&base, &post);
    if let Some(v) = find_with_suffix(dir, ".etc-diff.json").and_then(|p| serde_json::from_slice::<serde_json::Value>(&std::fs::read(p).ok()?).ok()) {
        d.etc = v["changes"]
            .as_array()
            .map(|a| a.iter().map(|c| (c["change"].as_str().unwrap_or("").to_string(), c["path"].as_str().unwrap_or("").to_string())).collect())
            .unwrap_or_default();
    }
    if let Some(o) = other {
        let theirs = read_pkgs(find_with_suffix(o, ".packages.post.tsv"));
        let (a, r, u) = pkg_diff(&theirs, &post);
        d.compare = Some((o.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(), a, r, u));
    }
    d
}

impl Workspace {
    fn output_data(&mut self, dir: &Path) -> Rc<OutputData> {
        let other = self.compare_with.as_ref().and_then(|id| self.active().map(|p| p.project.builds_dir().join(id)));
        if let Some((d, o, data)) = &self.output_cache {
            if d == dir && *o == other {
                return data.clone();
            }
        }
        let data = Rc::new(load(dir, other.as_deref()));
        self.output_cache = Some((dir.to_path_buf(), other, data.clone()));
        data
    }

    fn group(&self, key: &str, title: String, rows: Vec<String>, c: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let open = self.output_open.contains(key);
        let k = key.to_string();
        let n = rows.len();
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .id(SharedString::from(format!("grp-{key}")))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .py(px(4.))
                    .cursor_pointer()
                    .child(div().w(px(12.)).text_size(px(10.)).text_color(c.muted).child(if open { "▾" } else { "▸" }))
                    .child(div().font_weight(gpui::FontWeight::SEMIBOLD).child(title))
                    .child(div().text_color(c.muted).child(format!("({n})")))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.output_open.remove(&k) {
                            this.output_open.insert(k.clone());
                        }
                        cx.notify();
                    })),
            )
            .when(open, |d| {
                d.child(
                    div()
                        .pl(px(18.))
                        .flex()
                        .flex_col()
                        .font_family(MONO)
                        .text_size(px(12.))
                        .children(rows.into_iter().take(2000).map(|r| div().whitespace_nowrap().child(r))),
                )
            })
            .into_any_element()
    }

    pub fn render_output_tab(&mut self, c: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let empty = |msg: &str, c: &Colors| div().size_full().flex().items_center().justify_center().text_color(c.muted).child(msg.to_string()).into_any_element();
        let Some(v) = self.current_view() else { return empty("No builds yet. Outputs appear here after a successful build.", c) };
        if v.live {
            return empty("Build in progress …", c);
        }
        let Some(dir) = v.dir.clone() else { return empty("This run produced no image (extract only or failed before Assemble).", c) };
        let build_id = v.build_id.clone().unwrap_or_default();
        let artifacts = v.tracker.artifacts.clone();
        let state = v.state;
        let duration = v.duration_ms.unwrap_or(0);
        let missing: Vec<_> = artifacts.iter().filter(|a| std::fs::metadata(dir.join(&a.path)).map(|m| m.len()).ok() != Some(a.bytes)).cloned().collect();
        if state != BuildState::Succeeded {
            let can_retry = !artifacts.is_empty() && !missing.is_empty();
            return div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(10.))
                .child(div().text_color(c.muted).child("Outputs appear here once a build succeeds."))
                .when(can_retry, |d| {
                    d.child(button("retry-export", "Retry Export", Kind::Primary, true, c).on_click(cx.listener(move |this, _, _, cx| this.retry_export(cx))))
                })
                .into_any_element();
        }
        let data = self.output_data(&dir);

        let mut rows: Vec<(String, String, u64, PathBuf)> =
            artifacts.iter().filter(|a| a.kind != "manifest").map(|a| (a.kind.clone(), a.path.clone(), a.bytes, dir.join(&a.path))).collect();
        for (kind, name) in [("lock", "recipe.lock.json"), ("log", "build.log")] {
            let p = dir.join(name);
            if let Ok(m) = std::fs::metadata(&p) {
                rows.push((kind.into(), name.into(), m.len(), p));
            }
        }
        let mut list = div().flex().flex_col().rounded(px(8.)).border_1().border_color(c.border).overflow_hidden();
        for (i, (kind, name, bytes, path)) in rows.into_iter().enumerate() {
            let missing = !path.exists();
            let p1 = path.clone();
            let p2 = path.clone();
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(12.))
                    .h(px(36.))
                    .when(i > 0, |d| d.border_t_1().border_color(c.border))
                    .child(div().w(px(70.)).text_size(px(11.5)).text_color(c.muted).child(kind.to_uppercase()))
                    .child(div().flex_1().font_family(MONO).text_size(px(12.)).overflow_hidden().whitespace_nowrap().text_ellipsis().child(name))
                    .child(div().w(px(80.)).text_size(px(12.)).text_color(if missing { c.danger } else { c.muted }).child(if missing { "missing".into() } else { human(bytes) }))
                    .child(
                        div()
                            .w(px(330.))
                            .flex()
                            .justify_end()
                            .gap(px(8.))
                            .when(kind == "checksum", |d| {
                                let p = path.clone();
                                d.child(button(("copy-hash", i), "Copy Hash", Kind::Normal, !missing, c).on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(h) = std::fs::read_to_string(&p).ok().and_then(|t| t.split_whitespace().next().map(String::from)) {
                                        cx.write_to_clipboard(ClipboardItem::new_string(h));
                                        this.toast("Hash copied", cx);
                                    }
                                })))
                            })
                            .when(kind == "image", |d| {
                                let p = path.clone();
                                d.child(button(("flash-art", i), "Write to SD Card…", Kind::Primary, !missing, c).on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_flash(p.clone(), cx);
                                })))
                            })
                            .child(button(("reveal-art", i), "Reveal", Kind::Normal, !missing, c).on_click(cx.listener(move |_, _, _, cx| cx.reveal_path(&p1))))
                            .child(button(("copy-path", i), "Copy Path", Kind::Normal, true, c).on_click(cx.listener(move |this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(p2.to_string_lossy().into_owned()));
                                this.toast("Path copied", cx);
                            }))),
                    ),
            );
        }

        let s = data.summary.clone().unwrap_or_default();
        let image_compressed = artifacts.iter().find(|a| a.kind == "image").map(|a| a.bytes);
        let n = |k: &str| s[k].as_u64();
        let kv = |k: &str, v: String| div().flex().gap(px(10.)).child(div().w(px(170.)).text_color(c.muted).child(k.to_string())).child(v);
        let summary = div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .text_size(px(12.5))
            .child(kv("Image (compressed)", image_compressed.map(human).unwrap_or_else(|| "-".into())))
            .child(kv("Image (uncompressed)", n("image_bytes").map(human).unwrap_or_else(|| "-".into())))
            .child(kv(
                "Rootfs used / free",
                match (n("rootfs_used"), n("rootfs_bytes")) {
                    (Some(u), Some(t)) => format!("{} / {}", human(u), human(t.saturating_sub(u))),
                    _ => "-".into(),
                },
            ))
            .child(kv("Packages", n("packages").map(|p| p.to_string()).unwrap_or_else(|| "-".into())))
            .child(kv("Build duration", fmt_ms(duration)))
            .child(kv("Base image", data.base.clone().unwrap_or_else(|| "-".into())))
            .child(kv("Builder version", data.builder.clone().or_else(|| s["builder_version"].as_str().map(String::from)).unwrap_or_else(|| "-".into())));

        let others: Vec<String> =
            self.active().map(|p| p.builds.iter().filter(|b| b.record.id != build_id && b.record.result == BuildState::Succeeded).map(|b| b.record.id.clone()).collect()).unwrap_or_default();
        let mut picker = div().flex().flex_wrap().gap(px(6.)).items_center().child(div().text_color(c.muted).child("Compare with build:"));
        for (i, o) in others.iter().take(12).enumerate() {
            let sel = self.compare_with.as_deref() == Some(o.as_str());
            let id = o.clone();
            picker = picker.child(button(("cmp", i), o.clone(), if sel { Kind::Primary } else { Kind::Normal }, true, c).on_click(cx.listener(move |this, _, _, cx| {
                this.compare_with = if this.compare_with.as_deref() == Some(id.as_str()) { None } else { Some(id.clone()) };
                cx.notify();
            })));
        }
        if others.is_empty() {
            picker = picker.child(div().text_color(c.faint).child("no other successful builds"));
        }

        let fmt2 = |v: &[(String, String)]| v.iter().map(|(a, b)| format!("{a}  {b}")).collect::<Vec<_>>();
        let fmt3 = |v: &[(String, String, String)]| v.iter().map(|(a, b, x)| format!("{a}  {b} → {x}")).collect::<Vec<_>>();
        let etc_rows = |kind: &str| data.etc.iter().filter(|(c, _)| c == kind).map(|(_, p)| p.clone()).collect::<Vec<_>>();
        let changes = div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(self.group("pk-added", "Packages added".into(), fmt2(&data.added), c, cx))
            .child(self.group("pk-removed", "Packages removed".into(), fmt2(&data.removed), c, cx))
            .child(self.group("pk-upgraded", "Packages upgraded".into(), fmt3(&data.upgraded), c, cx))
            .child(self.group("etc-added", "/etc files added".into(), etc_rows("added"), c, cx))
            .child(self.group("etc-modified", "/etc files changed".into(), etc_rows("modified"), c, cx))
            .child(self.group("etc-removed", "/etc files removed".into(), etc_rows("removed"), c, cx));
        let compare = data.compare.as_ref().map(|(other, a, r, u)| {
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(div().text_size(px(12.)).text_color(c.muted).child(format!("Packages in this build vs. {other}")))
                .child(self.group("cmp-added", "Added".into(), fmt2(a), c, cx))
                .child(self.group("cmp-removed", "Removed".into(), fmt2(r), c, cx))
                .child(self.group("cmp-upgraded", "Changed version".into(), fmt3(u), c, cx))
        });

        let section = |t: &str| div().mt(px(8.)).text_size(px(15.)).font_weight(gpui::FontWeight::SEMIBOLD).child(t.to_string());
        div()
            .id("output")
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .p(px(20.))
                    .max_w(px(920.))
                    .child(section("Artifacts"))
                    .child(list)
                    .child(section("Summary"))
                    .child(summary)
                    .child(section("Changes vs. base"))
                    .child(changes)
                    .child(section("Compare with build…"))
                    .child(picker)
                    .children(compare),
            )
            .into_any_element()
    }

    fn retry_export(&mut self, cx: &mut Context<Self>) {
        let Some(p) = self.active() else { return };
        let Some(id) = self.current_view().and_then(|v| v.build_id.clone()) else { return };
        let project = imagesmith_core::project::Project { path: p.project.path.clone(), id: p.project.id.clone() };
        let rt = crate::workspace::runtime(cx);
        self.toast("Retrying export …", cx);
        let task = cx.background_spawn(async move {
            rt.ensure_store(&mut |_| {})?;
            imagesmith_core::build::retry_export(&rt, &project, &id)?;
            let rec = project.builds_dir().join(&id).join("build.json");
            if let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&std::fs::read(&rec)?) {
                v["result"] = serde_json::json!(BuildState::Succeeded);
                std::fs::write(&rec, serde_json::to_vec_pretty(&v)?)?;
            }
            Ok::<_, imagesmith_core::Error>(())
        });
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |this, cx| {
                match r {
                    Ok(()) => {
                        if let Some(v) = this.current_view_mut() {
                            v.state = BuildState::Succeeded;
                        }
                        if let Some(p) = this.active_mut() {
                            p.builds = p.project.builds();
                        }
                        this.output_cache = None;
                        this.toast("Export complete", cx)
                    }
                    Err(e) => this.toast(format!("Export failed: {e}"), cx),
                }
                cx.notify();
            });
        })
        .detach();
    }
}
