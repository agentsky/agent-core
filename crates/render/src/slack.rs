//! Markdown to Slack mrkdwn.
//!
//! The behavioral reference is qm-core's `toSlackMrkdwn` in
//! `src/slack/mrkdwn.ts`. Where CommonMark parsing and qm-core's regexes
//! disagree, this module follows the parse tree; `docs/impl-notes.md` lists
//! the differences.

use std::ops::Range;

use core_types::{LengthUnit, Limit};
use pulldown_cmark::{
    Alignment, CodeBlockKind, CowStr, Event, LinkType, Options, Parser, Tag, TagEnd,
};

use crate::url::{bare_url, ends_url};
use crate::{MentionDirectory, mention};

/// The most text one Slack message chunk holds: 3,000 characters, under
/// Slack's 4,000-character limit for a message's `text`.
///
/// Split the output of [`to_mrkdwn`] with it.
pub const MESSAGE_LIMIT: Limit = Limit {
    max: 3_000,
    unit: LengthUnit::Chars,
};

/// Names Slack treats as broadcasts to a whole channel or workspace.
const BROADCASTS: &[&str] = &["here", "channel", "everyone"];

/// What a thematic break (`---`) becomes. mrkdwn has no divider.
const RULE: &str = "──────────";

/// Inserted after the `@` of a broadcast so it can't notify anyone, around
/// escaped formatting characters so they can't pair up, and inside backtick
/// runs so they can't close a code block.
const ZERO_WIDTH_SPACE: char = '\u{200B}';

/// The characters Slack reads as formatting marks.
const DELIMITERS: [char; 4] = ['*', '_', '~', '`'];

/// Slack only knows backtick fences.
const FENCE: &str = "```";

/// How many bytes past `<!here` a closing `>` is looked for. A longer
/// label is left as escaped text, which is just as harmless.
const MAX_WIRE_LABEL: usize = 256;

/// Elements nested deeper than this are flattened into their ancestor at
/// this depth. Rendering and dropping the tree recurse once per level, so an
/// unbounded depth (a line of 100,000 `>`) would overflow the stack.
const MAX_DEPTH: usize = 64;

/// Converts agent-written Markdown to Slack mrkdwn.
///
/// - Headings become bold lines.
/// - `**bold**` and `__bold__` become `*bold*`, `*em*` and `_em_` become
///   `_em_`, and `~~strike~~` becomes `~strike~`. Slack doesn't format inside
///   a word, so emphasis touching a letter or digit (`5*3*2`) keeps its
///   Markdown delimiter character instead.
/// - `*`, `_`, `~` and `` ` `` written as backslash escapes or character
///   references get zero-width spaces around them, unless a letter or digit
///   sits on both sides, so Slack shows them instead of formatting with them.
///   So do those characters in image alt text.
/// - Inline and fenced code keep their contents; only `&`, `<` and `>` are
///   escaped. A fenced block keeps its info string, and gets a zero-width
///   space into any run of three backticks inside it, so the run can't close
///   the block.
/// - `[label](url)` becomes `<url|label>`, and images become links to their
///   source. A label that names a different host than the URL
///   (`[good.com](https://evil.com)`) is shown next to the link instead:
///   `good.com (<https://evil.com>)`. A link with an empty URL shows only its
///   label. Bare `http(s)` URLs get explicit `<url>` boundaries, so Slack
///   doesn't pull neighboring punctuation or formatting marks into them.
///   Emphasis-shaped runs inside a URL's path (`/__main__.html`) stay part
///   of the URL.
/// - List items become `•` or numbered lines, indented two spaces per
///   nesting level. Blockquotes keep their `>` prefix on every line.
/// - Tables become aligned plain text inside a code block.
/// - `&`, `<` and `>` are escaped everywhere, including inside code, so text
///   can never form a Slack control sequence such as `<!here>`.
/// - `@Name` outside code becomes `<@id>` when `directory` resolves it, and
///   stays text otherwise.
/// - Broadcasts are neutralized: typed `@here`, `@channel` and `@everyone`,
///   and literal `<!here>`, `<!channel>` and `<!everyone>`, become
///   `@here`-style text with a zero-width space after the `@`. Inside code
///   they are left alone, escaping aside.
///
/// Blocks are separated by a blank line when the source had one between
/// them, and by a line break otherwise.
///
/// # Examples
///
/// ```
/// use render::{MentionDirectory, slack::to_mrkdwn};
///
/// struct Team;
///
/// impl MentionDirectory for Team {
///     fn resolve(&self, name: &str) -> Option<String> {
///         name.eq_ignore_ascii_case("ada").then(|| "U123".to_string())
///     }
/// }
///
/// assert_eq!(
///     to_mrkdwn("**Thanks** @Ada, see [the docs](https://example.com) & `a < b`", &Team),
///     "*Thanks* <@U123>, see <https://example.com|the docs> &amp; `a &lt; b`",
/// );
/// ```
pub fn to_mrkdwn(md: &str, directory: &dyn MentionDirectory) -> String {
    let renderer = Renderer {
        src: md,
        line_starts: std::iter::once(0)
            .chain(md.match_indices('\n').map(|(i, _)| i + 1))
            .collect(),
        directory,
    };
    let lines = renderer.blocks(&parse(md));
    lines
        .into_iter()
        .map(|line| line.text)
        .collect::<Vec<_>>()
        .join("\n")
}

