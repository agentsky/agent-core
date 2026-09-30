//! Markdown to Slack mrkdwn.
//!
//! The behavioral reference is qm-core's `toSlackMrkdwn` in
//! `src/slack/mrkdwn.ts`. Where CommonMark parsing and qm-core's regexes
//! disagree, this module follows the parse tree; `docs/impl-notes.md` lists
//! the differences.

use std::ops::Range;

use pulldown_cmark::{Alignment, CodeBlockKind, CowStr, Event, LinkType, Options, Parser, Tag};

use crate::{MentionDirectory, mention};

/// Names Slack treats as broadcasts to a whole channel or workspace.
const BROADCASTS: &[&str] = &["here", "channel", "everyone"];

/// What a thematic break (`---`) becomes. mrkdwn has no divider.
const RULE: &str = "──────────";

/// Inserted after the `@` of a broadcast so it can't notify anyone.
const ZERO_WIDTH_SPACE: char = '\u{200B}';

/// Elements nested deeper than this are flattened into their ancestor at
/// this depth. Rendering and dropping the tree recurse once per level, so an
/// unbounded depth (a line of 100,000 `>`) would overflow the stack.
const MAX_DEPTH: usize = 64;

/// Converts agent-written Markdown to Slack mrkdwn.
///
/// - Headings become bold lines.
/// - `**bold**` and `__bold__` become `*bold*`, `*em*` and `_em_` become
///   `_em_`, and `~~strike~~` becomes `~strike~`.
/// - Inline and fenced code keep their contents; only `&`, `<` and `>` are
///   escaped. A fenced block keeps its info string.
/// - `[label](url)` becomes `<url|label>`, and images become links to their
///   source. Bare `http(s)` URLs get explicit `<url>` boundaries, so Slack
///   doesn't pull neighboring punctuation or formatting marks into them.
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
    Text(String),
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
    for (event, span) in Parser::new_ext(md, options).into_offset_iter() {
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
                Node {
                    kind: Kind::Elem(tag, children),
                    span,
                }
            }
            Event::Text(text) => Node {
                kind: Kind::Text(text.into_string()),
                span,
            },
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

/// pulldown-cmark splits one run of text into several events at characters
/// that could have been markup. Merging them lets mentions and URLs be
/// scanned whole.
fn push_merging_text<'a>(siblings: &mut Vec<Node<'a>>, node: Node<'a>) {
    if let (
        Some(Node {
            kind: Kind::Text(prev),
            span: prev_span,
        }),
        Kind::Text(text),
    ) = (siblings.last_mut(), &node.kind)
    {
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
        let fence = fence_for(&body.join("\n"));
        std::iter::once(fence.to_string())
            .chain(body.iter().map(|row| escape(row)))
            .chain(std::iter::once(fence.to_string()))
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
            Kind::Text(text) => self.text(text, ctx, out),
            Kind::Html(html) => self.text(html, ctx, out),
            Kind::Code(code) if ctx.plain => out.push_str(code),
            Kind::Code(code) => {
                out.push('`');
                out.push_str(&escape(code));
                out.push('`');
            }
            Kind::Break if ctx.plain || ctx.label || ctx.heading => out.push(' '),
            Kind::Break => out.push('\n'),
            Kind::Rule | Kind::Other => {}
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

    fn link(
        &self,
        link_type: LinkType,
        dest: &str,
        children: &[Node<'_>],
        ctx: Ctx,
        out: &mut String,
    ) {
        if ctx.plain {
            plain_link(&self.inline(children, ctx), dest, out);
            return;
        }
        let label_ctx = Ctx { label: true, ..ctx };
        let label = self.inline(children, label_ctx).replace('\n', " ");
        let url = match link_type {
            LinkType::Email => format!("mailto:{dest}"),
            _ => dest.to_string(),
        };
        let label = match link_type {
            LinkType::Autolink => "",
            _ => label.as_str(),
        };
        push_slack_link(&url, label, out);
    }

    fn image(&self, dest: &str, children: &[Node<'_>], ctx: Ctx, out: &mut String) {
        let alt = self.inline(children, Ctx::plain());
        if ctx.plain {
            plain_link(&alt, dest, out);
            return;
        }
        let mut label = String::new();
        self.slack_text(&alt, false, &mut label);
        if ctx.label {
            out.push_str(&label);
        } else {
            push_slack_link(dest, &label, out);
        }
    }

    fn text(&self, text: &str, ctx: Ctx, out: &mut String) {
        if ctx.plain {
            out.push_str(text);
        } else {
            self.slack_text(text, !ctx.label, out);
        }
    }

    /// Escapes text outside code and neutralizes broadcasts. With `arm`, it
    /// also resolves `@Name` mentions and gives bare URLs explicit bounds.
    fn slack_text(&self, text: &str, arm: bool, out: &mut String) {
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
            Kind::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let info = match kind {
        CodeBlockKind::Fenced(info) => info.as_ref(),
        CodeBlockKind::Indented => "",
    };
    let fence = fence_for(&content);
    let body = content.strip_suffix('\n').unwrap_or(&content);
    let body_lines = if content.is_empty() {
        Vec::new()
    } else {
        body.split('\n').map(escape).collect()
    };
    std::iter::once(format!("{fence}{}", escape(info)))
        .chain(body_lines)
        .chain(std::iter::once(fence.to_string()))
        .map(Line::code)
        .collect()
}

/// Slack only knows backtick fences. A body that itself holds a backtick
/// fence would close it early, so it keeps a tilde fence instead, as qm-core
/// does.
fn fence_for(body: &str) -> &'static str {
    if body.contains("```") { "~~~" } else { "```" }
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
    } else if label != dest {
        out.push_str(" (");
        out.push_str(dest);
        out.push(')');
    }
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
        after.find('>')?
    } else {
        return None;
    };
    Some((at + 2 + len + close + 1, word))
}

/// Finds a bare `http://` or `https://` URL at byte offset `at`, trimmed of
/// trailing punctuation the way qm-core's `trimUrlTail` does.
fn bare_url(text: &str, at: usize) -> Option<&str> {
    let rest = &text[at..];
    let scheme = ["https://", "http://"]
        .into_iter()
        .find(|scheme| rest.starts_with(scheme))?;
    if text[..at]
        .chars()
        .next_back()
        .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    let len = rest
        .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '|'))
        .unwrap_or(rest.len());
    let url = trim_url_tail(&rest[..len]);
    (url.len() > scheme.len()).then_some(url)
}

/// Drops trailing punctuation, and closing brackets that have no opening
/// partner inside the URL, so `(see https://x.io/a).` keeps `)` and `.` out.
fn trim_url_tail(url: &str) -> &str {
    const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];
    let mut unmatched = PAIRS.map(|(open, close)| {
        url.matches(close).count() as isize - url.matches(open).count() as isize
    });
    let mut end = url.len();
    while let Some(c) = url[..end].chars().next_back() {
        let drop = match PAIRS.iter().position(|&(_, close)| close == c) {
            Some(pair) if unmatched[pair] > 0 => {
                unmatched[pair] -= 1;
                true
            }
            Some(_) => false,
            None => matches!(
                c,
                '*' | '_' | '~' | '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"'
            ),
        };
        if !drop {
            break;
        }
        end -= c.len_utf8();
    }
    &url[..end]
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
