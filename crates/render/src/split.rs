//! Splitting rendered text into messages that fit a surface's limit.
//!
//! The behavioral reference is qm-core's `safeChunks` in
//! `src/slack/safe-cut.ts`.

use std::ops::Range;

use core_types::{LengthUnit, Limit};

/// Splits rendered text into chunks that each fit `limit`.
///
/// Split the surface's rendered output, not the agent's Markdown: rendering
/// changes the length (`&` becomes `&amp;` on Slack, a link becomes
/// `<url|label>`), and it has to see whole constructs, such as a table or a
/// list, to convert them. The splitter therefore knows the output syntax of
/// both renderers.
///
/// Where to cut, in order of preference:
///
/// 1. After a blank line outside code or before or after a code block, then
///    after a line break, then after a run of spaces outside code, if one
///    falls in the second half of the chunk.
/// 2. After the latest break of any of those kinds.
/// 3. The latest position that doesn't break a word, as a last resort.
///
/// A cut never falls inside a Slack token (`<url|label>`, `<@U123>`), an
/// HTML entity (`&amp;`), a Markdown link or image, an `@mention`, a code
/// fence line, or a character, and it doesn't separate a character from the
/// combining marks, joiners or modifiers that follow it. It avoids cutting
/// inside a code span or between a pair of `*`, `_` or `~` markers on one
/// line when it can. These rules give way only when a single construct is
/// longer than a chunk.
///
/// When a cut falls inside a fenced code block, the chunk ends with a
/// closing fence and the next chunk starts by repeating the opening fence
/// line, info string and any `>` or indent prefix included. The chunks are
/// otherwise consecutive pieces of `text`, so removing the added fence
/// lines and joining them gives `text` back. Whitespace at a cut stays at
/// the end of the earlier chunk.
///
/// Lengths are counted in `limit.unit`. A chunk holds at least one
/// character, so a limit smaller than one character (`max` of 0, or 1 in
/// UTF-16 before a character outside the Basic Multilingual Plane) can't be
/// met, and such a chunk exceeds it. Fences are only repeated for blocks
/// whose opening line fits the limit with room to spare. Empty text gives
/// no chunks.
///
/// # Examples
///
/// ```
/// use core_types::{LengthUnit, Limit};
///
/// let limit = Limit { max: 24, unit: LengthUnit::Chars };
/// let text = "Intro.\n\n```sh\nmake build\nmake test\n```";
/// assert_eq!(
///     render::split(text, limit),
///     ["Intro.\n\n", "```sh\nmake build\n```", "```sh\nmake test\n```"],
/// );
/// ```
pub fn split(text: &str, limit: Limit) -> Vec<String> {
    pieces(&Doc::new(text, limit))
        .into_iter()
        .map(|piece| piece.render(text))
        .collect()
}

/// One chunk: a range of the text, plus the fence lines added around it.
#[derive(Debug)]
pub(crate) struct Piece<'a> {
    /// The opening fence line repeated at the start, without its newline.
    pub(crate) reopen: Option<&'a str>,
    /// The byte range of the text the chunk holds.
    pub(crate) body: Range<usize>,
    /// The closing fence added at the end, with the newline before it when
    /// the body doesn't end with one.
    pub(crate) close: Option<String>,
}

impl Piece<'_> {
    fn render(&self, text: &str) -> String {
        let mut out = String::new();
        if let Some(reopen) = self.reopen {
            out.push_str(reopen);
            out.push('\n');
        }
        out.push_str(&text[self.body.clone()]);
        if let Some(close) = &self.close {
            out.push_str(close);
        }
        out
    }
}

/// The kinds of place a cut can fall, weakest first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Break {
    None,
    Space,
    Line,
    Paragraph,
}

/// How strictly to keep constructs whole.
#[derive(Clone, Copy)]
enum Rules {
    /// Neither hard nor soft constructs may be cut.
    All,
    /// Only hard constructs (tokens, links, mentions, fences, characters).
    Hard,
}

