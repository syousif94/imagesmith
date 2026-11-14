//! Markdown for assistant replies: pulldown-cmark parses into a small block tree that is cached
//! per message, then rendered with gpui `StyledText` highlights.

use crate::theme::{Colors, MONO};
use gpui::prelude::*;
use gpui::{
    div, px, AnyElement, ElementId, FontStyle, FontWeight, HighlightStyle, InteractiveText, SharedString, StrikethroughStyle, StyledText,
    UnderlineStyle,
};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;

#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Span {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    pub link: bool,
}

#[derive(Default, Debug, PartialEq)]
pub struct Inline {
    pub text: SharedString,
    pub spans: Vec<(Range<usize>, Span)>,
    pub links: Vec<(Range<usize>, String)>,
}

#[derive(Debug, PartialEq)]
pub enum Block {
    Text(Inline),
    Heading(u8, Inline),
    Code(SharedString),
    List { start: Option<u64>, items: Vec<(Option<bool>, Vec<Block>)> },
    Quote(Vec<Block>),
    Rule,
    Table { head: Vec<Inline>, rows: Vec<Vec<Inline>> },
}

#[derive(Default)]
struct InlineBuf {
    text: String,
    spans: Vec<(Range<usize>, Span)>,
    links: Vec<(Range<usize>, String)>,
}

impl InlineBuf {
    fn push(&mut self, s: &str, span: Span) {
        let start = self.text.len();
        self.text.push_str(s);
        if span != Span::default() && !s.is_empty() {
            match self.spans.last_mut() {
                Some((r, last)) if r.end == start && *last == span => r.end = self.text.len(),
                _ => self.spans.push((start..self.text.len(), span)),
            }
        }
    }

    fn finish(self) -> Inline {
        Inline { text: self.text.into(), spans: self.spans, links: self.links }
    }
}

enum Frame {
    Blocks(Vec<Block>),
    Quote(Vec<Block>),
    List(Option<u64>, Vec<(Option<bool>, Vec<Block>)>),
    Item(Option<bool>, Vec<Block>),
    Table(Vec<Inline>, Vec<Vec<Inline>>),
    Row(Vec<Inline>),
}

struct Builder {
    stack: Vec<Frame>,
    cur: Option<InlineBuf>,
    code: Option<String>,
    span: Span,
    depth: [u32; 3],
    link: Vec<(usize, String)>,
}

impl Builder {
    fn blocks(&mut self) -> &mut Vec<Block> {
        for f in self.stack.iter_mut().rev() {
            if let Frame::Blocks(b) | Frame::Quote(b) | Frame::Item(_, b) = f {
                return b;
            }
        }
        unreachable!("root frame is always Blocks")
    }

    fn inline(&mut self) -> &mut InlineBuf {
        self.cur.get_or_insert_with(InlineBuf::default)
    }

    fn take_inline(&mut self) -> Inline {
        self.cur.take().unwrap_or_default().finish()
    }

    fn flush(&mut self) {
        if let Some(buf) = self.cur.take() {
            if !buf.text.trim().is_empty() {
                let b = Block::Text(buf.finish());
                self.blocks().push(b);
            }
        }
    }

    fn text(&mut self, s: &str) {
        if let Some(code) = &mut self.code {
            code.push_str(s);
        } else {
            let span = self.span;
            self.inline().push(s, span);
        }
    }

    fn set_style(&mut self, which: usize, on: bool) {
        let d = &mut self.depth[which];
        *d = if on { *d + 1 } else { d.saturating_sub(1) };
        self.span.italic = self.depth[0] > 0;
        self.span.bold = self.depth[1] > 0;
        self.span.strike = self.depth[2] > 0;
    }

