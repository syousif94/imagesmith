//! Multi-line monospaced text editor with IME, selection, clipboard, undo,
//! line numbers and scrolling.

use crate::theme::{Colors, MONO};
use gpui::prelude::*;
use gpui::{
    actions, canvas, div, point, px, App, Bounds, ClipboardItem, Context, CursorStyle, ElementInputHandler, EventEmitter, FocusHandle,
    Focusable, KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, ScrollWheelEvent, SharedString, UTF16Selection,
    Window,
};
use std::ops::Range;
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;

actions!(
    text_area,
    [
        Backspace, Delete, Left, Right, Up, Down, SelectLeft, SelectRight, SelectUp, SelectDown, Home, End, SelectHome, SelectEnd, DocStart,
        DocEnd, SelectDocStart, SelectDocEnd, WordLeft, WordRight, SelectWordLeft, SelectWordRight, PageUp, PageDown, SelectAll, Copy, Cut,
        Paste, Newline, Tab, Undo, Redo, Save
    ]
);

pub fn bind_keys(cx: &mut App) {
    let ctx = Some("TextArea");
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("up", Up, ctx),
        KeyBinding::new("down", Down, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("shift-up", SelectUp, ctx),
        KeyBinding::new("shift-down", SelectDown, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("cmd-left", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("cmd-right", End, ctx),
        KeyBinding::new("shift-home", SelectHome, ctx),
        KeyBinding::new("cmd-shift-left", SelectHome, ctx),
        KeyBinding::new("shift-end", SelectEnd, ctx),
        KeyBinding::new("cmd-shift-right", SelectEnd, ctx),
        KeyBinding::new("cmd-up", DocStart, ctx),
        KeyBinding::new("cmd-down", DocEnd, ctx),
        KeyBinding::new("cmd-shift-up", SelectDocStart, ctx),
        KeyBinding::new("cmd-shift-down", SelectDocEnd, ctx),
        KeyBinding::new("alt-left", WordLeft, ctx),
        KeyBinding::new("alt-right", WordRight, ctx),
        KeyBinding::new("alt-shift-left", SelectWordLeft, ctx),
        KeyBinding::new("alt-shift-right", SelectWordRight, ctx),
        KeyBinding::new("pageup", PageUp, ctx),
        KeyBinding::new("pagedown", PageDown, ctx),
        KeyBinding::new("cmd-a", SelectAll, ctx),
        KeyBinding::new("cmd-c", Copy, ctx),
        KeyBinding::new("cmd-x", Cut, ctx),
        KeyBinding::new("cmd-v", Paste, ctx),
        KeyBinding::new("enter", Newline, ctx),
        KeyBinding::new("tab", Tab, ctx),
        KeyBinding::new("cmd-z", Undo, ctx),
        KeyBinding::new("cmd-shift-z", Redo, ctx),
        KeyBinding::new("cmd-s", Save, ctx),
    ]);
}

pub enum TextAreaEvent {
    Changed,
    Save,
}

const TAB_COLS: usize = 4;
const LINE_H: f32 = 18.;
const FONT_SIZE: f32 = 12.5;

pub struct TextArea {
    focus: FocusHandle,
    content: String,
    starts: Vec<usize>,
    sel: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    scroll: Point<Pixels>,
    bounds: Option<Bounds<Pixels>>,
    cell_w: Pixels,
    gutter: Pixels,
    undo: Vec<(String, Range<usize>)>,
    redo: Vec<(String, Range<usize>)>,
    last_typing: Option<Instant>,
    selecting: bool,
    goal_col: Option<usize>,
    reveal: bool,
    pub dirty: bool,
}

impl EventEmitter<TextAreaEvent> for TextArea {}

impl Focusable for TextArea {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn line_starts(s: &str) -> Vec<usize> {
    std::iter::once(0).chain(s.match_indices('\n').map(|(i, _)| i + 1)).collect()
}

impl TextArea {
    pub fn new(text: impl Into<String>, cx: &mut Context<Self>) -> TextArea {
        let content = text.into();
        TextArea {
            focus: cx.focus_handle(),
            starts: line_starts(&content),
            content,
            sel: 0..0,
            reversed: false,
            marked: None,
            scroll: Point::default(),
            bounds: None,
            cell_w: px(7.5),
            gutter: px(40.),
            undo: Vec::new(),
            redo: Vec::new(),
            last_typing: None,
            selecting: false,
            goal_col: None,
            reveal: true,
            dirty: false,
        }
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    // ------------------------------------------------------------ geometry

    fn line_of(&self, off: usize) -> usize {
        self.starts.partition_point(|&s| s <= off).saturating_sub(1)
    }

    fn line_range(&self, line: usize) -> Range<usize> {
        let start = self.starts[line];
        let end = self.starts.get(line + 1).map(|e| e - 1).unwrap_or(self.content.len());
        start..end
    }

    /// Display column of `off` (tabs expand to TAB_COLS).
    fn col_of(&self, off: usize) -> usize {
        let r = self.line_range(self.line_of(off));
        self.content[r.start..off.min(r.end)].chars().map(|c| if c == '\t' { TAB_COLS } else { 1 }).sum()
    }

    fn offset_at(&self, line: usize, col: usize) -> usize {
        let line = line.min(self.starts.len() - 1);
        let r = self.line_range(line);
        let mut c = 0;
        for (i, ch) in self.content[r.clone()].char_indices() {
            let w = if ch == '\t' { TAB_COLS } else { 1 };
            if c + w > col {
                return r.start + i + if col - c > w / 2 { ch.len_utf8() } else { 0 };
            }
            c += w;
        }
        r.end
    }

    fn cursor(&self) -> usize {
        if self.reversed {
            self.sel.start
        } else {
            self.sel.end
        }
    }

    fn line_h() -> Pixels {
        px(LINE_H)
    }

    fn visible_lines(&self) -> usize {
        self.bounds.map(|b| (b.size.height / Self::line_h()).floor() as usize).unwrap_or(20).max(1)
    }

    fn index_for_point(&self, p: Point<Pixels>) -> usize {
        let Some(b) = self.bounds else { return 0 };
        let y = p.y - b.top() + self.scroll.y;
        let line = if y < px(0.) { 0 } else { (y / Self::line_h()).floor() as usize };
        if line >= self.starts.len() {
            return self.content.len();
        }
        let x = p.x - b.left() - self.gutter + self.scroll.x;
        let col = if x < px(0.) { 0 } else { (x / self.cell_w).round() as usize };
        self.offset_at(line, col)
    }

    // ------------------------------------------------------------ editing

    fn snapshot(&mut self, typing: bool) {
        let coalesce = typing && self.last_typing.is_some_and(|t| t.elapsed() < Duration::from_millis(800));
        if !coalesce {
            self.undo.push((self.content.clone(), self.sel.clone()));
            if self.undo.len() > 200 {
                self.undo.remove(0);
            }
        }
        self.last_typing = typing.then(Instant::now);
        self.redo.clear();
    }

    fn replace(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        let typing = text.chars().count() == 1 && text != "\n" && range.is_empty();
        self.snapshot(typing);
        self.content.replace_range(range.clone(), text);
        self.starts = line_starts(&self.content);
        let end = range.start + text.len();
        self.sel = end..end;
        self.reversed = false;
        self.marked = None;
        self.goal_col = None;
        self.reveal = true;
        self.dirty = true;
        cx.emit(TextAreaEvent::Changed);
        cx.notify();
    }

    fn move_to(&mut self, off: usize, keep_goal: bool, cx: &mut Context<Self>) {
        self.sel = off..off;
        self.reversed = false;
        if !keep_goal {
            self.goal_col = None;
        }
        self.reveal = true;
        cx.notify();
    }

    fn select_to(&mut self, off: usize, keep_goal: bool, cx: &mut Context<Self>) {
        if self.reversed {
            self.sel.start = off;
        } else {
            self.sel.end = off;
        }
        if self.sel.end < self.sel.start {
            self.reversed = !self.reversed;
            self.sel = self.sel.end..self.sel.start;
        }
        if !keep_goal {
            self.goal_col = None;
        }
        self.reveal = true;
        cx.notify();
    }

    fn prev_boundary(&self, off: usize) -> usize {
        self.content.grapheme_indices(true).rev().find_map(|(i, _)| (i < off).then_some(i)).unwrap_or(0)
    }

    fn next_boundary(&self, off: usize) -> usize {
        self.content.grapheme_indices(true).find_map(|(i, _)| (i > off).then_some(i)).unwrap_or(self.content.len())
    }

    fn word_left(&self, off: usize) -> usize {
        let before = &self.content[..off];
        let trimmed = before.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_');
        trimmed.trim_end_matches(|c: char| c.is_alphanumeric() || c == '_').len()
    }

    fn word_right(&self, off: usize) -> usize {
        let after = &self.content[off..];
        let skip = after.len() - after.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '_').len();
        let rest = &after[skip..];
        off + skip + (rest.len() - rest.trim_start_matches(|c: char| c.is_alphanumeric() || c == '_').len())
    }

    fn vertical(&mut self, delta: isize) -> usize {
        let cur = self.cursor();
        let goal = match self.goal_col {
            Some(g) => g,
            None => {
                let g = self.col_of(cur);
                self.goal_col = Some(g);
                g
            }
        };
        let line = self.line_of(cur) as isize + delta;
        if line < 0 {
            return 0;
        }
        if line as usize >= self.starts.len() {
            return self.content.len();
        }
        self.offset_at(line as usize, goal)
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        if self.sel.is_empty() {
            let c = self.cursor();
            if c == 0 {
                return;
            }
            let r = self.prev_boundary(c)..c;
            self.replace(r, "", cx);
        } else {
            self.replace(self.sel.clone(), "", cx);
        }
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        if self.sel.is_empty() {
            let c = self.cursor();
            if c == self.content.len() {
                return;
            }
            let r = c..self.next_boundary(c);
            self.replace(r, "", cx);
        } else {
            self.replace(self.sel.clone(), "", cx);
        }
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        let t = if self.sel.is_empty() { self.prev_boundary(self.cursor()) } else { self.sel.start };
        self.move_to(t, false, cx);
    }
    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        let t = if self.sel.is_empty() { self.next_boundary(self.cursor()) } else { self.sel.end };
        self.move_to(t, false, cx);
    }
    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        let t = self.vertical(-1);
        self.move_to(t, true, cx);
    }
    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        let t = self.vertical(1);
        self.move_to(t, true, cx);
    }
    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        let t = self.prev_boundary(self.cursor());
        self.select_to(t, false, cx);
    }
    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        let t = self.next_boundary(self.cursor());
        self.select_to(t, false, cx);
    }
    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        let t = self.vertical(-1);
        self.select_to(t, true, cx);
    }
    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        let t = self.vertical(1);
        self.select_to(t, true, cx);
    }
    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let t = self.line_range(self.line_of(self.cursor())).start;
        self.move_to(t, false, cx);
    }
    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let t = self.line_range(self.line_of(self.cursor())).end;
        self.move_to(t, false, cx);
    }
    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        let t = self.line_range(self.line_of(self.cursor())).start;
        self.select_to(t, false, cx);
    }
    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        let t = self.line_range(self.line_of(self.cursor())).end;
        self.select_to(t, false, cx);
    }
    fn doc_start(&mut self, _: &DocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, false, cx);
    }
    fn doc_end(&mut self, _: &DocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), false, cx);
    }
    fn select_doc_start(&mut self, _: &SelectDocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, false, cx);
    }
    fn select_doc_end(&mut self, _: &SelectDocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), false, cx);
    }
    fn word_left_a(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let t = self.word_left(self.cursor());
        self.move_to(t, false, cx);
    }
    fn word_right_a(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        let t = self.word_right(self.cursor());
        self.move_to(t, false, cx);
    }
    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let t = self.word_left(self.cursor());
        self.select_to(t, false, cx);
    }
    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        let t = self.word_right(self.cursor());
        self.select_to(t, false, cx);
    }
    fn page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        let n = self.visible_lines() as isize;
        let t = self.vertical(-n);
        self.move_to(t, true, cx);
    }
    fn page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        let n = self.visible_lines() as isize;
        let t = self.vertical(n);
        self.move_to(t, true, cx);
    }
    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.sel = 0..self.content.len();
        self.reversed = false;
        cx.notify();
    }
    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.sel.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.content[self.sel.clone()].to_string()));
        }
    }
    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        if !self.sel.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(self.content[self.sel.clone()].to_string()));
            self.replace(self.sel.clone(), "", cx);
        }
    }
    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(t) = cx.read_from_clipboard().and_then(|i| i.text()) {
            self.replace(self.sel.clone(), &t.replace("\r\n", "\n"), cx);
        }
    }
    fn newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        let r = self.line_range(self.line_of(self.sel.start));
        let indent: String = self.content[r.start..self.sel.start].chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        self.replace(self.sel.clone(), &format!("\n{indent}"), cx);
    }
    fn tab(&mut self, _: &Tab, _: &mut Window, cx: &mut Context<Self>) {
        let uses_tabs = self.starts.iter().any(|&s| self.content[s..].starts_with('\t'));
        self.replace(self.sel.clone(), if uses_tabs { "\t" } else { "    " }, cx);
    }
    fn undo_a(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((text, sel)) = self.undo.pop() {
            self.redo.push((std::mem::replace(&mut self.content, text), self.sel.clone()));
            self.after_history(sel, cx);
        }
    }
    fn redo_a(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((text, sel)) = self.redo.pop() {
            self.undo.push((std::mem::replace(&mut self.content, text), self.sel.clone()));
            self.after_history(sel, cx);
        }
    }
    fn after_history(&mut self, sel: Range<usize>, cx: &mut Context<Self>) {
        self.starts = line_starts(&self.content);
        let s = sel.start.min(self.content.len());
        self.sel = s..sel.end.min(self.content.len()).max(s);
        self.last_typing = None;
        self.dirty = true;
        self.reveal = true;
        cx.emit(TextAreaEvent::Changed);
        cx.notify();
    }
    fn save(&mut self, _: &Save, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TextAreaEvent::Save);
    }

    fn on_mouse_down(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let off = self.index_for_point(e.position);
        if e.click_count >= 2 {
            let (a, b) = if e.click_count == 2 {
                (self.word_left(self.next_boundary(off).min(self.content.len())), self.word_right(off))
            } else {
                let r = self.line_range(self.line_of(off));
                (r.start, (r.end + 1).min(self.content.len()))
            };
            self.sel = a.min(b)..b.max(a);
            self.reversed = false;
            cx.notify();
            return;
        }
        self.selecting = true;
        if e.modifiers.shift {
            self.select_to(off, false, cx);
        } else {
            self.move_to(off, false, cx);
        }
    }

    fn on_mouse_move(&mut self, e: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            let off = self.index_for_point(e.position);
            self.select_to(off, false, cx);
        }
    }

    fn on_scroll(&mut self, e: &ScrollWheelEvent, window: &mut Window, cx: &mut Context<Self>) {
        let d = e.delta.pixel_delta(Self::line_h());
        let _ = window;
        let h = self.bounds.map(|b| b.size.height).unwrap_or(px(400.));
        let max_y = (Self::line_h() * self.starts.len() as f32 - h + Self::line_h()).max(px(0.));
        self.scroll.y = (self.scroll.y - d.y).clamp(px(0.), max_y);
        self.scroll.x = (self.scroll.x - d.x).max(px(0.));
        cx.notify();
    }

    /// Keeps the cursor inside the viewport after a move or edit.
    fn reveal_cursor(&mut self) {
        let Some(b) = self.bounds else { return };
        let cur = self.cursor();
        let lh = Self::line_h();
        let y = lh * self.line_of(cur) as f32;
        if y < self.scroll.y {
            self.scroll.y = y;
        } else if y + lh > self.scroll.y + b.size.height {
            self.scroll.y = y + lh - b.size.height;
        }
        let x = self.cell_w * self.col_of(cur) as f32;
        let w = b.size.width - self.gutter - px(8.);
        if x < self.scroll.x {
            self.scroll.x = (x - px(40.)).max(px(0.));
        } else if x > self.scroll.x + w {
            self.scroll.x = x - w + px(40.);
        }
    }

    fn utf16_to(&self, off16: usize) -> usize {
        let mut n16 = 0;
        for (i, ch) in self.content.char_indices() {
            if n16 >= off16 {
                return i;
            }
            n16 += ch.len_utf16();
        }
        self.content.len()
    }

    fn to_utf16(&self, off: usize) -> usize {
        self.content[..off.min(self.content.len())].encode_utf16().count()
    }
}

