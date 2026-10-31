//! Small stateless building blocks shared by every view.

use crate::theme::{Colors, MONO};
use gpui::prelude::*;
use gpui::{div, px, AnyView, App, ClickEvent, Div, ElementId, Hsla, SharedString, Stateful, Window};
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Primary,
    Normal,
    Ghost,
    Danger,
}

pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>, kind: Kind, enabled: bool, c: &Colors) -> Stateful<Div> {
    let (bg, fg, border) = match kind {
        Kind::Primary => (c.accent, c.accent_text, c.accent),
        Kind::Normal => (c.surface, c.text, c.border),
        Kind::Ghost => (gpui::transparent_black(), c.text, gpui::transparent_black()),
        Kind::Danger => (c.surface, c.danger, c.border),
    };
    let hover = c.hover;
    div()
        .id(id.into())
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(26.))
        .px(px(10.))
        .rounded(px(6.))
        .border_1()
        .border_color(border)
        .bg(bg)
        .text_color(fg)
        .text_size(px(12.5))
        .child(label.into())
        .when(!enabled, |d| d.opacity(0.45))
        .when(enabled, move |d| {
            d.cursor_pointer().hover(move |s| if kind == Kind::Primary { s.opacity(0.9) } else { s.bg(hover) })
        })
}

/// `on_click` only when enabled, so disabled buttons are inert.
pub trait ClickIf: Sized {
    fn on_click_if(self, enabled: bool, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self;
}

impl ClickIf for Stateful<Div> {
    fn on_click_if(self, enabled: bool, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        if enabled {
            self.on_click(f)
        } else {
            self
        }
    }
}

pub fn checkbox(id: impl Into<ElementId>, checked: bool, label: impl Into<SharedString>, c: &Colors) -> Stateful<Div> {
    div()
        .id(id.into())
        .flex()
        .items_center()
        .gap(px(6.))
        .cursor_pointer()
        .child(
            div()
                .size(px(14.))
                .rounded(px(3.))
                .border_1()
                .border_color(if checked { c.accent } else { c.border })
                .bg(if checked { c.accent } else { c.input_bg })
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(10.))
                .text_color(c.accent_text)
                .when(checked, |d| d.child("✓")),
        )
        .child(div().text_size(px(13.)).child(label.into()))
}

pub fn segmented(
    id: impl Into<SharedString>,
    options: &[&str],
    selected: usize,
    c: &Colors,
    on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> Div {
    let id: SharedString = id.into();
    let on_select = Rc::new(on_select);
    let mut row = div().flex().flex_none().p(px(2.)).gap(px(2.)).rounded(px(7.)).bg(c.track);
    for (i, label) in options.iter().enumerate() {
        let f = on_select.clone();
        let sel = i == selected;
        let hover = c.hover;
        row = row.child(
            div()
                .id(ElementId::Name(format!("{id}-{i}").into()))
                .px(px(12.))
                .h(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .text_size(px(12.5))
                .when(sel, |d| d.bg(if c.dark { c.surface_alt } else { c.group }).shadow_sm().font_weight(gpui::FontWeight::MEDIUM))
                .when(!sel, move |d| d.text_color(c.muted).cursor_pointer().hover(move |s| s.bg(hover)))
                .child(label.to_string())
                .on_click(move |_, w, cx| f(i, w, cx)),
        );
    }
    row
}

/// macOS-style on/off switch.
pub fn switch(id: impl Into<ElementId>, on: bool, c: &Colors) -> Stateful<Div> {
    div()
        .id(id.into())
        .flex()
        .flex_none()
        .w(px(34.))
        .h(px(20.))
        .p(px(2.))
        .rounded(px(10.))
        .cursor_pointer()
        .bg(if on { c.accent } else { c.border })
        .when(on, |d| d.justify_end())
        .child(div().size(px(16.)).rounded_full().bg(gpui::white()).shadow_sm())
}

/// A rounded inset group of rows separated by hairlines, as in System Settings.
pub fn group(rows: impl IntoIterator<Item = gpui::AnyElement>, c: &Colors) -> Div {
    let mut g = div().flex().flex_col().w_full().rounded(px(10.)).border_1().border_color(c.separator).bg(c.group).overflow_hidden();
    for (i, r) in rows.into_iter().enumerate() {
        g = g.child(div().when(i > 0, |d| d.border_t_1().border_color(c.separator)).child(r));
    }
    g
}

/// A row with a label (and optional explanation) on the left; append controls with `.child`.
pub fn setting_row(label: impl Into<SharedString>, detail: Option<SharedString>, c: &Colors) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(12.))
        .px(px(14.))
        .py(px(9.))
        .min_h(px(44.))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(140.))
                .gap(px(1.))
                .child(div().text_size(px(13.)).child(label.into()))
                .children(detail.map(|d| div().text_size(px(11.5)).text_color(c.muted).child(d))),
        )
}

