//! Embedded terminal: alacritty_terminal drives the PTY and VT state; this
//! view paints its grid and forwards keys, paste, scroll and selection.

use crate::theme::MONO;
use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::tty;
use alacritty_terminal::vte::ansi::{Color as AColor, CursorShape, NamedColor, Rgb};
use alacritty_terminal::Term;
use futures::channel::mpsc::{unbounded, UnboundedSender};
use futures::StreamExt;
use gpui::prelude::*;
use gpui::{
    canvas, div, px, rgb, App, Bounds, ClipboardItem, Context, EventEmitter, FocusHandle, Focusable, Font, FontWeight, Hsla, KeyDownEvent,
    Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, ScrollWheelEvent, SharedString, StyledText, Task, TextRun,
    UnderlineStyle, Window,
};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const FONT_SIZE: f32 = 13.;
const LINE_H: f32 = 17.;
const BG: u32 = 0x1c1c1e;
const FG: u32 = 0xd4d4d4;
const PALETTE: [u32; 16] = [
    0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, 0x666666, 0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6,
    0x29b8db, 0xffffff,
];

#[derive(Clone)]
struct Listener(UnboundedSender<TermEvent>);

impl EventListener for Listener {
    fn send_event(&self, e: TermEvent) {
        let _ = self.0.unbounded_send(e);
    }
}

pub enum TerminalEvent {
    Exited,
}

pub struct TerminalView {
    term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    focus: FocusHandle,
    grid: (usize, usize),
    cell: (Pixels, Pixels),
    origin: Point<Pixels>,
    selecting: bool,
    last_paste: Option<Instant>,
    pub exited: Option<Option<i32>>,
    pub title: Option<String>,
    _events: Task<()>,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Drop for TerminalView {
    fn drop(&mut self) {
        let _ = self.sender.send(Msg::Shutdown);
    }
}

fn rgb_hsla(c: Rgb) -> Hsla {
    rgb(((c.r as u32) << 16) | ((c.g as u32) << 8) | c.b as u32).into()
}

fn indexed(i: usize) -> u32 {
    match i {
        0..=15 => PALETTE[i],
        16..=231 => {
            let i = i - 16;
            let v = |n: usize| if n == 0 { 0 } else { 55 + n as u32 * 40 };
            (v(i / 36) << 16) | (v(i / 6 % 6) << 8) | v(i % 6)
        }
        _ => {
            let g = 8 + (i.min(255) - 232) as u32 * 10;
            (g << 16) | (g << 8) | g
        }
    }
}

/// A started PTY session, before it has a view.
pub struct Spawned {
    term: Arc<FairMutex<Term<Listener>>>,
    sender: EventLoopSender,
    rx: futures::channel::mpsc::UnboundedReceiver<TermEvent>,
}

impl TerminalView {
    pub fn spawn(program: String, args: Vec<String>) -> Result<Spawned, String> {
        let opts = tty::Options {
            shell: Some(tty::Shell::new(program, args)),
            drain_on_exit: true,
            env: HashMap::from([("TERM".into(), "xterm-256color".into()), ("COLORTERM".into(), "truecolor".into())]),
            ..Default::default()
        };
        let size = WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 17 };
        let pty = tty::new(&opts, size, 0).map_err(|e| format!("cannot open a PTY: {e}"))?;
        let (tx, rx) = unbounded();
        let listener = Listener(tx);
        let term = Arc::new(FairMutex::new(Term::new(Config::default(), &TermSize::new(80, 24), listener.clone())));
        let el = EventLoop::new(term.clone(), listener, pty, true, false).map_err(|e| e.to_string())?;
        let sender = el.channel();
        el.spawn();
        Ok(Spawned { term, sender, rx })
    }

