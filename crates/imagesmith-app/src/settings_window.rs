//! Settings window.

use crate::text_input::{InputEvent, TextInput};
use crate::theme::{settings, AppSettings, Colors, MONO, UI_FONT};
use crate::widgets::{button, checkbox, segmented, stepper, ClickIf, Kind};
use crate::workspace::runtime;
use gpui::prelude::*;
use gpui::{div, px, App, Context, Entity, PathPromptOptions, PromptLevel, SharedString, Subscription, Window};
use imagesmith_core::recipe::{Compress, SizeMode};
use imagesmith_core::settings::{AiProvider, Settings, Theme, WebSearch, DEFAULT_LMSTUDIO_URL};
use imagesmith_core::size::human;
use imagesmith_core::store::{self, Usage};

#[derive(Clone, Copy, PartialEq)]
enum Pane {
    Runtime,
    Resources,
    Caches,
    Defaults,
    Appearance,
    Assistant,
}

pub struct SettingsView {
    pane: Pane,
    usage: Option<Result<Usage, String>>,
    busy: Option<String>,
    api_key: Entity<TextInput>,
    lmstudio_url: Entity<TextInput>,
    lmstudio_key: Entity<TextInput>,
    langsearch_key: Entity<TextInput>,
    model: Entity<crate::model_picker::ModelPicker>,
    _subs: Vec<Subscription>,
}

pub fn update_settings(cx: &mut App, f: impl FnOnce(&mut Settings)) {
    let mut s = settings(cx).clone();
    f(&mut s);
    let _ = s.save();
    cx.set_global(AppSettings(s));
    cx.refresh_windows();
}

