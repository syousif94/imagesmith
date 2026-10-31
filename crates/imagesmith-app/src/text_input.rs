//! Single-line text field (adapted from gpui's input example) with IME
//! support, selection, clipboard, horizontal scrolling and a masked mode.

use crate::theme::Colors;
use gpui::prelude::*;
use gpui::{
    actions, div, fill, point, px, relative, size, App, Bounds, ClipboardItem, Context, CursorStyle, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId,
    KeyBinding, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ShapedLine, SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window, WrappedLine,
};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

actions!(
    text_input,
    [Backspace, Delete, Left, Right, Up, Down, SelectLeft, SelectRight, SelectAll, Home, End, Paste, Cut, Copy, Enter, Newline, Escape]
);

pub fn bind_keys(cx: &mut App) {
    let ctx = Some("TextInput");
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("cmd-a", SelectAll, ctx),
        KeyBinding::new("cmd-v", Paste, ctx),
        KeyBinding::new("cmd-c", Copy, ctx),
        KeyBinding::new("cmd-x", Cut, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("cmd-left", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("cmd-right", End, ctx),
        KeyBinding::new("enter", Enter, ctx),
        KeyBinding::new("shift-enter", Newline, ctx),
        KeyBinding::new("up", Up, ctx),
        KeyBinding::new("down", Down, ctx),
        KeyBinding::new("escape", Escape, ctx),
    ]);
}

pub enum InputEvent {
    Changed,
    Submit,
}

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    masked: bool,
    mono: bool,
    pub invalid: bool,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    scroll_x: Pixels,
    is_selecting: bool,
    /// Soft-wrapping mode that grows up to `max_lines`, then scrolls.
    multiline: bool,
    max_lines: usize,
    /// (byte offset, y) of each hard line as last painted.
    last_lines: Vec<(usize, Pixels, WrappedLine)>,
    line_height: Pixels,
    scroll_y: Pixels,
    goal_x: Option<Pixels>,
}

impl EventEmitter<InputEvent> for TextInput {}

const BULLET: char = '•';

impl TextInput {
    pub fn new(cx: &mut Context<Self>, placeholder: impl Into<SharedString>) -> TextInput {
        TextInput {
            focus_handle: cx.focus_handle(),
            content: SharedString::default(),
            placeholder: placeholder.into(),
            masked: false,
            mono: false,
            invalid: false,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            scroll_x: px(0.),
            is_selecting: false,
            multiline: false,
            max_lines: 1,
            last_lines: Vec::new(),
            line_height: px(18.),
            scroll_y: px(0.),
            goal_x: None,
        }
    }

    pub fn multiline(mut self, max_lines: usize) -> Self {
        self.multiline = true;
        self.max_lines = max_lines.max(1);
        self
    }

    pub fn masked(mut self) -> Self {
        self.masked = true;
        self
    }

    pub fn mono(mut self) -> Self {
        self.mono = true;
        self
    }

    pub fn text(&self) -> String {
        self.content.to_string()
    }