impl gpui::EntityInputHandler for TextArea {
    fn text_for_range(&mut self, r: Range<usize>, actual: &mut Option<Range<usize>>, _: &mut Window, _: &mut Context<Self>) -> Option<String> {
        let range = self.utf16_to(r.start)..self.utf16_to(r.end);
        actual.replace(self.to_utf16(range.start)..self.to_utf16(range.end));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: self.to_utf16(self.sel.start)..self.to_utf16(self.sel.end), reversed: self.reversed })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|m| self.to_utf16(m.start)..self.to_utf16(m.end))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(&mut self, r: Option<Range<usize>>, text: &str, _: &mut Window, cx: &mut Context<Self>) {
        let range = r.map(|r| self.utf16_to(r.start)..self.utf16_to(r.end)).or(self.marked.clone()).unwrap_or(self.sel.clone());
        self.replace(range, text, cx);
    }

    fn replace_and_mark_text_in_range(&mut self, r: Option<Range<usize>>, text: &str, new_sel: Option<Range<usize>>, _: &mut Window, cx: &mut Context<Self>) {
        let range = r.map(|r| self.utf16_to(r.start)..self.utf16_to(r.end)).or(self.marked.clone()).unwrap_or(self.sel.clone());
        self.content.replace_range(range.clone(), text);
        self.starts = line_starts(&self.content);
        self.marked = (!text.is_empty()).then(|| range.start..range.start + text.len());
        let end = range.start + text.len();
        self.sel = match new_sel {
            Some(s) => {
                let base = self.to_utf16(range.start);
                let a = self.utf16_to(base + s.start);
                let b = self.utf16_to(base + s.end);
                a..b
            }
            None => end..end,
        };
        self.dirty = true;
        cx.notify();
    }

    fn bounds_for_range(&mut self, r: Range<usize>, bounds: Bounds<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        let off = self.utf16_to(r.start);
        let x = bounds.left() + self.gutter - self.scroll.x + self.cell_w * self.col_of(off) as f32;
        let y = bounds.top() - self.scroll.y + Self::line_h() * self.line_of(off) as f32;
        Some(Bounds::new(point(x, y), gpui::size(self.cell_w, Self::line_h())))
    }

    fn character_index_for_point(&mut self, p: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        Some(self.to_utf16(self.index_for_point(p)))
    }
}