/// A fenced code block.
struct Fence {
    /// The opening line, without its newline.
    open: Range<usize>,
    /// The closing fence to add: the opening line's prefix and fence run.
    close: Range<usize>,
    /// The char index where the body starts, after the opening line.
    body_start: usize,
    /// The char index where the closing line starts, or the text's end.
    close_start: usize,
    /// The char index after the closing line and its newline.
    close_end: usize,
    /// Whether the fence is repeated when a cut falls inside it: when the
    /// opening line fits the limit twice, with room to spare.
    repeat: bool,
}

/// The text, indexed by char. Position `k` is the cut before char `k`.
pub(crate) struct Doc<'a> {
    text: &'a str,
    chars: Vec<char>,
    /// Byte offset of each position.
    offsets: Vec<usize>,
    /// Length in the limit's unit of the text before each position.
    lengths: Vec<usize>,
    unit: LengthUnit,
    max: usize,
    breaks: Vec<Break>,
    hard: Vec<bool>,
    soft: Vec<bool>,
    fences: Vec<Fence>,
    /// The repeated fence, if any, whose body a cut at each position falls
    /// in.
    open_at: Vec<Option<usize>>,
}

impl<'a> Doc<'a> {
    pub(crate) fn new(text: &'a str, limit: Limit) -> Self {
        let unit = limit.unit;
        let chars: Vec<char> = text.chars().collect();
        let mut offsets = Vec::with_capacity(chars.len() + 1);
        let mut lengths = Vec::with_capacity(chars.len() + 1);
        let (mut offset, mut length) = (0, 0);
        for &c in &chars {
            offsets.push(offset);
            lengths.push(length);
            offset += c.len_utf8();
            length += units(c, unit);
        }
        offsets.push(offset);
        lengths.push(length);
        let mut doc = Doc {
            text,
            chars,
            offsets,
            lengths,
            unit,
            max: limit.max,
            breaks: Vec::new(),
            hard: Vec::new(),
            soft: Vec::new(),
            fences: Vec::new(),
            open_at: Vec::new(),
        };
        let lines = doc.lines();
        doc.fences = doc.find_fences(&lines);
        let mut in_code = Marks::new(doc.chars.len());
        for fence in &doc.fences {
            let open_start = doc.char_index(fence.open.start);
            in_code.block(open_start, fence.close_start.max(fence.body_start));
        }
        let in_code = in_code.finish();
        doc.breaks = doc.find_breaks(&in_code);
        for fence in &doc.fences {
            let open_start = doc.char_index(fence.open.start);
            for k in [open_start, fence.close_end] {
                if k > 0 && k < doc.len() && doc.chars[k - 1] == '\n' {
                    doc.breaks[k] = Break::Paragraph;
                }
            }
        }
        doc.hard = doc.hard_marks();
        doc.soft = doc.soft_marks(&lines, &in_code);
        doc.open_at = vec![None; doc.len() + 1];
        for (i, fence) in doc.fences.iter().enumerate().filter(|(_, f)| f.repeat) {
            doc.open_at[fence.body_start..=fence.close_start].fill(Some(i));
        }
        doc
    }

    fn len(&self) -> usize {
        self.chars.len()
    }

    fn char_index(&self, byte: usize) -> usize {
        self.offsets.partition_point(|&o| o < byte)
    }

    fn units(&self, s: &str) -> usize {
        s.chars().map(|c| units(c, self.unit)).sum()
    }

    /// The char range of every line, without its newline.
    fn lines(&self) -> Vec<Range<usize>> {
        let mut lines = Vec::new();
        let mut start = 0;
        for (k, &c) in self.chars.iter().enumerate() {
            if c == '\n' {
                lines.push(start..k);
                start = k + 1;
            }
        }
        lines.push(start..self.len());
        lines
    }