    fn event(&mut self, e: Event) {
        match e {
            Event::Start(tag) => match tag {
                Tag::Paragraph | Tag::Heading { .. } | Tag::TableCell => self.flush(),
                Tag::BlockQuote(_) => {
                    self.flush();
                    self.stack.push(Frame::Quote(Vec::new()));
                }
                Tag::CodeBlock(_) => {
                    self.flush();
                    self.code = Some(String::new());
                }
                Tag::List(start) => {
                    self.flush();
                    self.stack.push(Frame::List(start, Vec::new()));
                }
                Tag::Item => {
                    self.flush();
                    self.stack.push(Frame::Item(None, Vec::new()));
                }
                Tag::Table(_) => {
                    self.flush();
                    self.stack.push(Frame::Table(Vec::new(), Vec::new()));
                }
                Tag::TableHead | Tag::TableRow => self.stack.push(Frame::Row(Vec::new())),
                Tag::Emphasis => self.set_style(0, true),
                Tag::Strong => self.set_style(1, true),
                Tag::Strikethrough => self.set_style(2, true),
                Tag::Link { dest_url, .. } => {
                    let at = self.inline().text.len();
                    self.link.push((at, dest_url.to_string()));
                    self.span.link = true;
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => self.flush(),
                TagEnd::Heading(level) => {
                    let inline = self.take_inline();
                    self.blocks().push(Block::Heading(level as u8, inline));
                }
                TagEnd::BlockQuote(_) => {
                    self.flush();
                    if let Some(Frame::Quote(b)) = self.stack.pop() {
                        self.blocks().push(Block::Quote(b));
                    }
                }
                TagEnd::CodeBlock => {
                    let code = self.code.take().unwrap_or_default();
                    self.blocks().push(Block::Code(code.trim_end_matches('\n').to_string().into()));
                }
                TagEnd::Item => {
                    self.flush();
                    if let Some(Frame::Item(task, b)) = self.stack.pop() {
                        if let Some(Frame::List(_, items)) = self.stack.last_mut() {
                            items.push((task, b));
                        }
                    }
                }
                TagEnd::List(_) => {
                    if let Some(Frame::List(start, items)) = self.stack.pop() {
                        self.blocks().push(Block::List { start, items });
                    }
                }
                TagEnd::TableCell => {
                    let cell = self.take_inline();
                    if let Some(Frame::Row(cells)) = self.stack.last_mut() {
                        cells.push(cell);
                    }
                }
                TagEnd::TableHead | TagEnd::TableRow => {
                    if let (Some(Frame::Row(cells)), Some(Frame::Table(head, rows))) = (self.stack.pop(), self.stack.last_mut()) {
                        if matches!(tag, TagEnd::TableHead) {
                            *head = cells;
                        } else {
                            rows.push(cells);
                        }
                    }
                }
                TagEnd::Table => {
                    if let Some(Frame::Table(head, rows)) = self.stack.pop() {
                        self.blocks().push(Block::Table { head, rows });
                    }
                }
                TagEnd::Emphasis => self.set_style(0, false),
                TagEnd::Strong => self.set_style(1, false),
                TagEnd::Strikethrough => self.set_style(2, false),
                TagEnd::Link => {
                    if let Some((start, url)) = self.link.pop() {
                        let buf = self.inline();
                        let end = buf.text.len();
                        if end > start {
                            buf.links.push((start..end, url));
                        }
                    }
                    self.span.link = !self.link.is_empty();
                }
                _ => {}
            },
            Event::Text(t) | Event::Html(t) | Event::InlineHtml(t) => self.text(&t),
            Event::Code(t) => {
                let span = Span { code: true, ..self.span };
                self.inline().push(&t, span);
            }
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.text("\n"),
            Event::Rule => {
                self.flush();
                self.blocks().push(Block::Rule);
            }
            Event::TaskListMarker(done) => {
                if let Some(Frame::Item(task, _)) = self.stack.last_mut() {
                    *task = Some(done);
                }
            }
            _ => {}
        }
    }
}

pub fn parse(text: &str) -> Vec<Block> {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut b = Builder { stack: vec![Frame::Blocks(Vec::new())], cur: None, code: None, span: Span::default(), depth: [0; 3], link: Vec::new() };
    for e in Parser::new_ext(text, opts) {
        b.event(e);
    }
    b.flush();
    // pulldown-cmark closes every open tag at EOF, so only the root frame is left.
    match b.stack.pop() {
        Some(Frame::Blocks(blocks)) => blocks,
        _ => Vec::new(),
    }
}

/// Parsed messages keyed by (chat id, item index); reparsed only when the text changes.
#[derive(Default)]
pub struct Cache(HashMap<(u64, usize), (u64, Rc<Vec<Block>>)>);

impl Cache {
    pub fn get(&mut self, key: (u64, usize), text: &str) -> Rc<Vec<Block>> {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut h);
        let h = h.finish();
        match self.0.get(&key) {
            Some((old, blocks)) if *old == h => blocks.clone(),
            _ => {
                let blocks = Rc::new(parse(text));
                self.0.insert(key, (h, blocks.clone()));
                blocks
            }
        }
    }
}

