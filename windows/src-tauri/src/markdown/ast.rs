//! Build a typed block/inline tree from pulldown-cmark's event stream,
//! mirroring the shapes `ASTConverter.swift` consumes from swift-markdown
//! (Heading / Paragraph / BlockQuote / lists / CodeBlock / ThematicBreak /
//! Table / HTMLBlock; Text / Strong / Emphasis / Strikethrough / InlineCode /
//! Link / Image / SoftBreak / HardBreak / InlineHTML).
//!
//! Every block carries its byte `range` in the source so the math-block
//! sniffer can recover the verbatim `$$…$$` text (the parsed inline tree
//! mangles LaTeX backslashes — same workaround as the Swift side, except we
//! slice by byte range instead of 1-based lines).

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::ops::Range;

#[derive(Debug)]
pub enum Block {
    Heading {
        level: u32,
        children: Vec<Inline>,
    },
    Paragraph {
        children: Vec<Inline>,
        range: Range<usize>,
    },
    BlockQuote {
        children: Vec<Block>,
    },
    List {
        /// `Some(start)` for ordered lists, `None` for bullet lists.
        start: Option<u64>,
        items: Vec<ListItem>,
    },
    CodeBlock {
        lang: Option<String>,
        code: String,
    },
    ThematicBreak,
    Table {
        head: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
    },
    HtmlBlock {
        raw: String,
    },
}

#[derive(Debug)]
pub struct ListItem {
    /// `Some(checked)` when the item carries a GFM `[ ]` / `[x]` marker.
    pub checkbox: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Debug)]
pub enum Inline {
    Text(String),
    Strong(Vec<Inline>),
    Emphasis(Vec<Inline>),
    Strikethrough(Vec<Inline>),
    Code(String),
    Link {
        dest: String,
        title: String,
        children: Vec<Inline>,
    },
    Image {
        dest: String,
        title: String,
        alt: String,
    },
    SoftBreak,
    HardBreak,
    InlineHtml(String),
}

/// Parse markdown body into a block tree. GFM tables / strikethrough /
/// tasklists on; math OFF — `$$` handling replicates the Swift paragraph
/// sniffer instead, so both platforms agree byte-for-byte.
pub fn parse_blocks(source: &str) -> Vec<Block> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(source, options).into_offset_iter();
    let mut events = parser.peekable();
    let blocks = build_blocks(&mut events, None);
    blocks
}

type Ev<'a> = std::iter::Peekable<pulldown_cmark::OffsetIter<'a>>;