    pub fn new(s: Spawned, cx: &mut Context<Self>) -> TerminalView {
        let Spawned { term, sender, mut rx } = s;
        let events = cx.spawn(async move |this, cx| {
            while let Some(first) = rx.next().await {
                let mut batch = vec![first];
                while let Ok(e) = rx.try_recv() {
                    batch.push(e);
                }
                if this.update(cx, |this, cx| batch.into_iter().for_each(|e| this.on_event(e, cx))).is_err() {
                    break;
                }
            }
        });
        TerminalView {
            term,
            sender,
            focus: cx.focus_handle(),
            grid: (80, 24),
            cell: (px(8.), px(LINE_H)),
            origin: Point::default(),
            selecting: false,
            last_paste: None,
            exited: None,
            title: None,
            _events: events,
        }
    }

    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        let _ = self.sender.send(Msg::Input(bytes.into()));
    }

    fn window_size(&self) -> WindowSize {
        WindowSize {
            num_lines: self.grid.1 as u16,
            num_cols: self.grid.0 as u16,
            cell_width: f32::from(self.cell.0) as u16,
            cell_height: f32::from(self.cell.1) as u16,
        }
    }

    fn on_event(&mut self, e: TermEvent, cx: &mut Context<Self>) {
        match e {
            TermEvent::Wakeup => cx.notify(),
            TermEvent::PtyWrite(s) => self.write(s.into_bytes()),
            TermEvent::Title(t) => {
                self.title = Some(t);
                cx.notify();
            }
            TermEvent::ResetTitle => self.title = None,
            TermEvent::ClipboardStore(_, s) => cx.write_to_clipboard(ClipboardItem::new_string(s)),
            TermEvent::ClipboardLoad(_, f) => {
                let text = cx.read_from_clipboard().and_then(|i| i.text()).unwrap_or_default();
                self.write(f(&text).into_bytes());
            }
            TermEvent::TextAreaSizeRequest(f) => self.write(f(self.window_size()).into_bytes()),
            TermEvent::ColorRequest(i, f) => {
                let c = self.term.lock().colors()[i].unwrap_or_else(|| {
                    let v = if i < 256 { indexed(i) } else if i == NamedColor::Background as usize { BG } else { FG };
                    Rgb { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8 }
                });
                self.write(f(c).into_bytes());
            }
            TermEvent::ChildExit(status) => {
                self.exited = Some(status.code());
                cx.emit(TerminalEvent::Exited);
                cx.notify();
            }
            TermEvent::Exit => {
                if self.exited.is_none() {
                    self.exited = Some(None);
                    cx.emit(TerminalEvent::Exited);
                }
                cx.notify();
            }
            _ => {}
        }
    }

    fn resize(&mut self, bounds: Bounds<Pixels>) {
        self.origin = bounds.origin;
        let cols = ((bounds.size.width / self.cell.0).floor() as usize).max(2);
        let rows = ((bounds.size.height / self.cell.1).floor() as usize).max(1);
        if (cols, rows) != self.grid {
            self.grid = (cols, rows);
            self.term.lock().resize(TermSize::new(cols, rows));
            let _ = self.sender.send(Msg::Resize(self.window_size()));
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        if self.last_paste.is_some_and(|t| t.elapsed() < Duration::from_millis(150)) {
            return;
        }
        self.last_paste = Some(Instant::now());
        let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) else { return };
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let bracketed = self.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        let out = if bracketed { format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")) } else { text };
        self.scroll_bottom();
        self.write(out.into_bytes());
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(s) = self.term.lock().selection_to_string().filter(|s| !s.is_empty()) {
            cx.write_to_clipboard(ClipboardItem::new_string(s));
        }
    }

    fn scroll_bottom(&self) {
        let mut t = self.term.lock();
        t.scroll_display(Scroll::Bottom);
        t.selection = None;
    }

    fn key_bytes(k: &Keystroke, app_cursor: bool) -> Option<Vec<u8>> {
        let m = &k.modifiers;
        if m.platform {
            return None;
        }
        let arrow = |c: char| Some(if app_cursor && !m.shift && !m.control && !m.alt { format!("\x1bO{c}") } else if m.alt || m.control || m.shift {
            let n = 1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.control as u8;
            format!("\x1b[1;{n}{c}")
        } else {
            format!("\x1b[{c}")
        }
        .into_bytes());
        let s = match k.key.as_str() {
            "enter" => "\r",
            "backspace" => {
                return Some(if m.alt { b"\x1b\x7f".to_vec() } else if m.control { vec![0x08] } else { vec![0x7f] });
            }
            "tab" => {
                if m.shift {
                    "\x1b[Z"
                } else {
                    "\t"
                }
            }
            "escape" => "\x1b",
            "up" => return arrow('A'),
            "down" => return arrow('B'),
            "right" => return arrow('C'),
            "left" => return arrow('D'),
            "home" => return arrow('H'),
            "end" => return arrow('F'),
            "pageup" => "\x1b[5~",
            "pagedown" => "\x1b[6~",
            "delete" => "\x1b[3~",
            "insert" => "\x1b[2~",
            "f1" => "\x1bOP",
            "f2" => "\x1bOQ",
            "f3" => "\x1bOR",
            "f4" => "\x1bOS",
            "f5" => "\x1b[15~",
            "f6" => "\x1b[17~",
            "f7" => "\x1b[18~",
            "f8" => "\x1b[19~",
            "f9" => "\x1b[20~",
            "f10" => "\x1b[21~",
            "f11" => "\x1b[23~",
            "f12" => "\x1b[24~",
            "space" if m.control => return Some(vec![0]),
            key if m.control => {
                let ch = key.chars().next().filter(|_| key.chars().count() == 1)?;
                let b = match ch {
                    'a'..='z' => ch as u8 - b'a' + 1,
                    '@' | '2' => 0,
                    '[' | '3' => 0x1b,
                    '\\' | '4' => 0x1c,
                    ']' | '5' => 0x1d,
                    '^' | '6' => 0x1e,
                    '_' | '-' | '7' => 0x1f,
                    _ => return None,
                };
                return Some(if m.alt { vec![0x1b, b] } else { vec![b] });
            }
            _ => {
                let text = if m.alt { Some(k.key.clone()) } else { k.key_char.clone() };
                let text = text.or_else(|| (k.key == "space").then(|| " ".to_string()))?;
                let mut out = Vec::new();
                if m.alt {
                    out.push(0x1b);
                }
                out.extend_from_slice(text.as_bytes());
                return Some(out);
            }
        };
        Some(s.as_bytes().to_vec())
    }

    fn on_key(&mut self, e: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let k = &e.keystroke;
        if k.modifiers.platform {
            match k.key.as_str() {
                "v" => self.paste(cx),
                "c" => self.copy(cx),
                "k" => {
                    self.write(b"\x0c".as_slice());
                }
                _ => return,
            }
            cx.stop_propagation();
            return;
        }
        let app_cursor = self.term.lock().mode().contains(TermMode::APP_CURSOR);
        if let Some(bytes) = Self::key_bytes(k, app_cursor) {
            self.scroll_bottom();
            self.write(bytes);
            cx.stop_propagation();
        }
    }

    fn grid_point(&self, p: Point<Pixels>) -> (GridPoint, Side) {
        let t = self.term.lock();
        let x = (p.x - self.origin.x).max(px(0.));
        let y = (p.y - self.origin.y).max(px(0.));
        let col = ((x / self.cell.0) as usize).min(self.grid.0.saturating_sub(1));
        let row = ((y / self.cell.1) as usize).min(self.grid.1.saturating_sub(1));
        let side = if (x / self.cell.0).fract() < 0.5 { Side::Left } else { Side::Right };
        let line = Line(row as i32 - t.grid().display_offset() as i32);
        (GridPoint::new(line, Column(col)), side)
    }

    fn on_mouse_down(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let (p, side) = self.grid_point(e.position);
        let ty = match e.click_count {
            2 => SelectionType::Semantic,
            3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        self.term.lock().selection = Some(Selection::new(ty, p, side));
        self.selecting = true;
        cx.notify();
    }

    fn on_mouse_move(&mut self, e: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        let (p, side) = self.grid_point(e.position);
        if let Some(s) = self.term.lock().selection.as_mut() {
            s.update(p, side);
        }
        cx.notify();
    }

    fn on_scroll(&mut self, e: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let dy = e.delta.pixel_delta(self.cell.1).y;
        let lines = (dy / self.cell.1).round() as i32;
        if lines != 0 {
            let mut t = self.term.lock();
            if t.mode().contains(TermMode::ALT_SCREEN) {
                drop(t);
                let key = if lines > 0 { b"\x1b[A".as_slice() } else { b"\x1b[B".as_slice() };
                for _ in 0..lines.unsigned_abs().min(10) {
                    self.write(key);
                }
            } else {
                t.scroll_display(Scroll::Delta(lines));
            }
            cx.notify();
        }
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let font_size = px(FONT_SIZE);
        let font = gpui::font(MONO);
        let font_id = window.text_system().resolve_font(&font);
        let cw = window.text_system().advance(font_id, font_size, 'm').map(|s| s.width).unwrap_or(px(8.));
        self.cell = (cw, px(LINE_H));
        let focused = self.focus.is_focused(window);
        let bold = Font { weight: FontWeight::BOLD, ..font.clone() };
        let default_bg: Hsla = rgb(BG).into();

        let term = self.term.lock();
        let content = term.renderable_content();
        let colors = content.colors;
        let resolve = |c: AColor| -> Hsla {
            match c {
                AColor::Spec(rgbv) => rgb_hsla(rgbv),
                AColor::Indexed(i) => colors[i as usize].map(rgb_hsla).unwrap_or_else(|| rgb(indexed(i as usize)).into()),
                AColor::Named(n) => {
                    let i = n as usize;
                    if let Some(v) = colors[i] {
                        return rgb_hsla(v);
                    }
                    match n {
                        NamedColor::Background => default_bg,
                        NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::Cursor => rgb(FG).into(),
                        NamedColor::DimForeground => Hsla::from(rgb(FG)).opacity(0.7),
                        _ if i < 16 => rgb(PALETTE[i]).into(),
                        _ if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&i) => {
                            Hsla::from(rgb(PALETTE[i - NamedColor::DimBlack as usize])).opacity(0.7)
                        }
                        _ => rgb(FG).into(),
                    }
                }
            }
        };
        let rows = term.screen_lines();
        let offset = content.display_offset as i32;
        let cursor = content.cursor;
        let show_cursor = cursor.shape != CursorShape::Hidden && offset == 0;
        let selection = content.selection;
        let mut lines: Vec<(String, Vec<TextRun>)> = (0..rows).map(|_| (String::new(), Vec::new())).collect();
        for cell in content.display_iter {
            let row = (cell.point.line.0 + offset) as usize;
            if row >= rows || cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let mut fg = resolve(cell.fg);
            let mut bg = resolve(cell.bg);
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if cell.flags.contains(Flags::DIM) {
                fg = fg.opacity(0.7);
            }
            if cell.flags.contains(Flags::HIDDEN) {
                fg = bg;
            }
            if selection.is_some_and(|s| s.contains(cell.point)) {
                bg = Hsla::from(rgb(0x264f78));
            }
            if show_cursor && cell.point == cursor.point {
                if focused && cursor.shape == CursorShape::Block {
                    fg = default_bg;
                    bg = rgb(FG).into();
                } else {
                    bg = Hsla::from(rgb(FG)).opacity(0.35);
                }
            }
            let ch = if cell.c == '\0' || cell.c == '\t' { ' ' } else { cell.c };
            let (text, runs) = &mut lines[row];
            let len = ch.len_utf8();
            text.push(ch);
            let f = if cell.flags.contains(Flags::BOLD) { bold.clone() } else { font.clone() };
            let underline = cell.flags.intersects(Flags::ALL_UNDERLINES).then(|| UnderlineStyle { color: Some(fg), thickness: px(1.), wavy: false });
            let bg_opt = (bg != default_bg).then_some(bg);
            match runs.last_mut() {
                Some(r) if r.color == fg && r.background_color == bg_opt && r.font == f && r.underline == underline => r.len += len,
                _ => runs.push(TextRun { len, font: f, color: fg, background_color: bg_opt, underline, strikethrough: None }),
            }
        }
        drop(term);

        let entity = cx.entity();
        div()
            .id("terminal")
            .key_context("Terminal")
            .track_focus(&self.focus)
            .size_full()
            .bg(default_bg)
            .p(px(8.))
            .font_family(MONO)
            .text_size(font_size)
            .line_height(px(LINE_H))
            .on_key_down(cx.listener(Self::on_key))
            .on_action(cx.listener(|this, _: &crate::text_input::Paste, _, cx| this.paste(cx)))
            .on_action(cx.listener(|this, _: &crate::text_input::Copy, _, cx| this.copy(cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, _| this.selecting = false))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(
                div()
                    .relative()
                    .size_full()
                    .overflow_hidden()
                    .child(
                        canvas(
                            move |bounds, window, cx| {
                                let changed = entity.update(cx, |this, _| {
                                    let before = this.grid;
                                    this.resize(bounds);
                                    before != this.grid
                                });
                                if changed {
                                    window.refresh();
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .children(lines.into_iter().map(|(text, runs)| {
                        div().h(px(LINE_H)).whitespace_nowrap().child(StyledText::new(SharedString::from(text)).with_runs(runs))
                    })),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_palette() {
        let k = |s: &str| Keystroke::parse(s).unwrap();
        assert_eq!(TerminalView::key_bytes(&k("ctrl-c"), false), Some(vec![3]));
        assert_eq!(TerminalView::key_bytes(&k("up"), false), Some(b"\x1b[A".to_vec()));
        assert_eq!(TerminalView::key_bytes(&k("up"), true), Some(b"\x1bOA".to_vec()));
        assert_eq!(TerminalView::key_bytes(&k("enter"), false), Some(b"\r".to_vec()));
        assert_eq!(TerminalView::key_bytes(&k("cmd-c"), false), None);
        assert_eq!(indexed(16), 0x000000);
        assert_eq!(indexed(231), 0xffffff);
        assert_eq!(indexed(232), 0x080808);
    }

    #[test]
    fn runs_a_program_in_a_pty() {
        let mut s = TerminalView::spawn("/bin/sh".into(), vec!["-c".into(), "printf 'hello \\033[31mred\\033[0m'; [ -t 0 ] && printf ' tty'".into()]).unwrap();
        let exited = futures::executor::block_on(async {
            while let Some(e) = s.rx.next().await {
                if matches!(e, TermEvent::ChildExit(_)) {
                    return true;
                }
            }
            false
        });
        assert!(exited);
        std::thread::sleep(Duration::from_millis(100));
        let t = s.term.lock();
        let row: String = (0..20).map(|c| t.grid()[Line(0)][Column(c)].c).collect();
        assert_eq!(row.trim_end(), "hello red tty");
        assert_eq!(t.grid()[Line(0)][Column(6)].fg, AColor::Named(NamedColor::Red));
        drop(t);
        let _ = s.sender.send(Msg::Shutdown);
    }
}
