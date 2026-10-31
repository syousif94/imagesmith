//! Model dropdown for the selected provider (OpenRouter or LM Studio), shared
//! by Settings and the assistant panel.

use crate::settings_window::update_settings;
use crate::text_input::{InputEvent, TextInput};
use crate::theme::{settings, Colors, MONO};
use crate::widgets::tooltip;
use gpui::prelude::*;
use gpui::{anchored, deferred, div, px, App, Context, Entity, Focusable, Global, SharedString, Subscription, Window};
use imagesmith_core::settings::{AiProvider, Settings};
use serde_json::Value;
use std::rc::Rc;

const MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

#[derive(Clone)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    /// Context size, price or load state.
    pub detail: String,
}

#[derive(Clone)]
pub enum Catalog {
    Idle,
    Loading,
    Ready(Rc<Vec<ModelInfo>>),
    Failed(String),
}

/// The loaded list and the provider/server it came from.
struct ModelCatalog(String, Catalog);
impl Global for ModelCatalog {}

fn source_key(s: &Settings) -> String {
    match s.ai_provider {
        AiProvider::OpenRouter => "openrouter".into(),
        AiProvider::LmStudio => format!("lmstudio:{}", s.lmstudio_base()),
    }
}

pub fn catalog(cx: &App) -> Catalog {
    let key = source_key(settings(cx));
    cx.try_global::<ModelCatalog>().filter(|c| c.0 == key).map(|c| c.1.clone()).unwrap_or(Catalog::Idle)
}

/// Fetches the model list once per provider (or again with `force`).
pub fn fetch_models(force: bool, cx: &mut App) {
    match catalog(cx) {
        Catalog::Loading => return,
        Catalog::Ready(_) if !force => return,
        _ => {}
    }
    let s = settings(cx).clone();
    let key = source_key(&s);
    cx.set_global(ModelCatalog(key.clone(), Catalog::Loading));
    let task = cx.background_executor().spawn(async move {
        match s.ai_provider {
            AiProvider::OpenRouter => load_openrouter(),
            AiProvider::LmStudio => load_lmstudio(&s.lmstudio_base(), s.lmstudio_api_key.as_deref()),
        }
    });
    cx.spawn(async move |cx| {
        let r = task.await;
        let _ = cx.update(|cx| {
            cx.set_global(ModelCatalog(
                key,
                match r {
                    Ok(m) => Catalog::Ready(Rc::new(m)),
                    Err(e) => Catalog::Failed(e),
                },
            ));
            cx.refresh_windows();
        });
    })
    .detach();
}

fn get_json(url: &str, token: Option<&str>) -> Result<Value, String> {
    use std::io::Write;
    let token = token.map(str::trim).filter(|t| !t.is_empty() && !t.chars().any(|c| c.is_control() || c.is_whitespace()));
    let mut cmd = std::process::Command::new("/usr/bin/curl");
    cmd.args(["-sS", "--fail", "--max-time", "30", url]);
    if token.is_some() {
        // Header on stdin keeps the token out of the process list.
        cmd.args(["-H", "@-"]);
    }
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if let (Some(t), Some(mut stdin)) = (token, child.stdin.take()) {
        let _ = writeln!(stdin, "Authorization: Bearer {t}");
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().trim_start_matches("curl: ").to_string());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
}

fn load_openrouter() -> Result<Vec<ModelInfo>, String> {
    get_json(MODELS_URL, None).map(|v| parse_models(&v)).map_err(|e| format!("Could not load models: {e}"))
}

/// LM Studio's native listing (0.4+), then the older v0 one, then the OpenAI-compatible one.
fn load_lmstudio(base: &str, token: Option<&str>) -> Result<Vec<ModelInfo>, String> {
    let mut last = String::new();
    for path in ["/api/v1/models", "/api/v0/models", "/v1/models"] {
        match get_json(&format!("{base}{path}"), token) {
            Ok(v) => return Ok(parse_lmstudio(&v)),
            Err(e) => last = e,
        }
    }
    Err(if last.contains("401") || last.contains("403") {
        format!("LM Studio at {base} requires an API token. Add it in Settings.")
    } else {
        format!("Could not reach LM Studio at {base}. Start its local server.")
    })
}