/// Consume events until `until` is reached (consuming its End) or the stream
/// ends, building blocks. Consecutive `Html` events merge into one HtmlBlock
/// (pulldown splits a single HTML block into several events).
fn build_blocks(events: &mut Ev, until: Option<TagEnd>) -> Vec<Block> {
    let mut blocks = Vec::new();
    while let Some((event, range)) = events.next() {
        // Tight list items (CommonMark): content arrives as bare inline
        // events with no Paragraph wrapper. Collect the run into an
        // implicit Paragraph block, mirroring swift-markdown's tree.
        if is_inline_event(&event) {
            let children = build_tight_inlines(events, event);
            blocks.push(Block::Paragraph { children, range });
            continue;
        }
        match event {
            Event::End(tag) => {
                // Compare by discriminant: payload-carrying ends (e.g.
                // `BlockQuote(Option<BlockQuoteKind>)`) must still match.
                if let Some(u) = &until {
                    if std::mem::discriminant(u) == std::mem::discriminant(&tag) {
                        return blocks;
                    }
                }
                // A stray End at the top level is ignored.
            }
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    let children = build_inlines(events, TagEnd::Paragraph);
                    blocks.push(Block::Paragraph {
                        children,
                        range: range.clone(),
                    });
                }
                Tag::Heading { level, .. } => {
                    let children = build_inlines(events, TagEnd::Heading(level));
                    blocks.push(Block::Heading {
                        level: level as u32,
                        children,
                    });
                }
                Tag::BlockQuote(_) => {
                    let children = build_blocks(events, Some(TagEnd::BlockQuote(None)));
                    blocks.push(Block::BlockQuote { children });
                }
                Tag::List(start) => {
                    let items = build_list_items(events);
                    blocks.push(Block::List { start, items });
                }
                Tag::CodeBlock(kind) => {
                    let lang = match kind {
                        pulldown_cmark::CodeBlockKind::Fenced(l) if !l.is_empty() => {
                            Some(l.to_string())
                        }
                        _ => None,
                    };
                    let mut code = String::new();
                    loop {
                        match events.next() {
                            Some((Event::Text(t), _)) => code.push_str(&t),
                            Some((Event::End(TagEnd::CodeBlock), _)) => break,
                            Some(_) => {}
                            None => break,
                        }
                    }
                    blocks.push(Block::CodeBlock { lang, code });
                }
                Tag::Table(_) => {
                    let (head, rows) = build_table(events);
                    blocks.push(Block::Table { head, rows });
                }
                // Containers we don't model: fallthrough shouldn't happen at
                // block level, but recurse defensively to keep the stream
                // aligned.
                _ => {}
            },
            Event::Rule => blocks.push(Block::ThematicBreak),
            Event::Html(html) => {
                let mut raw = html.to_string();
                while let Some((Event::Html(_), _)) = events.peek() {
                    if let Some((Event::Html(more), _)) = events.next() {
                        raw.push_str(&more);
                    }
                }
                blocks.push(Block::HtmlBlock { raw });
            }
            // Inline-level events at block level: ignore (shouldn't happen).
            _ => {}
        }
    }
    blocks
}

fn build_list_items(events: &mut Ev) -> Vec<ListItem> {
    let mut items = Vec::new();
    loop {
        match events.peek() {
            Some((Event::Start(Tag::Item), _)) => {
                events.next();
                let mut checkbox = None;
                // A TaskListMarker event, when present, is the item's first.
                if let Some((Event::TaskListMarker(checked), _)) = events.peek() {
                    checkbox = Some(*checked);
                    events.next();
                }
                let blocks = build_blocks(events, Some(TagEnd::Item));
                items.push(ListItem { checkbox, blocks });
            }
            Some((Event::End(TagEnd::List(_)), _)) => {
                events.next();
                break;
            }
            Some(_) => {
                events.next();
            }
            None => break,
        }
    }
    items
}

fn build_table(events: &mut Ev) -> (Vec<Vec<Inline>>, Vec<Vec<Vec<Inline>>>) {
    let mut head: Vec<Vec<Inline>> = Vec::new();
    let mut rows: Vec<Vec<Vec<Inline>>> = Vec::new();
    loop {
        match events.next() {
            Some((Event::Start(Tag::TableHead), _)) => {
                // Header cells arrive as TableCell tags directly under
                // TableHead (no TableRow wrapper).
                loop {
                    match events.next() {
                        Some((Event::Start(Tag::TableCell), _)) => {
                            head.push(build_inlines(events, TagEnd::TableCell));
                        }
                        Some((Event::End(TagEnd::TableHead), _)) => break,
                        Some(_) => {}
                        None => break,
                    }
                }
            }
            Some((Event::Start(Tag::TableRow), _)) => {
                let mut row = Vec::new();
                loop {
                    match events.next() {
                        Some((Event::Start(Tag::TableCell), _)) => {
                            row.push(build_inlines(events, TagEnd::TableCell));
                        }
                        Some((Event::End(TagEnd::TableRow), _)) => break,
                        Some(_) => {}
                        None => break,
                    }
                }
                rows.push(row);
            }
            Some((Event::End(TagEnd::Table), _)) => break,
            Some(_) => {}
            None => break,
        }
    }
    (head, rows)
}