impl SettingsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> SettingsView {
        cx.observe_window_appearance(window, |_, _, cx| cx.notify()).detach();
        let s = settings(cx).clone();
        let api_key = cx.new(|cx| {
            let mut t = TextInput::new(cx, "sk-or-…").masked().mono();
            t.set_text(s.openrouter_api_key.clone().unwrap_or_default(), cx);
            t
        });
        let model = cx.new(|cx| crate::model_picker::ModelPicker::new(380., cx));
        let lmstudio_url = cx.new(|cx| {
            let mut t = TextInput::new(cx, DEFAULT_LMSTUDIO_URL).mono();
            t.set_text(s.lmstudio_url.clone(), cx);
            t
        });
        let lmstudio_key = cx.new(|cx| {
            let mut t = TextInput::new(cx, "Optional").masked().mono();
            t.set_text(s.lmstudio_api_key.clone().unwrap_or_default(), cx);
            t
        });
        let langsearch_key = cx.new(|cx| {
            let mut t = TextInput::new(cx, "sk-…").masked().mono();
            t.set_text(s.langsearch_api_key.clone().unwrap_or_default(), cx);
            t
        });
        let subs = vec![
            cx.subscribe(&langsearch_key, |_, input, _: &InputEvent, cx| {
                let k = input.read(cx).text().trim().to_string();
                update_settings(cx, |s| s.langsearch_api_key = (!k.is_empty()).then_some(k));
            }),
            cx.subscribe(&lmstudio_key, |_, input, _: &InputEvent, cx| {
                let k = input.read(cx).text().trim().to_string();
                update_settings(cx, |s| s.lmstudio_api_key = (!k.is_empty()).then_some(k));
            }),
            cx.subscribe(&api_key, |_, input, _: &InputEvent, cx| {
                let k = input.read(cx).text().trim().to_string();
                update_settings(cx, |s| s.openrouter_api_key = (!k.is_empty()).then_some(k));
            }),
            cx.subscribe(&lmstudio_url, |_, input, _: &InputEvent, cx| {
                let u = input.read(cx).text().trim().to_string();
                update_settings(cx, |s| s.lmstudio_url = u);
            }),
        ];
        SettingsView { pane: Pane::Runtime, usage: None, busy: None, api_key, lmstudio_url, lmstudio_key, langsearch_key, model, _subs: subs }
    }

    fn load_usage(&mut self, cx: &mut Context<Self>) {
        let rt = runtime(cx);
        self.busy = Some("Reading store usage …".into());
        let task = cx.background_spawn(async move {
            rt.ensure_store(&mut |_| {}).map_err(|e| e.to_string())?;
            store::usage(&rt).map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |this, cx| {
                this.usage = Some(r);
                this.busy = None;
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn run_bg(&mut self, label: &str, f: impl FnOnce(&imagesmith_core::runner::Runtime) -> imagesmith_core::Result<()> + Send + 'static, cx: &mut Context<Self>) {
        let rt = runtime(cx);
        self.busy = Some(label.to_string());
        let task = cx.background_spawn(async move { f(&rt).map_err(|e| e.to_string()) });
        cx.spawn(async move |this, cx| {
            let r = task.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = r.err().map(|e| format!("Failed: {e}"));
                if this.pane == Pane::Caches {
                    this.load_usage(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = Colors::get(window, cx);
        let s = settings(cx).clone();
        let mut nav = div().flex().flex_col().gap(px(2.)).p(px(10.)).w(px(170.)).h_full().bg(c.sidebar).border_r_1().border_color(c.border).pt(px(40.));
        for (p, label) in [(Pane::Runtime, "Runtime"), (Pane::Resources, "Resources"), (Pane::Caches, "Caches"), (Pane::Defaults, "Defaults"), (Pane::Appearance, "Appearance"), (Pane::Assistant, "Assistant")] {
            let sel = self.pane == p;
            nav = nav.child(
                div()
                    .id(label)
                    .px(px(10.))
                    .h(px(28.))
                    .flex()
                    .items_center()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .when(sel, |d| d.bg(c.selected).font_weight(gpui::FontWeight::SEMIBOLD))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.pane = p;
                        if p == Pane::Caches && this.usage.is_none() {
                            this.load_usage(cx);
                        }
                        cx.notify();
                    })),
            );
        }
        // Wraps so controls drop below the label instead of overflowing a narrow window.
        let row = |label: &str| {
            div().flex().flex_wrap().items_center().gap(px(12.)).min_w_0().child(div().w(px(160.)).flex_none().text_color(c.muted).child(label.to_string()))
        };
        let busy = self.busy.is_some();
        let host_cpus = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4);
        let body = match self.pane {
            Pane::Runtime => {
                let rt = runtime(cx);
                div()
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .child(
                        row("container binary")
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(160.))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .font_family(MONO)
                                    .text_size(px(12.))
                                    .child(rt.bin.display().to_string()),
                            )
                            .child(button("bin-choose", "Choose…", Kind::Normal, true, &c).on_click(cx.listener(|_, _, _, cx| {
                                let rx = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Choose container binary".into()) });
                                cx.spawn(async move |_, cx| {
                                    if let Ok(Ok(Some(p))) = rx.await {
                                        let _ = cx.update(|cx| update_settings(cx, |s| s.container_bin = p.into_iter().next()));
                                    }
                                })
                                .detach();
                            })))
                            .when(s.container_bin.is_some(), |d| {
                                d.child(button("bin-auto", "Auto-detect", Kind::Ghost, true, &c).on_click(|_, _, cx| update_settings(cx, |s| s.container_bin = None)))
                            }),
                    )
                    .child(
                        row("Builder image")
                            .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().font_family(MONO).text_size(px(12.)).child(rt.builder_image()))
                            .child(button("rebuild", "Rebuild", Kind::Normal, !busy, &c).on_click_if(!busy, cx.listener(|this, _, _, cx| {
                                this.run_bg(
                                    "Rebuilding builder image …",
                                    |rt| {
                                        rt.build_builder_image(&mut |_| {})?;
                                        rt.delete_store()?;
                                        rt.ensure_store(&mut |_| {}).map(|_| ())
                                    },
                                    cx,
                                )
                            }))),
                    )
                    .child(row("").child(checkbox("start-launch", s.start_runtime_at_launch, "Start runtime at launch", &c).on_click(|_, _, cx| {
                        update_settings(cx, |s| s.start_runtime_at_launch = !s.start_runtime_at_launch)
                    })))
                    .into_any_element()
            }
            Pane::Resources => div()
                .flex()
                .flex_col()
                .gap(px(14.))
                .child(row("CPUs").child(stepper("cpus", s.cpus.to_string(), &c, move |d, _, cx| {
                    update_settings(cx, |s| s.cpus = (s.cpus as i64 + d).clamp(1, host_cpus as i64) as u32)
                })).child(div().text_color(c.faint).child(format!("of {host_cpus}"))))
                .child(row("Memory").child(stepper("mem", format!("{} GB", s.memory_gb), &c, |d, _, cx| {
                    update_settings(cx, |s| s.memory_gb = (s.memory_gb as i64 + d).clamp(2, 16) as u32)
                })))
                .child(div().text_size(px(12.)).text_color(c.muted).child("Changes apply when the store container is recreated, which happens automatically before the next build."))
                .into_any_element(),
            Pane::Caches => {
                let mut list = div().flex().flex_col().gap(px(2.));
                match &self.usage {
                    None => list = list.child(div().text_color(c.muted).child("Not loaded")),
                    Some(Err(e)) => list = list.child(div().text_color(c.danger).child(e.clone())),
                    Some(Ok(u)) => {
                        list = list.child(div().pb(px(6.)).child(format!("Store volume: {} used of {} ({} free)", human(u.used), human(u.total), human(u.avail))));
                        let mut entries = u.entries.clone();
                        entries.sort_by(|a, b| (a.kind.as_str(), b.last_used).cmp(&(b.kind.as_str(), a.last_used)));
                        for (i, e) in entries.into_iter().enumerate() {
                            let (kind, key) = (e.kind.clone(), e.key.clone());
                            list = list.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .h(px(26.))
                                    .child(div().w(px(60.)).text_color(c.muted).child(e.kind.clone()))
                                    .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().font_family(MONO).text_size(px(11.5)).child(e.key.clone()))
                                    .child(div().w(px(80.)).child(human(e.bytes)))
                                    .child(button(("evict", i), "Delete", Kind::Danger, !busy, &c).on_click_if(!busy, cx.listener(move |this, _, _, cx| {
                                        let (k, key) = (kind.clone(), key.clone());
                                        this.run_bg("Deleting …", move |rt| store::evict(rt, &k, &key), cx)
                                    }))),
                            );
                        }
                    }
                }
                div()
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .child(row("Cache cap (LRU)").child(stepper("cap", format!("{} GB", s.cache_cap_gb), &c, |d, _, cx| {
                        update_settings(cx, |s| s.cache_cap_gb = (s.cache_cap_gb as i64 + d * 5).clamp(5, 2000) as u64)
                    })))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(8.))
                            .child(button("reload-usage", "Refresh", Kind::Normal, !busy, &c).on_click_if(!busy, cx.listener(|this, _, _, cx| this.load_usage(cx))))
                            .child(button("recreate", "Recreate Store Container", Kind::Normal, !busy, &c).on_click_if(!busy, cx.listener(|this, _, _, cx| {
                                this.run_bg("Recreating store container …", |rt| {
                                    rt.delete_store()?;
                                    rt.ensure_store(&mut |_| {}).map(|_| ())
                                }, cx)
                            })))
                            .child(button("clear-all", "Clear All…", Kind::Danger, !busy, &c).on_click_if(!busy, cx.listener(|_, _, w, cx| {
                                let rx = w.prompt(
                                    PromptLevel::Critical,
                                    "Delete the store volume?",
                                    Some("All imported bases, rootfs caches, apt caches and project stage directories are deleted. Project folders and exported builds are untouched."),
                                    &["Delete Everything", "Cancel"],
                                    cx,
                                );
                                cx.spawn(async move |this, cx| {
                                    if rx.await.ok() == Some(0) {
                                        let _ = this.update(cx, |this, cx| this.run_bg("Deleting store volume …", |rt| rt.delete_volume(), cx));
                                    }
                                })
                                .detach();
                            }))),
                    )
                    .child(div().id("usage").max_h(px(360.)).overflow_y_scroll().child(list))
                    .into_any_element()
            }
            Pane::Defaults => {
                let comp = match s.compress {
                    Compress::None => 0,
                    Compress::Zstd => 1,
                    Compress::Xz => 2,
                };
                let level = s.level.unwrap_or(if s.compress == Compress::Xz { 6 } else { 19 });
                let size = match s.size_mode {
                    SizeMode::Shrink => 0,
                    SizeMode::Fixed => 1,
                    SizeMode::Grow => 2,
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .child(div().text_size(px(12.)).text_color(c.muted).child("Used for the starter recipe of new projects."))
                    .child(row("Compression").child(segmented("d-comp", &["None", "zstd", "xz"], comp, &c, |i, _, cx| {
                        update_settings(cx, |s| {
                            s.compress = [Compress::None, Compress::Zstd, Compress::Xz][i];
                            s.level = None;
                        })
                    })))
                    .child(row("Level").child(stepper("d-level", level.to_string(), &c, move |d, _, cx| {
                        update_settings(cx, |s| {
                            let max = if s.compress == Compress::Xz { 9 } else { 19 };
                            s.level = Some((level + d as i32).clamp(1, max));
                        })
                    })))
                    .child(row("").child(checkbox("d-bmap", s.bmap, "Write .bmap", &c).on_click(|_, _, cx| update_settings(cx, |s| s.bmap = !s.bmap))))
                    .child(row("").child(checkbox("d-sum", s.checksum, "SHA-256 checksum", &c).on_click(|_, _, cx| update_settings(cx, |s| s.checksum = !s.checksum))))
                    .child(row("Size mode").child(segmented("d-size", &["Shrink", "Fixed", "Grow"], size, &c, |i, _, cx| {
                        update_settings(cx, |s| s.size_mode = [SizeMode::Shrink, SizeMode::Fixed, SizeMode::Grow][i])
                    })))
                    .into_any_element()
            }
            Pane::Appearance => {
                let t = match s.theme {
                    Theme::System => 0,
                    Theme::Light => 1,
                    Theme::Dark => 2,
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .child(row("Theme").child(segmented("theme", &["System", "Light", "Dark"], t, &c, |i, _, cx| {
                        update_settings(cx, |s| s.theme = [Theme::System, Theme::Light, Theme::Dark][i])
                    })))
                    .into_any_element()
            }
            Pane::Assistant => {
                let lm = s.ai_provider == AiProvider::LmStudio;
                div()
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .child(div().text_size(px(12.)).text_color(c.muted).child(if lm {
                        "The assistant runs on a model served by LM Studio on this Mac or your network. Start its local server from LM Studio's Developer tab."
                    } else {
                        "The assistant sends the active project's recipe.toml and your messages to OpenRouter. The key is stored in ImageSmith's settings file, readable only by you."
                    }))
                    .child(row("Provider").child(segmented("ai-provider", &["OpenRouter", "LM Studio"], if lm { 1 } else { 0 }, &c, |i, _, cx| {
                        update_settings(cx, |s| s.ai_provider = [AiProvider::OpenRouter, AiProvider::LmStudio][i]);
                        crate::model_picker::fetch_models(false, cx);
                    })))
                    .when(!lm, |d| {
                        d.child(
                            row("OpenRouter API key")
                                .child(div().flex_1().min_w(px(200.)).child(self.api_key.clone()))
                                .child(button("get-key", "Get a Key…", Kind::Normal, true, &c).on_click(|_, _, cx| cx.open_url("https://openrouter.ai/keys"))),
                        )
                    })
                    .when(lm, |d| {
                        d.child(
                            row("Server")
                                .child(div().flex_1().min_w(px(200.)).child(self.lmstudio_url.clone()))
                                .child(button("lm-refresh", "Connect", Kind::Normal, true, &c).on_click(|_, _, cx| crate::model_picker::fetch_models(true, cx))),
                        )
                        .child(row("API token").child(div().flex_1().min_w(px(200.)).child(self.lmstudio_key.clone())))
                    })
                    .child(row("Model").child(div().flex_1().min_w(px(200.)).child(self.model.clone())))
                    .child(row("").child(div().text_size(px(12.)).text_color(c.muted).child(if lm {
                        "Pick a model that supports tool use; LM Studio loads it on first request."
                    } else {
                        "Only models that support tool calling are listed. Type a full model ID to use one that isn't."
                    })))
                    .child(div().h(px(1.)).bg(c.border))
                    .child(row("Web search").child(segmented(
                        "web-search",
                        &["Off", "LangSearch", "OpenRouter"],
                        match s.web_search {
                            WebSearch::Off => 0,
                            WebSearch::LangSearch => 1,
                            WebSearch::OpenRouter => 2,
                        },
                        &c,
                        |i, _, cx| update_settings(cx, |s| s.web_search = [WebSearch::Off, WebSearch::LangSearch, WebSearch::OpenRouter][i]),
                    )))
                    .when(s.web_search == WebSearch::LangSearch, |d| {
                        d.child(
                            row("LangSearch API key")
                                .child(div().flex_1().min_w(px(200.)).child(self.langsearch_key.clone()))
                                .child(button("ls-key", "Get a Key…", Kind::Normal, true, &c).on_click(|_, _, cx| cx.open_url("https://langsearch.com/dashboard"))),
                        )
                    })
                    .when(s.web_search == WebSearch::OpenRouter && lm, |d| {
                        d.child(
                            row("OpenRouter API key")
                                .child(div().flex_1().min_w(px(200.)).child(self.api_key.clone()))
                                .child(button("get-key", "Get a Key…", Kind::Normal, true, &c).on_click(|_, _, cx| cx.open_url("https://openrouter.ai/keys"))),
                        )
                    })
                    .child(row("").child(div().text_size(px(12.)).text_color(c.muted).child(match s.web_search {
                        WebSearch::Off => "Lets the assistant look up things like a vendor's repository and signing key. Works with OpenRouter and local models.".to_string(),
                        WebSearch::LangSearch => "Searches run through LangSearch's free web search API with your key.".to_string(),
                        WebSearch::OpenRouter => format!(
                            "Searches run through OpenRouter's web search tool (Exa) with {}, billed to your OpenRouter credits.",
                            if s.ai_model.trim().is_empty() { imagesmith_core::settings::DEFAULT_AI_MODEL } else { s.ai_model.trim() }
                        ),
                    })))
                    .into_any_element()
            }
        };
        let title: SharedString = match self.pane {
            Pane::Runtime => "Runtime",
            Pane::Resources => "Resources",
            Pane::Caches => "Caches",
            Pane::Defaults => "Defaults",
            Pane::Appearance => "Appearance",
            Pane::Assistant => "Assistant",
        }
        .into();
        div()
            .size_full()
            .flex()
            .bg(c.bg)
            .text_color(c.text)
            .font_family(UI_FONT)
            .text_size(px(13.))
            .child(nav)
            .child(
                div()
                    .id("settings-body")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .overflow_y_scroll()
                    .gap(px(16.))
                    .p(px(24.))
                    .pt(px(40.))
                    .child(div().text_size(px(18.)).font_weight(gpui::FontWeight::SEMIBOLD).child(title))
                    .child(body)
                    .children(self.busy.clone().map(|b| div().text_size(px(12.)).text_color(if b.starts_with("Failed") { c.danger } else { c.muted }).child(b))),
            )
    }
}