fn parse_lmstudio(v: &Value) -> Vec<ModelInfo> {
    // v1 lists under "models" with "key"; v0 and the OpenAI listing use "data" with "id".
    let list = v["models"].as_array().or_else(|| v["data"].as_array());
    let mut out: Vec<ModelInfo> = list
        .into_iter()
        .flatten()
        .filter(|m| match m["type"].as_str() {
            Some(t) => t == "llm" || t == "vlm",
            None => !m["id"].as_str().unwrap_or("").contains("embed"),
        })
        .filter_map(|m| {
            let id = m["key"].as_str().or_else(|| m["id"].as_str())?.to_string();
            let size = m["params_string"].as_str().map(String::from);
            let ctx = m["max_context_length"].as_u64().map(context_label);
            let state = match m["loaded_instances"].as_array() {
                Some(l) => Some(if l.is_empty() { "not loaded" } else { "loaded" }.to_string()),
                None => m["state"].as_str().map(|s| s.replace('-', " ")),
            };
            let tools = m.pointer("/capabilities/trained_for_tool_use").and_then(Value::as_bool).unwrap_or(false)
                || m["capabilities"].as_array().is_some_and(|c| c.iter().any(|x| x == "tool_use"));
            let detail = [size, ctx, state, tools.then(|| "tools".to_string())].into_iter().flatten().collect::<Vec<_>>().join(" · ");
            let name = m["display_name"].as_str().unwrap_or_default().to_string();
            Some(ModelInfo { id, name, detail })
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn context_label(n: u64) -> String {
    if n >= 1000 {
        format!("{}K", n / 1000)
    } else {
        n.to_string()
    }
}

/// Models that support tool calling, which the assistant needs.
fn parse_models(v: &Value) -> Vec<ModelInfo> {
    let mut out: Vec<ModelInfo> = v["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["supported_parameters"].as_array().is_some_and(|p| p.iter().any(|x| x == "tools")))
        .filter_map(|m| {
            let price = m.pointer("/pricing/prompt").and_then(Value::as_str).and_then(|s| s.parse::<f64>().ok()).map(|p| p * 1e6);
            Some(ModelInfo {
                id: m["id"].as_str()?.to_string(),
                name: m["name"].as_str().unwrap_or_default().to_string(),
                detail: price_label(m["context_length"].as_u64().unwrap_or(0), price),
            })
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

pub struct ModelPicker {
    open: bool,
    query: Entity<TextInput>,
    width: f32,
    /// Shows an OpenRouter / LM Studio switch atop the list.
    show_provider: bool,
    _sub: Subscription,
}

impl ModelPicker {
    pub fn new(width: f32, cx: &mut Context<Self>) -> ModelPicker {
        let query = cx.new(|cx| TextInput::new(cx, "Search models…"));
        let sub = cx.subscribe(&query, |this: &mut ModelPicker, input, e: &InputEvent, cx| {
            if matches!(e, InputEvent::Submit) {
                let q = input.read(cx).text();
                let first = this.filtered(&q, cx).first().map(|m| m.id.clone());
                if let Some(id) = first.or_else(|| (!q.trim().is_empty()).then(|| q.trim().to_string())) {
                    this.choose(id, cx);
                }
            }
            cx.notify();
        });
        ModelPicker { open: false, query, width, show_provider: false, _sub: sub }
    }

    pub fn with_provider_switch(mut self) -> Self {
        self.show_provider = true;
        self
    }

    fn filtered(&self, q: &str, cx: &App) -> Vec<ModelInfo> {
        let Catalog::Ready(models) = catalog(cx) else { return Vec::new() };
        let q = q.trim().to_lowercase();
        models.iter().filter(|m| q.is_empty() || m.id.to_lowercase().contains(&q) || m.name.to_lowercase().contains(&q)).cloned().collect()
    }

    fn choose(&mut self, id: String, cx: &mut Context<Self>) {
        self.open = false;
        update_settings(cx, |s| s.set_active_model(id));
        cx.notify();
    }

    fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = !self.open;
        if self.open {
            fetch_models(false, cx);
            self.query.update(cx, |i, cx| i.set_text("", cx));
            window.focus(&self.query.focus_handle(cx));
        }
        cx.notify();
    }
}

fn price_label(context: u64, price: Option<f64>) -> String {
    let ctx = context_label(context);
    match price {
        Some(p) if p == 0.0 => format!("{ctx} · free"),
        Some(p) => format!("{ctx} · ${p:.2}/M"),
        None => ctx,
    }
}

impl Render for ModelPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = Colors::get(window, cx);
        let lmstudio = settings(cx).ai_provider == AiProvider::LmStudio;
        let current = settings(cx).active_model().to_string();
        let label = match catalog(cx) {
            Catalog::Ready(m) => m.iter().find(|x| x.id == current).map(|x| x.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| current.clone()),
            _ => current.clone(),
        };
        let label = if label.is_empty() { "Choose a model".to_string() } else { label };
        let provider = if lmstudio { "LM Studio" } else { "OpenRouter" };
        let hover = c.hover;
        let trigger = div()
            .id("model-trigger")
            .flex()
            .items_center()
            .gap(px(6.))
            .h(px(24.))
            .px(px(8.))
            .min_w_0()
            .rounded(px(6.))
            .border_1()
            .border_color(c.border)
            .bg(c.input_bg)
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .tooltip(tooltip(format!("{provider} model: {current}")))
            .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.)).child(label))
            .child(div().flex_none().text_size(px(10.)).text_color(c.muted).child("▾"))
            .on_click(cx.listener(|this, _, w, cx| this.toggle(w, cx)));

        let mut root = div().flex().flex_col().min_w_0().child(trigger);
        if !self.open {
            return root;
        }
        let q = self.query.read(cx).text();
        let mut list = div().flex().flex_col().p(px(4.));
        match catalog(cx) {
            Catalog::Idle | Catalog::Loading => list = list.child(div().p(px(8.)).text_color(c.muted).text_size(px(12.)).child("Loading models…")),
            Catalog::Failed(e) => {
                list = list.child(div().p(px(8.)).text_color(c.danger).text_size(px(12.)).child(e)).child(
                    div()
                        .id("model-retry")
                        .p(px(8.))
                        .text_size(px(12.))
                        .text_color(c.accent)
                        .cursor_pointer()
                        .child("Try Again")
                        .on_click(|_, _, cx| fetch_models(true, cx)),
                )
            }
            Catalog::Ready(_) => {
                let models = self.filtered(&q, cx);
                if models.is_empty() {
                    list = list.child(div().p(px(8.)).text_color(c.muted).text_size(px(12.)).child(if lmstudio {
                        "No matching models. Download one in LM Studio."
                    } else {
                        "No matching models with tool support."
                    }));
                }
                let custom = q.trim().to_string();
                if !custom.is_empty() && (lmstudio || custom.contains('/')) && !models.iter().any(|m| m.id == custom) {
                    let id = custom.clone();
                    list = list.child(
                        div()
                            .id("model-custom")
                            .px(px(8.))
                            .py(px(5.))
                            .rounded(px(5.))
                            .cursor_pointer()
                            .hover(move |s| s.bg(hover))
                            .text_size(px(12.5))
                            .child(format!("Use “{custom}”"))
                            .on_click(cx.listener(move |this, _, _, cx| this.choose(id.clone(), cx))),
                    );
                }
                for (i, m) in models.into_iter().enumerate() {
                    let sel = m.id == current;
                    let id = m.id.clone();
                    let (accent, selected) = (c.accent, c.selected);
                    list = list.child(
                        div()
                            .id(("model", i))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(8.))
                            .py(px(4.))
                            .rounded(px(5.))
                            .cursor_pointer()
                            .when(sel, move |d| d.bg(selected))
                            .when(!sel, move |d| d.hover(move |s| s.bg(hover)))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().text_size(px(12.5)).overflow_hidden().whitespace_nowrap().text_ellipsis().child(if m.name.is_empty() { m.id.clone() } else { m.name.clone() }))
                                    .child(div().font_family(MONO).text_size(px(10.5)).text_color(c.muted).overflow_hidden().whitespace_nowrap().text_ellipsis().child(m.id.clone())),
                            )
                            .child(div().flex_none().text_size(px(10.5)).text_color(c.muted).child(m.detail.clone()))
                            .when(sel, move |d| d.child(div().flex_none().text_color(accent).child("✓")))
                            .on_click(cx.listener(move |this, _, _, cx| this.choose(id.clone(), cx))),
                    );
                }
            }
        }
        let count: SharedString = match catalog(cx) {
            Catalog::Ready(m) if lmstudio => format!("{} models in LM Studio", m.len()).into(),
            Catalog::Ready(m) => format!("{} models with tool support", m.len()).into(),
            _ => "".into(),
        };
        let popover = div()
            .id("model-popover")
            .occlude()
            .mt(px(4.))
            .w(px(self.width))
            .max_h(px(420.))
            .flex()
            .flex_col()
            .rounded(px(8.))
            .bg(c.surface)
            .border_1()
            .border_color(c.border)
            .shadow_lg()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.open = false;
                cx.notify();
            }))
            .when(self.show_provider, |d| {
                d.child(
                    div().flex().p(px(6.)).pb_0().child(
                        crate::widgets::segmented("picker-provider", &["OpenRouter", "LM Studio"], if lmstudio { 1 } else { 0 }, &c, |i, _, cx| {
                            update_settings(cx, |s| s.ai_provider = [AiProvider::OpenRouter, AiProvider::LmStudio][i]);
                            fetch_models(false, cx);
                        })
                        .flex_1(),
                    ),
                )
            })
            .child(div().p(px(6.)).border_b_1().border_color(c.border).child(self.query.clone()))
            .child(div().id("model-list").flex_1().min_h_0().overflow_y_scroll().child(list))
            .child(
                div()
                    .flex()
                    .items_center()
                    .px(px(10.))
                    .py(px(5.))
                    .border_t_1()
                    .border_color(c.border)
                    .text_size(px(11.))
                    .text_color(c.muted)
                    .child(div().flex_1().child(count))
                    .child(div().id("model-refresh").cursor_pointer().hover(|s| s.opacity(0.7)).child("Refresh").on_click(|_, _, cx| fetch_models(true, cx))),
            );
        root = root.child(deferred(anchored().snap_to_window_with_margin(px(8.)).child(popover)).with_priority(3));
        root
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn keeps_tool_models() {
        let v = serde_json::json!({ "data": [
            { "id": "b/x", "name": "X", "context_length": 128000, "pricing": { "prompt": "0.000003" }, "supported_parameters": ["tools", "temperature"] },
            { "id": "a/y", "name": "Y", "supported_parameters": ["temperature"] },
            { "id": "a/z", "name": "Z", "pricing": { "prompt": "0" }, "supported_parameters": ["tools"] },
        ]});
        let m = super::parse_models(&v);
        assert_eq!(m.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["a/z", "b/x"]);
        assert_eq!(m[1].detail, "128K · $3.00/M");
        assert_eq!(m[0].detail, "0 · free");

        let v = serde_json::json!({ "data": [
            { "id": "qwen3-8b", "type": "llm", "state": "not-loaded", "max_context_length": 32768, "capabilities": ["tool_use"] },
            { "id": "nomic-embed", "type": "embeddings" },
        ]});
        let m = super::parse_lmstudio(&v);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].detail, "32K · not loaded · tools");

        let v = serde_json::json!({ "models": [
            { "type": "llm", "key": "google/gemma-4-26b-a4b", "display_name": "Gemma 4 26B A4B", "params_string": "26B-A4B",
              "loaded_instances": [{ "id": "google/gemma-4-26b-a4b" }], "max_context_length": 262144,
              "capabilities": { "vision": true, "trained_for_tool_use": true } },
            { "type": "embedding", "key": "nomic", "loaded_instances": [] },
        ]});
        let m = super::parse_lmstudio(&v);
        assert_eq!(m.len(), 1);
        assert_eq!((m[0].id.as_str(), m[0].name.as_str()), ("google/gemma-4-26b-a4b", "Gemma 4 26B A4B"));
        assert_eq!(m[0].detail, "26B-A4B · 262K · loaded · tools");
    }
}