fn build_inlines(events: &mut Ev, until: TagEnd) -> Vec<Inline> {
    let mut out = Vec::new();
    while let Some((event, _)) = events.next() {
        if matches!(&event, Event::End(tag) if tag == &until) {
            return out;
        }
        consume_inline(event, events, &mut out);
    }
    out
}

/// True for events that live inside a paragraph (inline content). Block-level
/// tags, `Html`, `Rule`, and `End` return false.
fn is_inline_event(event: &Event) -> bool {
    match event {
        Event::Text(_)
        | Event::Code(_)
        | Event::InlineHtml(_)
        | Event::SoftBreak
        | Event::HardBreak
        | Event::FootnoteReference(_)
        | Event::InlineMath(_)
        | Event::DisplayMath(_) => true,
        Event::Start(tag) => matches!(
            tag,
            Tag::Emphasis
                | Tag::Strong
                | Tag::Strikethrough
                | Tag::Superscript
                | Tag::Subscript
                | Tag::Link { .. }
                | Tag::Image { .. }
        ),
        _ => false,
    }
}

/// Collect a run of inline events (starting with the already-consumed `first`)
/// until a block boundary or a foreign container End, which is left in the
/// stream for the block layer.
fn build_tight_inlines<'a>(events: &mut Ev<'a>, first: Event<'a>) -> Vec<Inline> {
    let mut out = Vec::new();
    consume_inline(first, events, &mut out);
    loop {
        let stop = match events.peek() {
            None => true,
            Some((Event::End(_), _)) => true,
            Some((event, _)) => !is_inline_event(event),
        };
        if stop {
            break;
        }
        let (event, _) = events.next().unwrap();
        consume_inline(event, events, &mut out);
    }
    out
}

/// Append one inline event to `out`, recursing into inline containers.
/// Non-inline events are dropped (the caller filters via `is_inline_event`).
fn consume_inline<'a>(event: Event<'a>, events: &mut Ev<'a>, out: &mut Vec<Inline>) {
    match event {
        Event::Text(t) => out.push(Inline::Text(t.to_string())),
        Event::Code(c) => out.push(Inline::Code(c.to_string())),
        Event::InlineHtml(h) => out.push(Inline::InlineHtml(h.to_string())),
        Event::SoftBreak => out.push(Inline::SoftBreak),
        Event::HardBreak => out.push(Inline::HardBreak),
        Event::Start(tag) => match tag {
            Tag::Emphasis => out.push(Inline::Emphasis(build_inlines(events, TagEnd::Emphasis))),
            Tag::Strong => out.push(Inline::Strong(build_inlines(events, TagEnd::Strong))),
            Tag::Strikethrough => {
                out.push(Inline::Strikethrough(build_inlines(events, TagEnd::Strikethrough)))
            }
            Tag::Link {
                dest_url, title, ..
            } => {
                let children = build_inlines(events, TagEnd::Link);
                out.push(Inline::Link {
                    dest: dest_url.to_string(),
                    title: title.to_string(),
                    children,
                });
            }
            Tag::Image {
                dest_url, title, ..
            } => {
                // Image children are the alt text's inline events.
                let alt_events = build_inlines(events, TagEnd::Image);
                out.push(Inline::Image {
                    dest: dest_url.to_string(),
                    title: title.to_string(),
                    alt: plain_text(&alt_events),
                });
            }
            _ => {}
        },
        _ => {}
    }
}

/// Concatenated text of an inline subtree — used for image alt
/// (`Image.plainText` in swift-markdown).
fn plain_text(inlines: &[Inline]) -> String {
    let mut s = String::new();
    for i in inlines {
        match i {
            Inline::Text(t) => s.push_str(t),
            Inline::Code(c) => s.push_str(c),
            Inline::Strong(c) | Inline::Emphasis(c) | Inline::Strikethrough(c) => {
                s.push_str(&plain_text(c))
            }
            Inline::Link { children, .. } => s.push_str(&plain_text(children)),
            _ => {}
        }
    }
    s
}
