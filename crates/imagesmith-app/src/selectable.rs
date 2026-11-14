//! Read-only wrapped text that can be selected with the mouse and copied.

use crate::text_input::{index_in, position_in, shape, Copy, SelectAll};
use gpui::prelude::*;
use gpui::{
    div, fill, point, px, relative, size, App, Bounds, ClipboardItem, Context, CursorStyle, DispatchPhase, ElementId, Entity, FocusHandle,
    FontWeight, GlobalElementId, Hsla, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    SharedString, Style, TextRun, Window, WrappedLine,
};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

/// One paragraph of the text.
#[derive(Clone, PartialEq)]
pub struct Span {
    pub text: String,
    pub color: Hsla,
    pub bold: bool,
}

pub struct SelectableText {
    focus: FocusHandle,
    spans: Vec<Span>,
    text: SharedString,
    sel: Range<usize>,
    anchor: usize,
    selecting: bool,
    lines: Vec<(usize, Pixels, WrappedLine)>,
    bounds: Option<Bounds<Pixels>>,
    lh: Pixels,
}

impl SelectableText {
    pub fn new(cx: &mut Context<Self>) -> Self {
        SelectableText {
            focus: cx.focus_handle(),
            spans: Vec::new(),
            text: SharedString::default(),
            sel: 0..0,
            anchor: 0,
            selecting: false,
            lines: Vec::new(),
            bounds: None,
            lh: px(18.),
        }
    }

    pub fn set_spans(&mut self, spans: Vec<Span>, cx: &mut Context<Self>) {
        if spans == self.spans {
            return;
        }
        self.text = spans.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join("\n").into();
        self.spans = spans;
        self.sel = 0..0;
        self.anchor = 0;
        cx.notify();
    }

    fn index_for(&self, p: Point<Pixels>) -> usize {
        let Some(b) = self.bounds else { return 0 };
        if p.y < b.top() {
            return 0;
        }
        if p.y >= b.bottom() {
            return self.text.len();
        }
        index_in(&self.lines, point(p.x - b.left(), p.y - b.top()), self.lh)
    }

    fn extend(&mut self, i: usize) {
        self.sel = self.anchor.min(i)..self.anchor.max(i);
    }

    fn on_mouse_down(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let i = self.index_for(e.position);
        let t = self.text.as_str();
        match e.click_count {
            2 => {
                self.sel = t
                    .split_word_bound_indices()
                    .find(|(s, w)| i >= *s && i < s + w.len())
                    .map(|(s, w)| s..s + w.len())
                    .unwrap_or(i..i);
                self.anchor = self.sel.start;
            }
            n if n >= 3 => {
                let a = t[..i].rfind('\n').map(|x| x + 1).unwrap_or(0);
                let b = t[i..].find('\n').map(|x| i + x).unwrap_or(t.len());
                self.sel = a..b;
                self.anchor = a;
            }
            _ => {
                if e.modifiers.shift {
                    self.extend(i);
                } else {
                    self.anchor = i;
                    self.sel = i..i;
                }
                self.selecting = true;
            }
        }
        cx.notify();
    }

    fn drag(&mut self, p: Point<Pixels>, cx: &mut Context<Self>) {
        if self.selecting {
            let i = self.index_for(p);
            self.extend(i);
            cx.notify();
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.sel.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.text[self.sel.clone()].to_string()));
        }
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.anchor = 0;
        self.sel = 0..self.text.len();
        cx.notify();
    }
}

impl Render for SelectableText {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = crate::theme::Colors::get(window, cx);
        div()
            .key_context("Selectable")
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
            .w_full()
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::select_all))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .child(TextEl { view: cx.entity(), selection_color: c.accent_soft })
    }
}

struct TextEl {
    view: Entity<SelectableText>,
    selection_color: Hsla,
}

impl IntoElement for TextEl {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl TextEl {
    fn runs(&self, window: &Window, cx: &App) -> (SharedString, Vec<TextRun>, Pixels) {
        let v = self.view.read(cx);
        let style = window.text_style();
        let n = v.spans.len();
        let runs = v
            .spans
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let mut font = style.font();
                if s.bold {
                    font.weight = FontWeight::SEMIBOLD;
                }
                let len = s.text.len() + usize::from(i + 1 < n);
                TextRun { len, font, color: s.color, background_color: None, underline: None, strikethrough: None }
            })
            .collect();
        (v.text.clone(), runs, style.font_size.to_pixels(window.rem_size()))
    }
}

impl Element for TextEl {
    type RequestLayoutState = ();
    type PrepaintState = (Vec<(usize, Pixels, WrappedLine)>, Vec<PaintQuad>);

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(&mut self, _: Option<&GlobalElementId>, _: Option<&gpui::InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, ()) {
        let (text, runs, font_size) = self.runs(window, cx);
        let lh = window.line_height();
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let id = window.request_measured_layout(style, move |known, avail, window, _| {
            let width = known.width.or(match avail.width {
                gpui::AvailableSpace::Definite(w) => Some(w),
                _ => None,
            });
            let (lines, h) = shape(window, &text, &runs, font_size, width.unwrap_or(px(100_000.)), lh);
            let w = width.unwrap_or_else(|| lines.iter().map(|(_, _, l)| l.size(lh).width).fold(px(0.), |a, b| a.max(b)));
            size(w, h)
        });
        (id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let (text, runs, font_size) = self.runs(window, cx);
        let lh = window.line_height();
        let (lines, _) = shape(window, &text, &runs, font_size, bounds.size.width, lh);
        let sel = self.view.read(cx).sel.clone();
        let mut quads = Vec::new();
        if !sel.is_empty() {
            let pos = |o: usize| position_in(&lines, o, lh, text.len());
            let (a, b) = (pos(sel.start), pos(sel.end));
            let o = bounds.origin;
            let rect = |x0: Pixels, y0: Pixels, x1: Pixels, y1: Pixels| {
                fill(Bounds::from_corners(point(o.x + x0, o.y + y0), point(o.x + x1, o.y + y1)), self.selection_color)
            };
            if a.y == b.y {
                quads.push(rect(a.x, a.y, b.x, a.y + lh));
            } else {
                quads.push(rect(a.x, a.y, bounds.size.width, a.y + lh));
                if b.y > a.y + lh {
                    quads.push(rect(px(0.), a.y + lh, bounds.size.width, b.y));
                }
                quads.push(rect(px(0.), b.y, b.x, b.y + lh));
            }
        }
        (lines, quads)
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let lh = window.line_height();
        for q in prepaint.1.drain(..) {
            window.paint_quad(q);
        }
        for (_, y, line) in &prepaint.0 {
            let _ = line.paint(point(bounds.left(), bounds.top() + *y), lh, gpui::TextAlign::Left, None, window, cx);
        }
        let lines = std::mem::take(&mut prepaint.0);
        self.view.update(cx, |v, _| {
            v.lines = lines;
            v.bounds = Some(bounds);
            v.lh = lh;
        });
        // Drag and release may happen outside the text, so listen window-wide.
        let view = self.view.clone();
        window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble {
                view.update(cx, |v, cx| v.drag(e.position, cx));
            }
        });
        let view = self.view.clone();
        window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble {
                view.update(cx, |v, _| v.selecting = false);
            }
        });
    }
}