    /// Finds fenced code blocks line by line. A fence may follow spaces and
    /// `>` quote markers, and its closing line must have as many `>` as
    /// its opening line.
    fn find_fences(&self, lines: &[Range<usize>]) -> Vec<Fence> {
        struct Open {
            line: Range<usize>,
            close: Range<usize>,
            marker: char,
            run: usize,
            quotes: usize,
        }
        let mut fences = Vec::new();
        let mut open: Option<Open> = None;
        for line in lines {
            let chars = &self.chars[line.clone()];
            let indent = chars
                .iter()
                .take_while(|c| matches!(c, ' ' | '\t' | '>'))
                .count();
            let quotes = chars[..indent].iter().filter(|&&c| c == '>').count();
            let marker = chars.get(indent).copied();
            let run = chars[indent..]
                .iter()
                .take_while(|&&c| Some(c) == marker)
                .count();
            let rest = &chars[indent + run..];
            match open.take() {
                Some(block) => {
                    let closes = marker == Some(block.marker)
                        && run >= block.run
                        && quotes == block.quotes
                        && rest.iter().all(|c| c.is_whitespace());
                    if closes {
                        fences.push(self.fence(block.line, block.close, line.start));
                    } else {
                        open = Some(block);
                    }
                }
                None => {
                    let opens = run >= 3
                        && match marker {
                            Some('`') => !rest.contains(&'`'),
                            Some('~') => true,
                            _ => false,
                        };
                    if let (true, Some(marker)) = (opens, marker) {
                        open = Some(Open {
                            line: line.clone(),
                            close: line.start..line.start + indent + run,
                            marker,
                            run,
                            quotes,
                        });
                    }
                }
            }
        }
        if let Some(block) = open {
            fences.push(self.fence(block.line, block.close, self.len()));
        }
        fences
    }

    fn fence(&self, open: Range<usize>, close: Range<usize>, close_start: usize) -> Fence {
        let open = self.offsets[open.start]..self.offsets[open.end];
        let close = self.offsets[close.start]..self.offsets[close.end];
        let body_start = (self.char_index(open.end) + 1).min(self.len());
        let repeat = 2 * self.units(&self.text[open.clone()]) + 4 <= self.max;
        let close_end = (close_start..self.len())
            .find(|&k| self.chars[k] == '\n')
            .map_or(self.len(), |k| k + 1);
        Fence {
            open,
            close,
            body_start,
            close_start,
            close_end,
            repeat,
        }
    }

    fn find_breaks(&self, in_code: &[bool]) -> Vec<Break> {
        let is_space = |c: char| c == ' ' || c == '\t';
        (0..=self.len())
            .map(|k| {
                let before = k.checked_sub(1).map(|i| self.chars[i]);
                let after = self.chars.get(k).copied();
                match before {
                    Some('\n') if k >= 2 && self.chars[k - 2] == '\n' && !in_code[k] => {
                        Break::Paragraph
                    }
                    Some('\n') => Break::Line,
                    Some(c)
                        if is_space(c)
                            && !in_code[k]
                            && after.is_some_and(|a| !is_space(a) && a != '\n') =>
                    {
                        Break::Space
                    }
                    _ => Break::None,
                }
            })
            .collect()
    }