impl Render for TextArea {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = Colors::get(window, cx);
        let font_size = px(FONT_SIZE);
        let font_id = window.text_system().resolve_font(&gpui::font(MONO));
        self.cell_w = window.text_system().advance(font_id, font_size, 'm').map(|s| s.width).unwrap_or(px(7.5));
        let digits = self.starts.len().to_string().len().max(3);
        self.gutter = self.cell_w * digits as f32 + px(18.);
        if self.reveal {
            self.reveal = false;
            self.reveal_cursor();
        }
        let lh = Self::line_h();
        let focused = self.focus.is_focused(window);
        let height = self.bounds.map(|b| b.size.height).unwrap_or(px(600.));
        let first = (self.scroll.y / lh).floor().max(0.) as usize;
        let count = (height / lh).ceil() as usize + 1;
        let last = (first + count).min(self.starts.len());
        let cur = self.cursor();
        let (cur_line, cur_col) = (self.line_of(cur), self.col_of(cur));

        let mut layers = div().absolute().inset_0();
        for line in first..last {
            let y = lh * line as f32 - self.scroll.y;
            let r = self.line_range(line);
            let raw = &self.content[r.clone()];
            let shown: SharedString = raw.replace('\t', &" ".repeat(TAB_COLS)).into();
            let comment = raw.trim_start().starts_with('#');
            if !self.sel.is_empty() && self.sel.start <= r.end && self.sel.end >= r.start {
                let a = self.col_of(self.sel.start.max(r.start));
                let mut b = self.col_of(self.sel.end.min(r.end));
                if self.sel.end > r.end {
                    b += 1;
                }
                layers = layers.child(
                    div()
                        .absolute()
                        .top(y)
                        .left(self.gutter - self.scroll.x + self.cell_w * a as f32)
                        .w(self.cell_w * (b.saturating_sub(a)) as f32)
                        .h(lh)
                        .bg(c.accent_soft),
                );
            }
            layers = layers
                .child(
                    div()
                        .absolute()
                        .top(y)
                        .left(self.gutter - self.scroll.x)
                        .h(lh)
                        .whitespace_nowrap()
                        .text_color(if comment { c.faint } else { c.text })
                        .child(shown),
                )
                .child(
                    div()
                        .absolute()
                        .top(y)
                        .left_0()
                        .w(self.gutter - px(8.))
                        .h(lh)
                        .bg(c.surface_alt)
                        .flex()
                        .justify_end()
                        .pr(px(6.))
                        .text_color(if line == cur_line { c.muted } else { c.faint })
                        .child((line + 1).to_string()),
                );
        }
        if focused && self.sel.is_empty() && (first..last).contains(&cur_line) {
            layers = layers.child(
                div()
                    .absolute()
                    .top(lh * cur_line as f32 - self.scroll.y)
                    .left(self.gutter - self.scroll.x + self.cell_w * cur_col as f32)
                    .w(px(1.5))
                    .h(lh)
                    .bg(c.accent),
            );
        }
        let entity = cx.entity();
        let entity2 = cx.entity();
        let focus = self.focus.clone();
        div()
            .id("text-area")
            .key_context("TextArea")
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(c.input_bg)
            .font_family(MONO)
            .text_size(font_size)
            .line_height(lh)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::doc_start))
            .on_action(cx.listener(Self::doc_end))
            .on_action(cx.listener(Self::select_doc_start))
            .on_action(cx.listener(Self::select_doc_end))
            .on_action(cx.listener(Self::word_left_a))
            .on_action(cx.listener(Self::word_right_a))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::page_up))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::tab))
            .on_action(cx.listener(Self::undo_a))
            .on_action(cx.listener(Self::redo_a))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(|this, _: &crate::text_input::Copy, w, cx| this.copy(&Copy, w, cx)))
            .on_action(cx.listener(|this, _: &crate::text_input::Cut, w, cx| this.cut(&Cut, w, cx)))
            .on_action(cx.listener(|this, _: &crate::text_input::Paste, w, cx| this.paste(&Paste, w, cx)))
            .on_action(cx.listener(|this, _: &crate::text_input::SelectAll, w, cx| this.select_all(&SelectAll, w, cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, _| this.selecting = false))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _, _, _| this.selecting = false))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(
                canvas(
                    move |bounds, _, cx| {
                        entity.update(cx, |this, _| this.bounds = Some(bounds));
                        bounds
                    },
                    move |bounds, _, window, cx| {
                        window.handle_input(&focus, ElementInputHandler::new(bounds, entity2), cx);
                    },
                )
                .absolute()
                .size_full(),
            )
            .child(layers)
    }
}

#[cfg(test)]
mod tests {
    use super::line_starts;

    #[test]
    fn starts() {
        assert_eq!(line_starts("a\nbc\n"), vec![0, 2, 5]);
        assert_eq!(line_starts(""), vec![0]);
    }
}