pub fn render(blocks: &[Block], id: SharedString, c: &Colors) -> AnyElement {
    let mut n = 0;
    div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .text_size(px(13.))
        .line_height(px(19.))
        .children(blocks.iter().map(|b| block(b, &id, &mut n, c)))
        .into_any_element()
}

fn block(b: &Block, id: &SharedString, n: &mut usize, c: &Colors) -> AnyElement {
    match b {
        Block::Text(i) => div().child(inline(i, id, n, c)).into_any_element(),
        Block::Heading(level, i) => div()
            .font_weight(FontWeight::SEMIBOLD)
            .text_size(px(match level {
                1 => 16.,
                2 => 15.,
                _ => 13.5,
            }))
            .line_height(px(22.))
            .child(inline(i, id, n, c))
            .into_any_element(),
        Block::Code(code) => div()
            .px(px(8.))
            .py(px(6.))
            .rounded(px(6.))
            .bg(c.surface)
            .border_1()
            .border_color(c.separator)
            .font_family(MONO)
            .text_size(px(11.5))
            .line_height(px(16.))
            .child(code.clone())
            .into_any_element(),
        Block::List { start, items } => div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .children(items.iter().enumerate().map(|(k, (task, body))| {
                let marker: SharedString = match (task, start) {
                    (Some(true), _) => "☑".into(),
                    (Some(false), _) => "☐".into(),
                    (None, Some(s)) => format!("{}.", s + k as u64).into(),
                    (None, None) => "•".into(),
                };
                div()
                    .flex()
                    .gap(px(6.))
                    .child(div().flex_none().min_w(px(14.)).text_color(c.muted).child(marker))
                    .child(div().flex_1().min_w_0().flex().flex_col().gap(px(6.)).children(body.iter().map(|b| block(b, id, n, c))))
            }))
            .into_any_element(),
        Block::Quote(body) => div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .pl(px(10.))
            .border_l_2()
            .border_color(c.separator)
            .text_color(c.muted)
            .children(body.iter().map(|b| block(b, id, n, c)))
            .into_any_element(),
        Block::Rule => div().h(px(1.)).bg(c.separator).into_any_element(),
        Block::Table { head, rows } => {
            let row = |cells: &[Inline], header: bool, n: &mut usize| {
                div()
                    .flex()
                    .when(header, |d| d.bg(c.group).font_weight(FontWeight::SEMIBOLD))
                    .children(cells.iter().map(|i| div().flex_1().min_w_0().px(px(6.)).py(px(3.)).child(inline(i, id, n, c))).collect::<Vec<_>>())
            };
            let mut t = div().flex().flex_col().rounded(px(6.)).border_1().border_color(c.separator).overflow_hidden();
            if !head.is_empty() {
                t = t.child(row(head, true, n));
            }
            for r in rows {
                t = t.child(div().border_t_1().border_color(c.separator).child(row(r, false, n)));
            }
            t.into_any_element()
        }
    }
}