    /// Replaces the content without emitting `Changed`.
    pub fn set_text(&mut self, t: impl Into<SharedString>, cx: &mut Context<Self>) {
        let t = t.into();
        if t != self.content {
            self.content = t;
            self.selected_range = self.content.len()..self.content.len();
            self.marked_range = None;
            cx.notify();
        }
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx)
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let t = if self.multiline { self.content[..self.cursor_offset()].rfind('\n').map(|i| i + 1).unwrap_or(0) } else { 0 };
        self.move_to(t, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let c = self.cursor_offset();
        let t = if self.multiline { self.content[c..].find('\n').map(|i| c + i).unwrap_or(self.content.len()) } else { self.content.len() };
        self.move_to(t, cx);
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(-1., cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(1., cx);
    }

    fn vertical(&mut self, dir: f32, cx: &mut Context<Self>) {
        if !self.multiline || self.content.is_empty() {
            return;
        }
        let lh = self.line_height;
        let p = position_in(&self.last_lines, self.cursor_offset(), lh, self.content.len());
        let x = *self.goal_x.get_or_insert(p.x);
        let y = p.y + lh * dir + lh / 2.;
        let total = self.last_lines.last().map(|(_, y, l)| *y + l.size(lh).height).unwrap_or(lh);
        let t = if y < px(0.) {
            0
        } else if y >= total {
            self.content.len()
        } else {
            index_in(&self.last_lines, point(x, y), lh)
        };
        let goal = self.goal_x;
        self.move_to(t, cx);
        self.goal_x = goal;
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if self.multiline {
            self.replace_text_in_range(None, "\n", window, cx);
        } else {
            cx.emit(InputEvent::Submit);
        }
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn enter(&mut self, _: &Enter, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(InputEvent::Submit);
    }

    fn escape(&mut self, _: &Escape, window: &mut Window, _: &mut Context<Self>) {
        window.blur();
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        self.is_selecting = true;
        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), cx)
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = if self.multiline { text.replace("\r\n", "\n") } else { text.replace('\n', " ") };
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() && !self.masked {
            cx.write_to_clipboard(ClipboardItem::new_string(self.content[self.selected_range.clone()].to_string()));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() && !self.masked {
            cx.write_to_clipboard(ClipboardItem::new_string(self.content[self.selected_range.clone()].to_string()));
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.goal_x = None;
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    /// Content offset → offset in the displayed (possibly masked) text.
    fn display_offset(&self, offset: usize) -> usize {
        if self.masked {
            self.content[..offset].chars().count() * BULLET.len_utf8()
        } else {
            offset
        }
    }

    fn content_offset(&self, display: usize) -> usize {
        if self.masked {
            let n = display / BULLET.len_utf8();
            self.content.char_indices().nth(n).map(|(i, _)| i).unwrap_or(self.content.len())
        } else {
            display
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        if self.multiline {
            let Some(b) = self.last_bounds else { return 0 };
            return index_in(&self.last_lines, point(position.x - b.left(), position.y - b.top() + self.scroll_y), self.line_height);
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref()) else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        self.content_offset(line.closest_index_for_x(position.x - bounds.left() + self.scroll_x))
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify()
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content.grapheme_indices(true).rev().find_map(|(idx, _)| (idx < offset).then_some(idx)).unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: self.range_to_utf16(&self.selected_range), reversed: self.selection_reversed })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(&mut self, range_utf16: Option<Range<usize>>, new_text: &str, _: &mut Window, cx: &mut Context<Self>) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        self.content = (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..]).into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range.take();
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        self.content = (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..]).into();
        self.marked_range = (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .map(|new_range| new_range.start + range.start..new_range.end + range.start)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        cx.notify();
    }

    fn bounds_for_range(&mut self, range_utf16: Range<usize>, bounds: Bounds<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        if self.multiline {
            let range = self.range_from_utf16(&range_utf16);
            let p = position_in(&self.last_lines, range.start, self.line_height, self.content.len());
            let o = point(bounds.left() + p.x, bounds.top() + p.y - self.scroll_y);
            return Some(Bounds::new(o, size(px(2.), self.line_height)));
        }
        let last_layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let x = |o: usize| bounds.left() - self.scroll_x + last_layout.x_for_index(self.display_offset(o));
        Some(Bounds::from_corners(point(x(range.start), bounds.top()), point(x(range.end), bounds.bottom())))
    }

    fn character_index_for_point(&mut self, p: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        if self.multiline {
            return Some(self.offset_to_utf16(self.index_for_mouse_position(p)));
        }
        let bounds = self.last_bounds?;
        let last_layout = self.last_layout.as_ref()?;
        let idx = last_layout.index_for_x(p.x - bounds.left() + self.scroll_x)?;
        Some(self.offset_to_utf16(self.content_offset(idx)))
    }
}

struct TextElement {
    input: Entity<TextInput>,
    cursor_color: gpui::Hsla,
    selection_color: gpui::Hsla,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
    scroll_x: Pixels,
}

impl IntoElement for TextElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let style = window.text_style();
        let shown: SharedString = if input.masked {
            BULLET.to_string().repeat(input.content.chars().count()).into()
        } else {
            input.content.clone()
        };
        let (display_text, text_color) =
            if input.content.is_empty() { (input.placeholder.clone(), style.color.opacity(0.4)) } else { (shown, style.color) };