struct Node<'a> {
    kind: Kind<'a>,
    span: Range<usize>,
}

enum Kind<'a> {
    Elem(Tag<'a>, Vec<Node<'a>>),
    /// Text, and the byte offsets in it of formatting characters the source
    /// escaped.
    Text(String, Vec<usize>),
    Code(CowStr<'a>),
    Html(CowStr<'a>),
    Break,
    Rule,
    Other,
}

fn parse(md: &str) -> Vec<Node<'_>> {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let mut root = Vec::new();
    let mut open: Vec<(Tag<'_>, Range<usize>, Vec<Node<'_>>)> = Vec::new();
    let mut flattened = 0usize;
    let events: Vec<_> = Parser::new_ext(md, options).into_offset_iter().collect();
    let urls = bare_urls(md, &events);
    for (event, span) in events {
        let node = match event {
            Event::Start(_) if open.len() >= MAX_DEPTH => {
                flattened += 1;
                continue;
            }
            Event::Start(tag) => {
                open.push((tag, span, Vec::new()));
                continue;
            }
            Event::End(_) if flattened > 0 => {
                flattened -= 1;
                continue;
            }
            Event::End(_) => {
                let Some((tag, span, children)) = open.pop() else {
                    continue;
                };
                if splits_bare_url(&tag, &span, &urls) {
                    let siblings = open
                        .last_mut()
                        .map_or(&mut root, |(_, _, children)| children);
                    unwrap_markup(md, span, children, siblings);
                    continue;
                }
                Node {
                    kind: Kind::Elem(tag, children),
                    span,
                }
            }
            Event::Text(text) => {
                let escaped = if is_escaped_delimiter(md, &span, &text) {
                    vec![0]
                } else {
                    Vec::new()
                };
                Node {
                    kind: Kind::Text(text.into_string(), escaped),
                    span,
                }
            }
            Event::Code(code) => Node {
                kind: Kind::Code(code),
                span,
            },
            Event::Html(html) | Event::InlineHtml(html) => Node {
                kind: Kind::Html(html),
                span,
            },
            Event::SoftBreak | Event::HardBreak => Node {
                kind: Kind::Break,
                span,
            },
            Event::Rule => Node {
                kind: Kind::Rule,
                span,
            },
            _ => Node {
                kind: Kind::Other,
                span,
            },
        };
        let siblings = open
            .last_mut()
            .map_or(&mut root, |(_, _, children)| children);
        push_merging_text(siblings, node);
    }
    root
}