pub fn pill(text: impl Into<SharedString>, fg: Hsla, bg: Hsla) -> Div {
    div()
        .flex_none()
        .px(px(8.))
        .py(px(1.))
        .rounded(px(10.))
        .border_1()
        .border_color(fg.opacity(0.5))
        .bg(bg)
        .text_color(fg)
        .text_size(px(11.5))
        .child(text.into())
}

pub fn token(id: impl Into<ElementId>, text: impl Into<SharedString>, c: &Colors, on_remove: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Div {
    let hover = c.danger;
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(2.))
        .pl(px(10.))
        .pr(px(4.))
        .h(px(24.))
        .rounded(px(12.))
        .bg(c.track)
        .text_size(px(12.5))
        .child(text.into())
        .child(
            div()
                .id(id.into())
                .size(px(16.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .text_size(px(12.))
                .text_color(c.muted)
                .cursor_pointer()
                .hover(move |s| s.text_color(hover))
                .tooltip(tooltip("Remove"))
                .child("×")
                .on_click(on_remove),
        )
}

pub fn stepper(
    id: impl Into<SharedString>,
    value: impl Into<SharedString>,
    c: &Colors,
    on_change: impl Fn(i64, &mut Window, &mut App) + 'static,
) -> Div {
    let id: SharedString = id.into();
    let f = Rc::new(on_change);
    let g = f.clone();
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .child(button(ElementId::Name(format!("{id}-minus").into()), "−", Kind::Normal, true, c).w(px(26.)).on_click(move |_, w, cx| f(-1, w, cx)))
        .child(div().min_w(px(56.)).flex().justify_center().text_size(px(13.)).child(value.into()))
        .child(button(ElementId::Name(format!("{id}-plus").into()), "+", Kind::Normal, true, c).w(px(26.)).on_click(move |_, w, cx| g(1, w, cx)))
}

pub fn section_label(text: impl Into<SharedString>, c: &Colors) -> Div {
    div().text_size(px(11.)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(c.muted).child(text.into())
}

pub fn mono(text: impl Into<SharedString>) -> Div {
    div().font_family(MONO).text_size(px(12.)).child(text.into())
}

pub fn field_row(label: impl Into<SharedString>, c: &Colors) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .child(div().w(px(110.)).flex_none().text_size(px(12.5)).text_color(c.muted).child(label.into()))
}

pub fn progress_bar(frac: f64, c: &Colors) -> Div {
    div()
        .h(px(5.))
        .w_full()
        .rounded(px(3.))
        .bg(c.border)
        .child(div().h_full().rounded(px(3.)).bg(c.accent).w(gpui::relative(frac.clamp(0.0, 1.0) as f32)))
}

pub struct Tooltip(pub SharedString);

impl Render for Tooltip {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let c = Colors::get(window, cx);
        div()
            .max_w(px(360.))
            .px(px(8.))
            .py(px(5.))
            .rounded(px(5.))
            .bg(c.surface_alt)
            .border_1()
            .border_color(c.border)
            .text_color(c.text)
            .text_size(px(12.))
            .shadow_md()
            .child(self.0.clone())
    }
}

pub fn tooltip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView {
    let t: SharedString = text.into();
    move |_, cx| cx.new(|_| Tooltip(t.clone())).into()
}

pub fn fmt_ms(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Removes ANSI escape sequences; colors are conveyed by line classification.
pub fn strip_ansi(s: &str) -> String {
    if !s.contains('\x1b') && !s.contains('\r') {
        return s.to_string();
    }
    // Carriage returns overwrite the line; keep what was written last.
    let s = s.rsplit('\r').find(|p| !p.is_empty()).unwrap_or("");
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(ch) = it.next() {
        if ch == '\x1b' {
            if it.peek() == Some(&'[') {
                it.next();
                for c in it.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            } else {
                it.next();
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn strips_ansi() {
        assert_eq!(super::strip_ansi("\x1b[1;31merror\x1b[0m: x"), "error: x");
        assert_eq!(super::strip_ansi("10%\r20%\r"), "20%");
        assert_eq!(super::fmt_ms(61_000), "1:01");
    }
}