    /// Positions inside constructs a cut must never split.
    fn hard_marks(&self) -> Vec<bool> {
        let chars = &self.chars;
        let n = self.len();
        let mut marks = Marks::new(n);
        let mut k = 0;
        while k < n {
            match chars[k] {
                '<' => {
                    let end = (k + 1..n)
                        .find(|&j| matches!(chars[j], '>' | '<' | '\n'))
                        .unwrap_or(n);
                    if end < n && chars[end] == '>' {
                        marks.block(k, end + 1);
                        k = end + 1;
                    } else {
                        k = end;
                    }
                    continue;
                }
                '&' => {
                    let name = chars[k + 1..]
                        .iter()
                        .take(11)
                        .take_while(|c| c.is_ascii_alphanumeric() || **c == '#')
                        .count();
                    if (1..=10).contains(&name) && chars.get(k + 1 + name) == Some(&';') {
                        marks.block(k, k + name + 2);
                    }
                }
                '@' if k == 0 || !chars[k - 1].is_alphanumeric() => {
                    let name = chars[k + 1..]
                        .iter()
                        .take_while(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-'))
                        .count();
                    marks.block(k, k + 1 + name);
                }
                _ => {}
            }
            k += 1;
        }
        self.mark_links(&mut marks);
        for fence in &self.fences {
            let open_start = self.char_index(fence.open.start);
            marks.block(open_start, fence.body_start + 1);
            if fence.close_start < n {
                marks.block(fence.close_start - 1, fence.close_end);
            }
        }
        let mut marks = marks.finish();
        let mut regional = 0;
        for k in 1..n {
            let (prev, c) = (chars[k - 1], chars[k]);
            regional = if is_regional(prev) { regional + 1 } else { 0 };
            if is_extender(c) || prev == '\u{200D}' || (is_regional(c) && regional % 2 == 1) {
                marks[k] = true;
            }
        }
        marks
    }

    /// Blocks Markdown links and images, `[label](dest)`, on one line.
    fn mark_links(&self, marks: &mut Marks) {
        let chars = &self.chars;
        let n = self.len();
        let mut paren_match = vec![usize::MAX; n];
        let mut parens: Vec<usize> = Vec::new();
        let mut brackets: Vec<usize> = Vec::new();
        let mut label_ends: Vec<(usize, usize)> = Vec::new();
        for (k, &c) in chars.iter().enumerate() {
            match c {
                '\n' => {
                    parens.clear();
                    brackets.clear();
                }
                '(' => parens.push(k),
                ')' => {
                    if let Some(open) = parens.pop() {
                        paren_match[open] = k;
                    }
                }
                '[' => brackets.push(k),
                ']' => {
                    if let Some(open) = brackets.pop() {
                        label_ends.push((open, k));
                    }
                }
                _ => {}
            }
        }
        for (open, close) in label_ends {
            if chars.get(close + 1) != Some(&'(') || paren_match[close + 1] == usize::MAX {
                continue;
            }
            let start = if open > 0 && chars[open - 1] == '!' {
                open - 1
            } else {
                open
            };
            marks.block(start, paren_match[close + 1] + 1);
        }
    }

    /// Positions inside constructs a cut should avoid splitting: code spans
    /// and pairs of formatting markers on one line, outside code blocks.
    fn soft_marks(&self, lines: &[Range<usize>], in_code: &[bool]) -> Vec<bool> {
        let chars = &self.chars;
        let mut marks = Marks::new(self.len());
        for line in lines {
            if in_code[line.start] || in_code[line.end] {
                continue;
            }
            let mut runs: Vec<(char, Range<usize>)> = Vec::new();
            let mut k = line.start;
            while k < line.end {
                let c = chars[k];
                let len = chars[k..line.end].iter().take_while(|&&x| x == c).count();
                if matches!(c, '`' | '*' | '_' | '~') {
                    runs.push((c, k..k + len));
                }
                k += len;
            }
            let mut ticks_to = vec![None; runs.len()];
            let mut next_by_len: Vec<(usize, usize)> = Vec::new();
            for (i, (c, run)) in runs.iter().enumerate().rev() {
                if *c != '`' {
                    continue;
                }
                let len = run.len();
                match next_by_len.iter_mut().find(|(l, _)| *l == len) {
                    Some(entry) => {
                        ticks_to[i] = Some(entry.1);
                        entry.1 = i;
                    }
                    None => next_by_len.push((len, i)),
                }
            }
            let mut i = 0;
            let mut open: [Option<usize>; 3] = [None; 3];
            while i < runs.len() {
                let (c, run) = &runs[i];
                if *c == '`' {
                    if let Some(j) = ticks_to[i] {
                        marks.block(run.start, runs[j].1.end);
                        i = j + 1;
                        continue;
                    }
                } else {
                    let slot = match c {
                        '*' => 0,
                        '_' => 1,
                        _ => 2,
                    };
                    match open[slot].take() {
                        Some(start) => marks.block(start, run.end),
                        None => open[slot] = Some(run.start),
                    }
                }
                i += 1;
            }
        }
        marks.finish()
    }

    /// The fence a chunk starting or ending at `k` must repeat or close.
    fn open_fence_at(&self, k: usize) -> Option<&Fence> {
        if k == 0 || k >= self.len() {
            return None;
        }
        self.open_at[k].map(|i| &self.fences[i])
    }

    fn fits(&self, start: usize, prefix: usize, k: usize) -> bool {
        prefix + self.lengths[k] - self.lengths[start] + self.close_len(k) <= self.max
    }

    fn close(&self, k: usize) -> Option<String> {
        let fence = self.open_fence_at(k)?;
        let mut close = String::new();
        if self.chars[k - 1] != '\n' {
            close.push('\n');
        }
        close.push_str(&self.text[fence.close.clone()]);
        Some(close)
    }

    fn close_len(&self, k: usize) -> usize {
        self.open_fence_at(k).map_or(0, |fence| {
            usize::from(self.chars[k - 1] != '\n') + self.units(&self.text[fence.close.clone()])
        })
    }

    fn allowed(&self, k: usize, rules: Rules) -> bool {
        !self.hard[k] && (matches!(rules, Rules::Hard) || !self.soft[k])
    }
}

/// Splits `doc` into pieces that fit its limit.
pub(crate) fn pieces<'a>(doc: &Doc<'a>) -> Vec<Piece<'a>> {
    let n = doc.len();
    let mut out = Vec::new();
    let mut start = 0;
    while start < n {
        let reopen = doc
            .open_fence_at(start)
            .map(|fence| &doc.text[fence.open.clone()]);
        let prefix = reopen.map_or(0, |line| doc.units(line) + 1);
        let end = if prefix + doc.lengths[n] - doc.lengths[start] <= doc.max {
            n
        } else {
            choose(doc, start, prefix)
        };
        out.push(Piece {
            reopen,
            body: doc.offsets[start]..doc.offsets[end],
            close: doc.close(end),
        });
        start = end;
    }
    out
}