/// The source ranges of the bare URLs as rendered, sorted and disjoint.
///
/// A URL is measured in the source over a run of text and emphasis events,
/// because pulldown-cmark ends a text event at `_`, `*` or `~` that it takes
/// for emphasis even in the middle of a URL. Any other event, such as inline
/// code, ends the run, and so does a character reference or escape that
/// renders as a character no URL holds, as `&lt;` does. Each run is scanned
/// once. A URL right after a character reference or escape that renders as
/// a letter or digit, as `&#97;` does, is joined to a word as rendered, so
/// it isn't one.
fn bare_urls(md: &str, events: &[(Event<'_>, Range<usize>)]) -> Vec<Range<usize>> {
    let mut urls = Vec::new();
    let mut joined = Vec::new();
    let mut run: Option<Range<usize>> = None;
    for (event, span) in events {
        let piece = match event {
            Event::Text(text) if md[span.clone()] == **text || !text.contains(ends_url) => {
                if md[span.clone()] != **text
                    && text
                        .chars()
                        .next_back()
                        .is_some_and(|c| c.is_ascii_alphanumeric())
                {
                    joined.push(span.end);
                }
                Some(span.clone())
            }
            Event::Start(Tag::Strong | Tag::Emphasis | Tag::Strikethrough) => {
                Some(span.start..span.start)
            }
            Event::End(TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough) => {
                Some(span.end..span.end)
            }
            _ => None,
        };
        match (piece, &mut run) {
            (Some(piece), Some(run)) => run.end = run.end.max(piece.end),
            (Some(piece), None) => run = Some(piece),
            (None, _) => {
                if let Some(run) = run.take() {
                    scan_run(md, run, &joined, &mut urls);
                }
            }
        }
    }
    if let Some(run) = run {
        scan_run(md, run, &joined, &mut urls);
    }
    urls
}

/// Records the bare URLs in one run of source text. `joined` holds the
/// sorted source offsets where the rendered text before ends in a letter or
/// digit although the source doesn't.
fn scan_run(md: &str, run: Range<usize>, joined: &[usize], urls: &mut Vec<Range<usize>>) {
    let text = &md[..run.end];
    let mut at = run.start;
    while let Some(offset) = text[at..].find("http") {
        let start = at + offset;
        let found = bare_url(text, start).filter(|_| joined.binary_search(&start).is_err());
        at = match found {
            Some(url) => {
                urls.push(start..start + url.len());
                start + url.len()
            }
            None => start + "http".len(),
        };
    }
}

/// Whether `tag` is emphasis or strikethrough that opens inside a bare URL
/// as rendered. Such markup is part of the URL's path, as in
/// `https://docs.python.org/3/library/__main__.html`, so it is kept as the
/// source wrote it and the URL is linked whole. Markup that opens before a
/// URL wraps it, as in `**https://x.io/a**'s`, and its closing delimiter ends
/// the URL.
fn splits_bare_url(tag: &Tag<'_>, span: &Range<usize>, urls: &[Range<usize>]) -> bool {
    let after = urls.partition_point(|url| url.start <= span.start);
    matches!(tag, Tag::Strong | Tag::Emphasis | Tag::Strikethrough)
        && after > 0
        && span.start < urls[after - 1].end
}

/// Puts the children of an element in its place, with its delimiters as
/// text, as the source wrote them.
fn unwrap_markup<'a>(
    md: &str,
    span: Range<usize>,
    children: Vec<Node<'a>>,
    siblings: &mut Vec<Node<'a>>,
) {
    let inner_start = children.first().map_or(span.end, |child| child.span.start);
    let inner_end = children.last().map_or(span.end, |child| child.span.end);
    let delimiter = |range: Range<usize>| Node {
        kind: Kind::Text(md[range.clone()].to_string(), Vec::new()),
        span: range,
    };
    push_merging_text(siblings, delimiter(span.start..inner_start));
    for child in children {
        push_merging_text(siblings, child);
    }
    if inner_end < span.end {
        push_merging_text(siblings, delimiter(inner_end..span.end));
    }
}

/// Whether a text event starts with a formatting character that the source
/// wrote as a backslash escape (`\*`) or a character reference (`&ast;`).
/// pulldown-cmark starts a new text event at each of them.
fn is_escaped_delimiter(md: &str, span: &Range<usize>, text: &str) -> bool {
    text.starts_with(DELIMITERS) && (md[..span.start].ends_with('\\') || md[span.clone()] != *text)
}

/// pulldown-cmark splits one run of text into several events at characters
/// that could have been markup. Merging them lets mentions and URLs be
/// scanned whole.
fn push_merging_text<'a>(siblings: &mut Vec<Node<'a>>, node: Node<'a>) {
    if let (
        Some(Node {
            kind: Kind::Text(prev, prev_escaped),
            span: prev_span,
        }),
        Kind::Text(text, escaped),
    ) = (siblings.last_mut(), &node.kind)
    {
        let base = prev.len();
        prev_escaped.extend(escaped.iter().map(|offset| base + offset));
        prev.push_str(text);
        prev_span.end = node.span.end;
        return;
    }
    siblings.push(node);
}