        let run = TextRun { len: display_text.len(), font: style.font(), color: text_color, background_color: None, underline: None, strikethrough: None };
        let runs = match input.marked_range.as_ref().filter(|_| !input.masked && !input.content.is_empty()) {
            Some(m) => vec![
                TextRun { len: m.start, ..run.clone() },
                TextRun {
                    len: m.end - m.start,
                    underline: Some(UnderlineStyle { color: Some(run.color), thickness: px(1.0), wavy: false }),
                    ..run.clone()
                },
                TextRun { len: display_text.len() - m.end, ..run },
            ]
            .into_iter()
            .filter(|r| r.len > 0)
            .collect(),
            None => vec![run],
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window.text_system().shape_line(display_text, font_size, &runs, None);

        let x_of = |o: usize| if input.content.is_empty() { px(0.) } else { line.x_for_index(input.display_offset(o)) };
        let cursor_x = x_of(input.cursor_offset());
        let width = bounds.size.width;
        let mut scroll_x = input.scroll_x;
        if cursor_x - scroll_x > width - px(2.) {
            scroll_x = cursor_x - width + px(2.);
        } else if cursor_x < scroll_x {
            scroll_x = cursor_x;
        }
        if line.width <= width {
            scroll_x = px(0.);
        }
        let left = bounds.left() - scroll_x;
        let sel = input.selected_range.clone();
        let (selection, cursor) = if sel.is_empty() {
            (None, Some(fill(Bounds::new(point(left + cursor_x, bounds.top()), size(px(1.5), bounds.size.height)), self.cursor_color)))
        } else {
            (
                Some(fill(
                    Bounds::from_corners(point(left + x_of(sel.start), bounds.top()), point(left + x_of(sel.end), bounds.bottom())),
                    self.selection_color,
                )),
                None,
            )
        };
        PrepaintState { line: Some(line), cursor, selection, scroll_x }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(&focus_handle, ElementInputHandler::new(bounds, self.input.clone()), cx);
        let scroll_x = prepaint.scroll_x;
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            if let Some(selection) = prepaint.selection.take() {
                window.paint_quad(selection)
            }
            let line = prepaint.line.take().unwrap();
            let _ = line.paint(point(bounds.left() - scroll_x, bounds.top()), window.line_height(), window, cx);
            if focus_handle.is_focused(window) {
                if let Some(cursor) = prepaint.cursor.take() {
                    window.paint_quad(cursor);
                }
            }
            self.input.update(cx, |input, _| {
                input.last_layout = Some(line);
                input.last_bounds = Some(bounds);
                input.scroll_x = scroll_x;
            });
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = Colors::get(window, cx);
        let focused = self.focus_handle.is_focused(window);
        let multiline = self.multiline;
        div()
            .flex()
            .w_full()
            .min_w(px(40.))
            .when(!multiline, |d| d.items_center().h(px(26.)))
            .when(multiline, |d| d.items_start().min_h(px(26.)).py(px(4.)))
            .px(px(7.))
            .rounded(px(5.))
            .border_1()
            .border_color(if self.invalid {
                c.danger
            } else if focused {
                c.accent
            } else {
                c.border
            })
            .bg(c.input_bg)
            .text_color(c.text)
            .text_size(px(13.))
            .line_height(px(18.))
            .when(self.mono, |d| d.font_family(crate::theme::MONO).text_size(px(12.)))
            .overflow_hidden()
            .key_context("TextInput")
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::enter))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::escape))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .when(!multiline, |d| d.child(TextElement { input: cx.entity(), cursor_color: c.accent, selection_color: c.accent_soft }))
            .when(multiline, |d| d.child(WrappedTextElement { input: cx.entity(), cursor_color: c.accent, selection_color: c.accent_soft }))
    }
}

/// Position of byte `off` relative to the unscrolled text origin.
fn position_in(lines: &[(usize, Pixels, WrappedLine)], off: usize, lh: Pixels, len: usize) -> Point<Pixels> {
    for (start, y, line) in lines {
        if off >= *start && off <= start + line.len() {
            if let Some(p) = line.position_for_index(off - start, lh) {
                return point(p.x, p.y + *y);
            }
        }
    }
    match lines.last() {
        Some((_, y, l)) if off >= len => point(px(0.), *y + l.size(lh).height),
        _ => point(px(0.), px(0.)),
    }
}

fn index_in(lines: &[(usize, Pixels, WrappedLine)], p: Point<Pixels>, lh: Pixels) -> usize {
    for (i, (start, y, line)) in lines.iter().enumerate() {
        if p.y < *y + line.size(lh).height || i + 1 == lines.len() {
            let r = line.closest_index_for_position(point(p.x, (p.y - *y).max(px(0.))), lh);
            return start + r.unwrap_or_else(|e| e).min(line.len());
        }
    }
    0
}

struct WrappedTextElement {
    input: Entity<TextInput>,
    cursor_color: gpui::Hsla,
    selection_color: gpui::Hsla,
}

