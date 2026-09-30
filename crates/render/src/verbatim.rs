//! Byte ranges of Markdown that text rewrites must leave alone, found with
//! the same `pulldown-cmark` parse the Slack renderer uses.

use std::ops::Range;

use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd};

/// What besides code counts as verbatim.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    /// Code spans and code blocks, delimiters included.
    Code,
    /// Code, plus the parts of links and images that aren't their label:
    /// destinations, titles, reference names and whole autolinks.
    CodeAndLinkTargets,
}

/// Returns the verbatim ranges of `md`, sorted and without overlaps.
pub(crate) fn ranges(md: &str, scope: Scope) -> Vec<Range<usize>> {
    let links = scope == Scope::CodeAndLinkTargets;
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let mut found = Vec::new();
    let mut open: Vec<(Range<usize>, Option<Range<usize>>)> = Vec::new();
    for (event, span) in Parser::new_ext(md, options).into_offset_iter() {
        let child = match event {
            Event::Start(Tag::CodeBlock(_)) | Event::Code(_) => {
                found.push(span.clone());
                Some(span)
            }
            Event::Start(Tag::Link { link_type, .. } | Tag::Image { link_type, .. }) if links => {
                if matches!(link_type, LinkType::Autolink | LinkType::Email) {
                    found.push(span.clone());
                }
                open.push((span, None));
                None
            }
            Event::End(TagEnd::Link | TagEnd::Image) if links => {
                let Some((span, label)) = open.pop() else {
                    continue;
                };
                match label {
                    Some(label) => {
                        found.push(span.start..label.start);
                        found.push(label.end..span.end);
                    }
                    None => found.push(span.clone()),
                }
                Some(span)
            }
            _ => Some(span),
        };
        if let (Some(child), Some((_, label))) = (child, open.last_mut()) {
            *label = Some(match label.take() {
                Some(label) => label.start.min(child.start)..label.end.max(child.end),
                None => child,
            });
        }
    }
    merge(found)
}

fn merge(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.retain(|range| !range.is_empty());
    ranges.sort_by_key(|range| range.start);
    let mut out: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match out.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => out.push(range),
        }
    }
    out
}

/// Whether `range` overlaps any of `sorted`, which must come from
/// [`ranges`].
pub(crate) fn overlaps(sorted: &[Range<usize>], range: &Range<usize>) -> bool {
    let first = sorted.partition_point(|r| r.end <= range.start);
    sorted.get(first).is_some_and(|r| r.start < range.end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(md: &str, scope: Scope) -> Vec<&str> {
        ranges(md, scope).into_iter().map(|r| &md[r]).collect()
    }

    #[test]
    fn code_spans_and_blocks_include_their_delimiters() {
        let md = "a `b` c\n\n```rust\nx\n```\n\n    indented\n";
        assert_eq!(
            texts(md, Scope::Code),
            ["`b`", "```rust\nx\n```", "indented\n"]
        );
    }

    #[test]
    fn link_targets_exclude_the_label() {
        let md = "[the @docs](https://x.io/@all \"t\") and ![alt](i.png) and [](e)";
        assert_eq!(
            texts(md, Scope::CodeAndLinkTargets),
            ["[", "](https://x.io/@all \"t\")", "![", "](i.png)", "[](e)"]
        );
        assert!(texts(md, Scope::Code).is_empty());
    }

    #[test]
    fn autolinks_are_verbatim_whole() {
        let md = "see <https://x.io/@here> or <me@all.io>";
        assert_eq!(
            texts(md, Scope::CodeAndLinkTargets),
            ["<https://x.io/@here>", "<me@all.io>"]
        );
    }

    #[test]
    fn a_label_with_code_and_an_image_keeps_its_extent() {
        let md = "[`a` ![b](c.png) d](u)";
        assert_eq!(
            texts(md, Scope::CodeAndLinkTargets),
            ["[`a`", "![", "](c.png)", "](u)"]
        );
    }

    #[test]
    fn overlap_checks_the_neighbors() {
        let sorted = [2..4, 8..10];
        assert!(!overlaps(&sorted, &(0..2)));
        assert!(overlaps(&sorted, &(3..5)));
        assert!(!overlaps(&sorted, &(4..8)));
        assert!(overlaps(&sorted, &(0..20)));
        assert!(!overlaps(&sorted, &(10..12)));
    }
}