fn is_block(node: &Node<'_>) -> bool {
    match &node.kind {
        Kind::Rule => true,
        Kind::Elem(tag, _) => matches!(
            tag,
            Tag::Paragraph
                | Tag::Heading { .. }
                | Tag::BlockQuote(_)
                | Tag::CodeBlock(_)
                | Tag::HtmlBlock
                | Tag::List(_)
                | Tag::Item
                | Tag::FootnoteDefinition(_)
                | Tag::DefinitionList
                | Tag::DefinitionListTitle
                | Tag::DefinitionListDefinition
                | Tag::Table(_)
                | Tag::TableHead
                | Tag::TableRow
                | Tag::TableCell
                | Tag::MetadataBlock(_)
        ),
        _ => false,
    }
}

/// One output line. Code lines are never indented under list items, so
/// indentation can't change what a code block shows.
struct Line {
    text: String,
    code: bool,
}

impl Line {
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            code: false,
        }
    }

    fn code(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            code: true,
        }
    }
}

/// How inline content is being rendered.
#[derive(Clone, Copy, Default)]
struct Ctx {
    /// Raw text without markup or escaping, for table cells and alt text.
    plain: bool,
    /// Inside a link label: no mention or URL arming, breaks become spaces.
    label: bool,
    /// Inside a heading, which is already bold.
    heading: bool,
    strong: bool,
    em: bool,
    strike: bool,
}

impl Ctx {
    fn plain() -> Self {
        Self {
            plain: true,
            ..Self::default()
        }
    }
}

struct Renderer<'a> {
    src: &'a str,
    line_starts: Vec<usize>,
    directory: &'a dyn MentionDirectory,
}