fn inline(i: &Inline, id: &SharedString, n: &mut usize, c: &Colors) -> AnyElement {
    let hl = i.spans.iter().map(|(r, s)| {
        let mut h = HighlightStyle::default();
        if s.bold {
            h.font_weight = Some(FontWeight::SEMIBOLD);
        }
        if s.italic {
            h.font_style = Some(FontStyle::Italic);
        }
        if s.strike {
            h.strikethrough = Some(StrikethroughStyle { thickness: px(1.), color: None });
        }
        if s.code {
            h.background_color = Some(c.surface_alt);
        }
        if s.link {
            h.color = Some(c.accent);
            h.underline = Some(UnderlineStyle { thickness: px(1.), color: Some(c.accent), wavy: false });
        }
        (r.clone(), h)
    });
    let text = StyledText::new(i.text.clone()).with_highlights(hl.collect::<Vec<_>>());
    if i.links.is_empty() {
        return text.into_any_element();
    }
    *n += 1;
    let urls: Vec<String> = i.links.iter().map(|(_, u)| u.clone()).collect();
    InteractiveText::new(ElementId::NamedInteger(id.clone(), *n as u64), text)
        .on_click(i.links.iter().map(|(r, _)| r.clone()).collect(), move |ix, _, cx| {
            let url = &urls[ix];
            if url.starts_with("https://") || url.starts_with("http://") || url.starts_with("mailto:") {
                cx.open_url(url);
            }
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(b: &Block) -> &str {
        match b {
            Block::Text(i) | Block::Heading(_, i) => &i.text,
            _ => panic!("not text: {b:?}"),
        }
    }

    #[test]
    fn inline_styles_and_links() {
        let b = parse("Use **apt** and `curl`, see [docs](https://x.y).");
        assert_eq!(b.len(), 1);
        let Block::Text(i) = &b[0] else { panic!() };
        assert_eq!(i.text.as_ref(), "Use apt and curl, see docs.");
        assert_eq!(i.spans[0], (4..7, Span { bold: true, ..Default::default() }));
        assert_eq!(i.spans[1], (12..16, Span { code: true, ..Default::default() }));
        assert_eq!(i.links, vec![(22..26, "https://x.y".to_string())]);
    }

    #[test]
    fn blocks() {
        let b = parse("# Title\n\n- a\n- [x] b\n\n1. one\n2. two\n\n```sh\necho hi\n```\n\n> quoted\n\n---\n\n| A | B |\n|---|---|\n| 1 | 2 |\n");
        assert_eq!(text(&b[0]), "Title");
        let Block::List { start: None, items } = &b[1] else { panic!("{:?}", b[1]) };
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].0, Some(true));
        assert_eq!(text(&items[0].1[0]), "a");
        assert!(matches!(&b[2], Block::List { start: Some(1), items } if items.len() == 2));
        assert!(matches!(&b[3], Block::Code(c) if c.as_ref() == "echo hi"));
        assert!(matches!(&b[4], Block::Quote(q) if text(&q[0]) == "quoted"));
        assert!(matches!(b[5], Block::Rule));
        let Block::Table { head, rows } = &b[6] else { panic!() };
        assert_eq!(head.len(), 2);
        assert_eq!(rows[0][1].text.as_ref(), "2");
    }

    #[test]
    fn unterminated_while_streaming() {
        let b = parse("Run:\n\n```sh\napt install");
        assert!(matches!(&b[1], Block::Code(c) if c.as_ref() == "apt install"));
        let b = parse("- one\n- tw");
        assert!(matches!(&b[0], Block::List { items, .. } if items.len() == 2));
    }

    #[test]
    fn cache_reuses_until_text_changes() {
        let mut c = Cache::default();
        let a = c.get((1, 0), "hi");
        assert!(Rc::ptr_eq(&a, &c.get((1, 0), "hi")));
        assert!(!Rc::ptr_eq(&a, &c.get((1, 0), "hi there")));
    }
}