/// Chooses where the chunk starting at `start` ends.
fn choose(doc: &Doc<'_>, start: usize, prefix: usize) -> usize {
    let budget = doc.max.saturating_sub(prefix);
    let base = doc.lengths[start];
    let last = start
        + doc.lengths[start..]
            .partition_point(|&len| len - base <= budget)
            .saturating_sub(1);
    let half = base + budget / 2;
    let quarter = base + budget / 4;
    let fitting = || {
        (start + 1..=last)
            .rev()
            .filter(move |&k| doc.fits(start, prefix, k))
    };
    for (rules, floor) in [(Rules::All, quarter), (Rules::Hard, base)] {
        for kind in [Break::Paragraph, Break::Line, Break::Space] {
            let found = fitting().find(|&k| {
                doc.lengths[k] >= half && doc.breaks[k] >= kind && doc.allowed(k, rules)
            });
            if let Some(k) = found {
                return k;
            }
        }
        let found = fitting().find(|&k| {
            doc.lengths[k] >= floor && doc.breaks[k] > Break::None && doc.allowed(k, rules)
        });
        if let Some(k) = found {
            return k;
        }
    }
    let inside_word = |k: usize| !doc.chars[k - 1].is_whitespace() && !doc.chars[k].is_whitespace();
    fitting()
        .find(|&k| doc.allowed(k, Rules::All) && !inside_word(k))
        .or_else(|| fitting().find(|&k| doc.allowed(k, Rules::All)))
        .or_else(|| fitting().find(|&k| doc.allowed(k, Rules::Hard)))
        .or_else(|| fitting().next())
        .unwrap_or(start + 1)
}

/// Marks positions strictly inside char ranges, in linear time however
/// the ranges nest.
struct Marks(Vec<i64>);

impl Marks {
    fn new(len: usize) -> Self {
        Self(vec![0; len + 2])
    }

    /// Marks the positions strictly between `start` and `end`.
    fn block(&mut self, start: usize, end: usize) {
        if end > start + 1 {
            self.0[start + 1] += 1;
            self.0[end] -= 1;
        }
    }

    fn finish(self) -> Vec<bool> {
        let mut depth = 0;
        let mut out: Vec<bool> = self
            .0
            .iter()
            .map(|d| {
                depth += d;
                depth > 0
            })
            .collect();
        out.pop();
        out
    }
}

fn units(c: char, unit: LengthUnit) -> usize {
    match unit {
        LengthUnit::Chars => 1,
        LengthUnit::Utf16 => c.len_utf16(),
    }
}

/// Characters that attach to the one before them: combining marks,
/// variation selectors, emoji modifiers, the zero-width joiner, the keycap
/// mark and emoji tag characters.
fn is_extender(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036F}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{200D}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FE20}'..='\u{FE2F}'
            | '\u{1F3FB}'..='\u{1F3FF}'
            | '\u{E0020}'..='\u{E007F}'
            | '\u{E0100}'..='\u{E01EF}'
    )
}

fn is_regional(c: char) -> bool {
    matches!(c, '\u{1F1E6}'..='\u{1F1FF}')
}

#[cfg(test)]
mod tests;