impl Renderer<'_> {
    fn blocks(&self, nodes: &[Node<'_>]) -> Vec<Line> {
        let mut parts = Vec::new();
        let mut i = 0;
        while i < nodes.len() {
            if is_block(&nodes[i]) {
                parts.push((nodes[i].span.clone(), self.block(&nodes[i])));
                i += 1;
                continue;
            }
            let end = nodes[i..]
                .iter()
                .position(is_block)
                .map_or(nodes.len(), |p| i + p);
            let run = &nodes[i..end];
            let span = run[0].span.start..run[run.len() - 1].span.end;
            parts.push((span, self.paragraph(run)));
            i = end;
        }
        self.join(parts)
    }

    /// Joins rendered blocks, keeping a blank line where the source had one.
    fn join(&self, parts: Vec<(Range<usize>, Vec<Line>)>) -> Vec<Line> {
        let mut out = Vec::new();
        let mut prev_last: Option<usize> = None;
        for (span, lines) in parts {
            if lines.is_empty() {
                continue;
            }
            let (first, last) = self.line_range(&span);
            if prev_last.is_some_and(|prev| first > prev + 1) {
                out.push(Line::text(""));
            }
            out.extend(lines);
            prev_last = Some(last);
        }
        out
    }

    fn line_range(&self, span: &Range<usize>) -> (usize, usize) {
        let line_of = |offset: usize| self.line_starts.partition_point(|&s| s <= offset) - 1;
        let first = line_of(span.start);
        let end = span.start + self.src[span.clone()].trim_end().len();
        let last = if end > span.start {
            line_of(end - 1)
        } else {
            first
        };
        (first, last)
    }

    fn block(&self, node: &Node<'_>) -> Vec<Line> {
        let Kind::Elem(tag, children) = &node.kind else {
            return match node.kind {
                Kind::Rule => vec![Line::text(RULE)],
                _ => self.paragraph(std::slice::from_ref(node)),
            };
        };
        match tag {
            Tag::Paragraph | Tag::HtmlBlock => self.paragraph(children),
            Tag::Heading { .. } => self.heading(children),
            Tag::BlockQuote(_) => quote(self.blocks(children)),
            Tag::CodeBlock(kind) => code_block(kind, children),
            Tag::List(start) => self.list(*start, children),
            Tag::Table(alignments) => self.table(alignments, children),
            _ => self.blocks(children),
        }
    }

    fn paragraph(&self, nodes: &[Node<'_>]) -> Vec<Line> {
        let text = self.inline(nodes, Ctx::default());
        let text = text.trim_end_matches('\n');
        if text.is_empty() {
            return Vec::new();
        }
        text.split('\n').map(Line::text).collect()
    }

    fn heading(&self, nodes: &[Node<'_>]) -> Vec<Line> {
        let ctx = Ctx {
            heading: true,
            ..Ctx::default()
        };
        let text = self.inline(nodes, ctx).replace('\n', " ");
        let text = text.trim();
        if text.is_empty() {
            return Vec::new();
        }
        vec![Line::text(format!("*{text}*"))]
    }

    fn list(&self, start: Option<u64>, items: &[Node<'_>]) -> Vec<Line> {
        let mut parts = Vec::new();
        for (index, item) in (0u64..).zip(items) {
            let marker = match start {
                Some(first) => format!("{}.", first.saturating_add(index)),
                None => "•".to_string(),
            };
            let children = match &item.kind {
                Kind::Elem(_, children) => children.as_slice(),
                _ => std::slice::from_ref(item),
            };
            parts.push((item.span.clone(), list_item(&marker, self.blocks(children))));
        }
        self.join(parts)
    }

    fn table(&self, alignments: &[Alignment], rows: &[Node<'_>]) -> Vec<Line> {
        let cells: Vec<Vec<String>> = rows
            .iter()
            .map(|row| match &row.kind {
                Kind::Elem(_, cells) => cells
                    .iter()
                    .map(|cell| match &cell.kind {
                        Kind::Elem(_, content) => {
                            self.inline(content, Ctx::plain()).trim().to_string()
                        }
                        _ => String::new(),
                    })
                    .collect(),
                _ => Vec::new(),
            })
            .collect();
        let columns = cells
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(0)
            .max(alignments.len());
        let widths: Vec<usize> = (0..columns)
            .map(|c| {
                cells
                    .iter()
                    .filter_map(|row| row.get(c))
                    .map(|cell| cell.chars().count())
                    .max()
                    .unwrap_or(0)
                    .max(1)
            })
            .collect();
        let format_row = |row: &Vec<String>| {
            widths
                .iter()
                .enumerate()
                .map(|(c, &width)| {
                    let cell = row.get(c).map_or("", String::as_str);
                    let alignment = alignments.get(c).copied().unwrap_or(Alignment::None);
                    pad(cell, width, alignment)
                })
                .collect::<Vec<_>>()
                .join(" | ")
                .trim_end()
                .to_string()
        };
        let separator = widths
            .iter()
            .map(|&width| "-".repeat(width))
            .collect::<Vec<_>>()
            .join("-+-");
        let mut body: Vec<String> = cells.iter().map(format_row).collect();
        body.insert(body.len().min(1), separator);
        std::iter::once(FENCE.to_string())
            .chain(body.iter().map(|row| escape(&break_fences(row, 0))))
            .chain(std::iter::once(FENCE.to_string()))
            .map(Line::code)
            .collect()
    }

    fn inline(&self, nodes: &[Node<'_>], ctx: Ctx) -> String {
        let mut out = String::new();
        for node in nodes {
            self.inline_node(node, ctx, &mut out);
        }
        out
    }

    fn inline_node(&self, node: &Node<'_>, ctx: Ctx, out: &mut String) {
        match &node.kind {
            Kind::Text(text, escaped) => self.text(text, escaped, ctx, out),
            Kind::Html(html) => self.text(html, &[], ctx, out),
            Kind::Code(code) if ctx.plain => out.push_str(code),
            Kind::Code(code) => {
                out.push('`');
                out.push_str(&escape(code));
                out.push('`');
            }
            Kind::Break if ctx.plain || ctx.label || ctx.heading => out.push(' '),
            Kind::Break => out.push('\n'),
            Kind::Rule | Kind::Other => {}
            Kind::Elem(Tag::Strong | Tag::Emphasis | Tag::Strikethrough, children)
                if self.inside_word(&node.span) =>
            {
                let delimiter = self.src[node.span.start..].chars().next().unwrap_or('*');
                out.push(delimiter);
                for child in children {
                    self.inline_node(child, ctx, out);
                }
                out.push(delimiter);
            }
            Kind::Elem(tag, children) => match tag {
                Tag::Strong => {
                    let inner = Ctx {
                        strong: true,
                        ..ctx
                    };
                    self.styled('*', ctx.strong || ctx.heading, children, inner, out);
                }
                Tag::Emphasis => {
                    let inner = Ctx { em: true, ..ctx };
                    self.styled('_', ctx.em, children, inner, out);
                }
                Tag::Strikethrough => {
                    let inner = Ctx {
                        strike: true,
                        ..ctx
                    };
                    self.styled('~', ctx.strike, children, inner, out);
                }
                Tag::Link {
                    link_type,
                    dest_url,
                    ..
                } => self.link(*link_type, dest_url, children, ctx, out),
                Tag::Image { dest_url, .. } => self.image(dest_url, children, ctx, out),
                _ => {
                    for child in children {
                        self.inline_node(child, ctx, out);
                    }
                }
            },
        }
    }

    fn styled(
        &self,
        marker: char,
        already: bool,
        children: &[Node<'_>],
        inner: Ctx,
        out: &mut String,
    ) {
        let text = self.inline(children, inner);
        if inner.plain || already || text.is_empty() {
            out.push_str(&text);
            return;
        }
        out.push(marker);
        out.push_str(&text);
        out.push(marker);
    }

    /// Whether the source has a letter or digit right before or after `span`.
    /// Slack doesn't format inside a word, so emphasis there keeps its
    /// Markdown delimiter character, once on each side: `5*3*2` must not
    /// become `5_3_2`.
    fn inside_word(&self, span: &Range<usize>) -> bool {
        self.src[..span.start]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric)
            || self.src[span.end..]
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric)
    }

    fn link(
        &self,
        link_type: LinkType,
        dest: &str,
        children: &[Node<'_>],
        ctx: Ctx,
        out: &mut String,
    ) {
        let shown = self.inline(children, Ctx::plain());
        if ctx.plain {
            plain_link(&shown, dest, out);
            return;
        }
        let url = match link_type {
            LinkType::Email => format!("mailto:{dest}"),
            _ => dest.to_string(),
        };
        let label = match link_type {
            LinkType::Autolink => String::new(),
            _ => self
                .inline(children, Ctx { label: true, ..ctx })
                .replace('\n', " "),
        };
        push_link(&url, &label, &shown, out);
    }

    /// Alt text is plain text, so every formatting character in it is shown
    /// as written.
    fn image(&self, dest: &str, children: &[Node<'_>], ctx: Ctx, out: &mut String) {
        let alt = self.inline(children, Ctx::plain());
        if ctx.plain {
            plain_link(&alt, dest, out);
            return;
        }
        let literal: Vec<usize> = alt.match_indices(DELIMITERS).map(|(i, _)| i).collect();
        let mut label = String::new();
        self.slack_text(&alt, &literal, false, &mut label);
        if ctx.label {
            out.push_str(&label);
        } else {
            push_link(dest, &label, &alt, out);
        }
    }

    fn text(&self, text: &str, escaped: &[usize], ctx: Ctx, out: &mut String) {
        if ctx.plain {
            out.push_str(text);
        } else {
            self.slack_text(text, escaped, !ctx.label, out);
        }
    }

    /// Escapes text outside code and neutralizes broadcasts. The formatting
    /// characters at the byte offsets in `literal` are kept from pairing up
    /// into Slack formatting. With `arm`, it also resolves `@Name` mentions
    /// and gives bare URLs explicit bounds.
    fn slack_text(&self, text: &str, literal: &[usize], arm: bool, out: &mut String) {
        let mut i = 0;
        while let Some(c) = text[i..].chars().next() {
            if c == '<'
                && let Some((end, word)) = wire_broadcast(text, i)
            {
                out.push('@');
                out.push(ZERO_WIDTH_SPACE);
                out.push_str(&word.to_ascii_lowercase());
                i = end;
                continue;
            }
            if c == '@' {
                if let Some(end) = mention::broadcast(text, i, BROADCASTS) {
                    out.push('@');
                    out.push(ZERO_WIDTH_SPACE);
                    out.push_str(&text[i + 1..end]);
                    i = end;
                    continue;
                }
                if arm && let Some(found) = mention::scan(text, i, BROADCASTS, self.directory) {
                    match found.id {
                        Some(id) => {
                            out.push_str("<@");
                            out.push_str(&escape(&id));
                            out.push('>');
                        }
                        None => out.push_str(&escape(&text[i..found.end])),
                    }
                    i = found.end;
                    continue;
                }
            }
            if arm && let Some(url) = bare_url(text, i) {
                push_slack_link(url, "", out);
                i += url.len();
                continue;
            }
            if literal.binary_search(&i).is_ok() {
                push_literal(text, i, c, out);
                i += c.len_utf8();
                continue;
            }
            push_escaped(c, out);
            i += c.len_utf8();
        }
    }
}

fn list_item(marker: &str, lines: Vec<Line>) -> Vec<Line> {
    let mut lines = lines.into_iter();
    let Some(first) = lines.next() else {
        return vec![Line::text(marker)];
    };
    let first = Line {
        text: format!("{marker} {}", first.text),
        code: first.code,
    };
    std::iter::once(first)
        .chain(lines.map(|line| {
            if line.code || line.text.is_empty() {
                line
            } else {
                Line::text(format!("  {}", line.text))
            }
        }))
        .collect()
}

fn quote(lines: Vec<Line>) -> Vec<Line> {
    lines
        .into_iter()
        .map(|line| Line {
            text: if line.text.is_empty() {
                ">".to_string()
            } else {
                format!("> {}", line.text)
            },
            code: line.code,
        })
        .collect()
}

fn code_block(kind: &CodeBlockKind<'_>, children: &[Node<'_>]) -> Vec<Line> {
    let content: String = children
        .iter()
        .filter_map(|child| match &child.kind {
            Kind::Text(text, _) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let info = match kind {
        CodeBlockKind::Fenced(info) => info.as_ref(),
        CodeBlockKind::Indented => "",
    };
    let body = content.strip_suffix('\n').unwrap_or(&content);
    let body_lines = if content.is_empty() {
        Vec::new()
    } else {
        body.split('\n')
            .map(|line| escape(&break_fences(line, 0)))
            .collect()
    };
    let open = format!("{FENCE}{}", escape(&break_fences(info, FENCE.len())));
    std::iter::once(open)
        .chain(body_lines)
        .chain(std::iter::once(FENCE.to_string()))
        .map(Line::code)
        .collect()
}

/// Slack closes a code block at any run of three backticks, and has no
/// escape, so this puts a zero-width space before every third backtick in a
/// row. `run` counts the backticks just before `text`.
fn break_fences(text: &str, mut run: usize) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '`' {
            if run >= 2 {
                out.push(ZERO_WIDTH_SPACE);
                run = 0;
            }
            run += 1;
        } else {
            run = 0;
        }
        out.push(c);
    }
    out
}

fn pad(cell: &str, width: usize, alignment: Alignment) -> String {
    let gap = width.saturating_sub(cell.chars().count());
    let (left, right) = match alignment {
        Alignment::Right => (gap, 0),
        Alignment::Center => (gap / 2, gap - gap / 2),
        Alignment::Left | Alignment::None => (0, gap),
    };
    format!("{}{cell}{}", " ".repeat(left), " ".repeat(right))
}

fn plain_link(label: &str, dest: &str, out: &mut String) {
    out.push_str(label);
    if label.is_empty() {
        out.push_str(dest);
    } else if !dest.is_empty() && label != dest {
        out.push_str(" (");
        out.push_str(dest);
        out.push(')');
    }
}

/// Writes a link to `url` showing `label`, which must already be escaped.
/// `shown` is the label as plain text. A label that names another host than
/// `url` goes next to the link instead of in it, so it can't disguise where
/// the link leads.
fn push_link(url: &str, label: &str, shown: &str, out: &mut String) {
    if url.is_empty() {
        out.push_str(label);
    } else if label.is_empty() || !misleads(shown, url) {
        push_slack_link(url, label, out);
    } else {
        out.push_str(label);
        out.push_str(" (");
        push_slack_link(url, "", out);
        out.push(')');
    }
}

/// Whether any word of `shown` looks like a URL, an email address or a
/// domain name whose host differs from `url`'s.
fn misleads(shown: &str, url: &str) -> bool {
    let dest = host(strip_scheme(url).1);
    shown
        .split_whitespace()
        .filter_map(named_host)
        .any(|named| named != dest)
}

/// The host a word of a label names, if it looks like a URL, an email
/// address or a domain name.
fn named_host(word: &str) -> Option<String> {
    let word = word.trim_matches(|c: char| !c.is_alphanumeric());
    let (scheme, rest) = strip_scheme(word);
    let named = host(rest);
    (scheme || looks_like_domain(&named)).then_some(named)
}

/// Splits off a `scheme://` or `mailto:` prefix, reporting whether there was
/// one.
fn strip_scheme(url: &str) -> (bool, &str) {
    let Some((scheme, rest)) = url.split_once(':') else {
        return (false, url);
    };
    let valid = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-'));
    if valid && (rest.starts_with("//") || scheme.eq_ignore_ascii_case("mailto")) {
        (true, rest)
    } else {
        (false, url)
    }
}

/// The host of a URL with its scheme removed, normalized for comparison:
/// no user info, port, trailing dot or invisible characters, lowercase,
/// dot look-alikes as `.`, and no leading `www.`. Browsers end the authority
/// at `\` as well as `/`.
fn host(rest: &str) -> String {
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let authority = rest.split(['/', '?', '#', '\\']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    let host = match host.strip_prefix('[') {
        Some(ipv6) => ipv6.split(']').next(),
        None => host.split(':').next(),
    }
    .unwrap_or_default();
    let host: String = host
        .chars()
        .filter(|&c| !is_default_ignorable(c))
        .map(|c| match c {
            '\u{2024}' | '\u{3002}' | '\u{FE52}' | '\u{FF0E}' | '\u{FF61}' => '.',
            _ => c,
        })
        .collect();
    let host = host.to_lowercase();
    let host = host.trim_end_matches('.');
    host.strip_prefix("www.").unwrap_or(host).to_string()
}

/// Unicode's default-ignorable code points: characters that render as
/// nothing, such as zero-width spaces and bidirectional controls, which IDNA
/// drops from hostnames and which could hide a dot from the domain check.
fn is_default_ignorable(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{115F}'..='\u{1160}'
            | '\u{17B4}'..='\u{17B5}'
            | '\u{180B}'..='\u{180F}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF0}'..='\u{FFF8}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0000}'..='\u{E0FFF}'
    )
}

/// Two or more dot-separated labels of letters, digits and `-`, ending in an
/// alphabetic or punycode top-level label, or an IPv4 address.
fn looks_like_domain(host: &str) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    let tld = labels.last().copied().unwrap_or_default();
    let named = tld.chars().count() >= 2 && tld.chars().all(char::is_alphabetic);
    let ipv4 = labels.len() == 4
        && labels
            .iter()
            .all(|label| label.chars().all(|c| c.is_ascii_digit()));
    labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty() && label.chars().all(|c| c.is_alphanumeric() || c == '-')
        })
        && (named || tld.starts_with("xn--") || ipv4)
}

/// Writes `<url>` or `<url|label>`. `label` must already be escaped.
fn push_slack_link(url: &str, label: &str, out: &mut String) {
    out.push('<');
    for (i, c) in url.char_indices() {
        match c {
            '|' | ' ' => out.push_str(&percent(c)),
            '@' | '#' | '!' if i == 0 => out.push_str(&percent(c)),
            _ => push_escaped(c, out),
        }
    }
    if !label.is_empty() {
        out.push('|');
        out.push_str(label);
    }
    out.push('>');
}

fn percent(c: char) -> String {
    format!("%{:02X}", u32::from(c))
}

/// Matches `<!here>`, `<!channel>` and `<!everyone>`, with or without a
/// `|label`, ignoring case, like qm-core's `MASS_MENTION`. Returns the end of
/// the token and the broadcast word.
fn wire_broadcast(text: &str, at: usize) -> Option<(usize, &str)> {
    let rest = text[at..].strip_prefix("<!")?;
    let len = rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    let word = &rest[..len];
    if !BROADCASTS.iter().any(|b| word.eq_ignore_ascii_case(b)) {
        return None;
    }
    let after = &rest[len..];
    let close = if after.starts_with('>') {
        0
    } else if after.starts_with('|') {
        let window = &after[..after.floor_char_boundary(MAX_WIRE_LABEL)];
        let close = window.find('>')?;
        if window[..close].contains('\n') {
            return None;
        }
        close
    } else {
        return None;
    };
    Some((at + 2 + len + close + 1, word))
}

/// Writes a formatting character the source escaped. Slack has no escape,
/// and it only formats at word boundaries, so a character with a letter or
/// digit on both sides stays as it is. Otherwise zero-width spaces on both
/// sides keep it from opening or closing formatting.
fn push_literal(text: &str, at: usize, c: char, out: &mut String) {
    let word_char = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
    let inside_word = word_char(text[..at].chars().next_back())
        && word_char(text[at + c.len_utf8()..].chars().next());
    if inside_word {
        out.push(c);
    } else {
        out.push(ZERO_WIDTH_SPACE);
        out.push(c);
        out.push(ZERO_WIDTH_SPACE);
    }
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        push_escaped(c, &mut out);
    }
    out
}

fn push_escaped(c: char, out: &mut String) {
    match c {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        _ => out.push(c),
    }
}

#[cfg(test)]
mod tests;