struct WrappedPrepaint {
    lines: Vec<(usize, Pixels, WrappedLine)>,
    quads: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
    scroll_y: Pixels,
}

impl IntoElement for WrappedTextElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl WrappedTextElement {
    fn display(&self, window: &Window, cx: &App) -> (SharedString, TextRun, Pixels) {
        let input = self.input.read(cx);
        let style = window.text_style();
        let (text, color) = if input.content.is_empty() { (input.placeholder.clone(), style.color.opacity(0.4)) } else { (input.content.clone(), style.color) };
        let run = TextRun { len: text.len(), font: style.font(), color, background_color: None, underline: None, strikethrough: None };
        (text, run, style.font_size.to_pixels(window.rem_size()))
    }
}

fn shape(window: &Window, text: &SharedString, run: &TextRun, font_size: Pixels, width: Pixels, lh: Pixels) -> (Vec<(usize, Pixels, WrappedLine)>, Pixels) {
    let lines = window.text_system().shape_text(text.clone(), font_size, std::slice::from_ref(run), Some(width), None).unwrap_or_default();
    let (mut out, mut start, mut y) = (Vec::new(), 0, px(0.));
    for l in lines {
        let h = l.size(lh).height;
        let len = l.len();
        out.push((start, y, l));
        start += len + 1;
        y += h;
    }
    (out, y.max(lh))
}

impl Element for WrappedTextElement {
    type RequestLayoutState = ();
    type PrepaintState = WrappedPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(&mut self, _: Option<&GlobalElementId>, _: Option<&gpui::InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, ()) {
        let (text, run, font_size) = self.display(window, cx);
        let lh = window.line_height();
        let max = self.input.read(cx).max_lines;
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let id = window.request_measured_layout(style, move |known, avail, window, _| {
            let width = known.width.or(match avail.width {
                gpui::AvailableSpace::Definite(w) => Some(w),
                _ => None,
            });
            let h = match width {
                Some(w) => shape(window, &text, &run, font_size, w, lh).1,
                None => lh,
            };
            size(width.unwrap_or(px(100.)), h.min(lh * max as f32))
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
    ) -> WrappedPrepaint {
        let (text, run, font_size) = self.display(window, cx);
        let lh = window.line_height();
        let (lines, total) = shape(window, &text, &run, font_size, bounds.size.width, lh);
        let input = self.input.read(cx);
        let empty = input.content.is_empty();
        let pos = |o: usize| if empty { point(px(0.), px(0.)) } else { position_in(&lines, o, lh, input.content.len()) };
        let cur = pos(input.cursor_offset());
        let mut sy = input.scroll_y;
        if cur.y < sy {
            sy = cur.y;
        } else if cur.y + lh > sy + bounds.size.height {
            sy = cur.y + lh - bounds.size.height;
        }
        sy = sy.clamp(px(0.), (total - bounds.size.height).max(px(0.)));
        let o = point(bounds.left(), bounds.top() - sy);
        let sel = input.selected_range.clone();
        let mut quads = Vec::new();
        if !sel.is_empty() && !empty {
            let (a, b) = (pos(sel.start), pos(sel.end));
            let rect = |x0: Pixels, y0: Pixels, x1: Pixels, y1: Pixels| fill(Bounds::from_corners(point(o.x + x0, o.y + y0), point(o.x + x1, o.y + y1)), self.selection_color);
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
        let cursor = sel.is_empty().then(|| fill(Bounds::new(point(o.x + cur.x, o.y + cur.y), size(px(1.5), lh)), self.cursor_color));
        WrappedPrepaint { lines, quads, cursor, scroll_y: sy }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut WrappedPrepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(&focus_handle, ElementInputHandler::new(bounds, self.input.clone()), cx);
        let lh = window.line_height();
        let sy = prepaint.scroll_y;
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for q in prepaint.quads.drain(..) {
                window.paint_quad(q);
            }
            for (_, y, line) in &prepaint.lines {
                let _ = line.paint(point(bounds.left(), bounds.top() - sy + *y), lh, gpui::TextAlign::Left, None, window, cx);
            }
            if focus_handle.is_focused(window) {
                if let Some(c) = prepaint.cursor.take() {
                    window.paint_quad(c);
                }
            }
        });
        let lines = std::mem::take(&mut prepaint.lines);
        self.input.update(cx, |input, _| {
            input.last_lines = lines;
            input.last_bounds = Some(bounds);
            input.line_height = lh;
            input.scroll_y = sy;
        });
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
