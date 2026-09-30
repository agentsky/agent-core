//! Tests for [`split`].
//!
//! Cases marked `qm-core` are ported from qm-core's `test/safe-cut.test.ts`.
//! qm-core counts JavaScript string length, so its cases use
//! [`LengthUnit::Utf16`]. The property tests use a small hand-written
//! generator, seeded, so a failure names the seed that reproduces it.

use super::*;
use crate::MentionDirectory;
use crate::slack::to_mrkdwn;

fn chars(max: usize) -> Limit {
    Limit {
        max,
        unit: LengthUnit::Chars,
    }
}

fn utf16(max: usize) -> Limit {
    Limit {
        max,
        unit: LengthUnit::Utf16,
    }
}

fn len(s: &str, unit: LengthUnit) -> usize {
    match unit {
        LengthUnit::Chars => s.chars().count(),
        LengthUnit::Utf16 => s.encode_utf16().count(),
    }
}

/// Splits and checks the invariants every split must hold: chunks fit,
/// aren't empty, are consecutive pieces of the text once the added fence
/// lines are removed, and close exactly the fences the next chunk reopens.
fn split_checked(text: &str, limit: Limit) -> Vec<String> {
    let doc = Doc::new(text, limit);
    let pieces = pieces(&doc);
    let chunks = split(text, limit);
    assert_eq!(chunks.len(), pieces.len());
    let mut at = 0;
    for (i, (piece, chunk)) in pieces.iter().zip(&chunks).enumerate() {
        assert_eq!(piece.body.start, at, "piece {i} isn't consecutive");
        assert!(!piece.body.is_empty(), "piece {i} is empty");
        at = piece.body.end;
        let mut expected = String::new();
        if let Some(reopen) = piece.reopen {
            expected.push_str(reopen);
            expected.push('\n');
        }
        expected.push_str(&text[piece.body.clone()]);
        expected.push_str(piece.close.as_deref().unwrap_or(""));
        assert_eq!(chunk, &expected, "piece {i} renders differently");
        if limit.max >= 2 {
            assert!(
                len(chunk, limit.unit) <= limit.max,
                "chunk {i} has {} units, over {}: {chunk:?}",
                len(chunk, limit.unit),
                limit.max
            );
        }
        let next_reopens = pieces.get(i + 1).is_some_and(|next| next.reopen.is_some());
        assert_eq!(
            piece.close.is_some(),
            next_reopens,
            "chunk {i} closes a fence the next chunk doesn't reopen, or the reverse"
        );
    }
    assert_eq!(at, text.len(), "the pieces don't cover the text");
    chunks
}

/// Joins the chunks after removing the fence lines the splitter added.
fn rejoin(text: &str, limit: Limit) -> String {
    let doc = Doc::new(text, limit);
    pieces(&doc)
        .iter()
        .map(|piece| &text[piece.body.clone()])
        .collect()
}

/// Whether every fence a chunk opens is closed in the same chunk.
fn fences_balanced(chunk: &str) -> bool {
    let mut open: Option<(char, usize)> = None;
    for line in chunk.split('\n') {
        let body = line.trim_start_matches([' ', '>']);
        let Some(marker) = body.chars().next().filter(|c| matches!(c, '`' | '~')) else {
            continue;
        };
        let run = body.chars().take_while(|&c| c == marker).count();
        if run < 3 {
            continue;
        }
        let rest = &body[run..];
        open = match open {
            Some((c, n)) if c == marker && run >= n && rest.trim().is_empty() => None,
            Some(other) => Some(other),
            None if marker == '`' && rest.contains('`') => None,
            None => Some((marker, run)),
        };
    }
    open.is_none()
}

/// The byte offsets at which the text was cut.
fn cuts(text: &str, limit: Limit) -> Vec<usize> {
    let doc = Doc::new(text, limit);
    let pieces = pieces(&doc);
    pieces[..pieces.len().saturating_sub(1)]
        .iter()
        .map(|piece| piece.body.end)
        .collect()
}

#[test]
fn empty_text_gives_no_chunks() {
    assert!(split_checked("", chars(10)).is_empty());
}

#[test]
fn text_that_fits_is_one_chunk() {
    assert_eq!(
        split_checked("abc", utf16(10)),
        ["abc"],
        "qm-core: short text"
    );
    assert_eq!(split_checked("exactly10!", chars(10)), ["exactly10!"]);
}

#[test]
fn paragraph_breaks_come_first() {
    let text = "first para line\nsecond line\n\nnext para here";
    assert_eq!(
        split_checked(text, chars(40)),
        ["first para line\nsecond line\n\n", "next para here"]
    );
}

#[test]
fn line_breaks_come_before_spaces() {
    let text = "one two three\nfour five six seven";
    assert_eq!(
        split_checked(text, chars(20)),
        ["one two three\n", "four five six seven"]
    );
}

#[test]
fn spaces_come_before_cutting_a_word() {
    let text = "alpha beta gamma delta";
    assert_eq!(
        split_checked(text, chars(13)),
        ["alpha beta ", "gamma delta"]
    );
}

#[test]
fn a_break_in_the_first_half_loses_to_a_weaker_one_in_the_second() {
    let text = "ab\n\ncdefghij klmnopqrstuvwxyz";
    assert_eq!(
        split_checked(text, chars(20)),
        ["ab\n\ncdefghij ", "klmnopqrstuvwxyz"]
    );
}

#[test]
fn a_word_longer_than_the_limit_is_cut() {
    let text = "x".repeat(25);
    assert_eq!(
        split_checked(&text, chars(10)),
        ["x".repeat(10), "x".repeat(10), "x".repeat(5)]
    );
}

#[test]
fn slack_tokens_are_not_cut() {
    let text =
        "see <https://example.test/very/long|the docs> for more, plus trailing text to split";
    let chunks = split_checked(text, utf16(50));
    for chunk in &chunks {
        assert_eq!(
            chunk.matches('<').count(),
            chunk.matches('>').count(),
            "qm-core: a <url|label> entity that fits is not bisected: {chunk:?}"
        );
    }
    assert_eq!(chunks.concat(), text);
}

#[test]
fn a_url_token_at_the_limit() {
    let token = "<https://example.com/path|docs>";
    let max = 40;
    let exact = format!("{}{token} tail", "a".repeat(max - token.len()));
    let chunks = split_checked(&exact, chars(max));
    assert_eq!(
        chunks[0],
        format!("{}{token}", "a".repeat(max - token.len()))
    );

    let over = format!("{} {token} tail", "a".repeat(max - token.len()));
    let chunks = split_checked(&over, chars(max));
    assert_eq!(chunks[0], format!("{} ", "a".repeat(max - token.len())));
    assert!(chunks[1].starts_with(token));

    let glued = format!("{}{token}", "a".repeat(max - token.len() + 1));
    let chunks = split_checked(&glued, chars(max));
    assert_eq!(chunks[1], token, "the token moves whole to the next chunk");
}

#[test]
fn a_bare_url_at_the_limit_moves_whole() {
    let url = "https://example.com/a/very/long/path";
    let text = format!("see {url} now");
    assert_eq!(
        split_checked(&text, chars(url.len() + 2)),
        ["see ", &format!("{url} "), "now"]
    );
}

#[test]
fn a_token_longer_than_the_limit_still_cuts() {
    let text = format!("<{}>", "x".repeat(30));
    let chunks = split_checked(&text, chars(10));
    assert_eq!(chunks.len(), 4);
    let text = format!("<{}", "x".repeat(100));
    let chunks = split_checked(&text, utf16(10));
    assert!(
        chunks.len() > 1,
        "qm-core: a pathological single entity longer than the budget still cuts"
    );
    assert_eq!(chunks.concat(), text);
}

#[test]
fn entities_are_not_cut() {
    let text = "a &amp; b &lt;c&gt; d";
    for max in 5..text.len() {
        for cut in cuts(text, chars(max)) {
            for entity in ["&amp;", "&lt;", "&gt;"] {
                for (start, _) in text.match_indices(entity) {
                    assert!(
                        cut <= start || cut >= start + entity.len(),
                        "max {max} cut {entity} at {cut}"
                    );
                }
            }
        }
    }
}

#[test]
fn markdown_links_and_images_are_not_cut() {
    let text = "go to [the docs page](https://x.io/a) or ![a chart](c.png) now";
    let link = 6..37;
    let image = 41..58;
    assert_eq!(&text[link.clone()], "[the docs page](https://x.io/a)");
    assert_eq!(&text[image.clone()], "![a chart](c.png)");
    for max in 32..text.len() {
        for cut in cuts(text, chars(max)) {
            assert!(
                !(link.start < cut && cut < link.end),
                "max {max} cut the link"
            );
            assert!(
                !(image.start < cut && cut < image.end),
                "max {max} cut the image"
            );
        }
    }
}

#[test]
fn mentions_are_not_cut() {
    let text = "ping @ada.lovelace-king now";
    for max in 20..text.len() {
        for cut in cuts(text, chars(max)) {
            assert!(!(5 < cut && cut < 23), "max {max} cut the mention at {cut}");
        }
    }
    assert_eq!(
        split_checked("aaaaaaa @bob", chars(10)),
        ["aaaaaaa ", "@bob"]
    );
}

#[test]
fn an_emoji_at_the_limit() {
    let text = format!("{}😀bbb", "a".repeat(9));
    assert_eq!(
        split_checked(&text, chars(10)),
        [format!("{}😀", "a".repeat(9)), "bbb".to_string()],
        "one char in Chars"
    );
    assert_eq!(
        split_checked(&text, utf16(10)),
        ["a".repeat(9), "😀bbb".to_string()],
        "two units in UTF-16"
    );
    let text = format!("{}😀{}", "a".repeat(9), "b".repeat(10));
    for max in 8..=12 {
        for chunk in split_checked(&text, utf16(max)) {
            assert!(
                chunk.encode_utf16().count() <= max,
                "qm-core: an emoji straddling the boundary is never split"
            );
        }
    }
}

#[test]
fn grapheme_extenders_stay_with_their_base() {
    let family = "👩\u{200D}👩\u{200D}👧";
    let text = format!("{family}{family}");
    let chunks = split_checked(&text, utf16(9));
    assert_eq!(chunks, [family, family]);

    let flags = "🇯🇵🇫🇷";
    assert_eq!(split_checked(flags, utf16(5)), ["🇯🇵", "🇫🇷"]);
    assert_eq!(split_checked(flags, utf16(6)), ["🇯🇵", "🇫🇷"]);

    let accents = "e\u{0301}e\u{0301}e\u{0301}";
    assert_eq!(
        split_checked(accents, chars(3)),
        ["e\u{0301}", "e\u{0301}", "e\u{0301}"]
    );

    let toned = "👍🏽👍🏽";
    assert_eq!(split_checked(toned, utf16(5)), ["👍🏽", "👍🏽"]);
}

#[test]
fn a_limit_below_one_character_still_makes_progress() {
    assert_eq!(split_checked("😀😀", utf16(1)), ["😀", "😀"]);
    assert_eq!(split_checked("ab", chars(0)), ["a", "b"]);
}

#[test]
fn formatting_pairs_are_kept_whole_when_they_fit() {
    let text = "start *bold text* trailing words follow here";
    for chunk in split_checked(text, utf16(14)) {
        assert_eq!(
            chunk.matches('*').count() % 2,
            0,
            "qm-core: a bold run is not left dangling open: {chunk:?}"
        );
    }
    assert_eq!(
        split_checked("see `a b c` then", chars(10)),
        ["see ", "`a b c` ", "then"]
    );
}

#[test]
fn a_formatting_pair_gives_way_before_a_tiny_chunk() {
    let text = format!("a_b {} c_d", "word ".repeat(10));
    let chunks = split_checked(&text, chars(30));
    assert!(chunks[0].len() > 20, "{chunks:?}");
}

#[test]
fn chunks_reassemble() {
    let text = format!(
        "{} <https://x.test|link> {}",
        "🎉".repeat(50),
        "*bold*".repeat(30)
    );
    assert_eq!(split_checked(&text, utf16(37)).concat(), text, "qm-core");
    assert_eq!(split_checked(&text, utf16(7)).concat(), text, "qm-core");
}

#[test]
fn a_fence_is_closed_and_reopened_with_its_info_string() {
    let text = "Intro.\n\n```rust\nlet a = 1;\nlet b = 2;\nlet c = 3;\n```\nDone.";
    let chunks = split_checked(text, chars(30));
    assert_eq!(
        chunks,
        [
            "Intro.\n\n```rust\nlet a = 1;\n```",
            "```rust\nlet b = 2;\n```",
            "```rust\nlet c = 3;\n```\nDone.",
        ]
    );
    assert_eq!(rejoin(text, chars(30)), text);
}

#[test]
fn qm_core_fenced_block_is_closed_and_reopened() {
    let code: Vec<String> = (0..60).map(|i| format!("row {i} | value {i:04}")).collect();
    let text = format!("intro prose\n```\n{}\n```\ntail prose", code.join("\n"));
    let chunks = split_checked(&text, utf16(200));
    assert!(chunks.len() > 2, "the fence spans several chunks");
    for chunk in &chunks {
        assert!(fences_balanced(chunk), "dangling fence in {chunk:?}");
    }
    assert_eq!(rejoin(&text, utf16(200)), text);
}

#[test]
fn qm_core_continuation_chunks_reopen_the_fence() {
    let text = format!("```\n{}```", "line\n".repeat(200));
    let chunks = split_checked(&text, utf16(150));
    for chunk in &chunks[1..] {
        assert!(chunk.starts_with("```\n"), "not reopened: {chunk:?}");
    }
    for chunk in &chunks[..chunks.len() - 1] {
        assert!(chunk.ends_with("\n```"), "not closed: {chunk:?}");
    }
}

#[test]
fn qm_core_a_fence_that_fits_is_untouched() {
    let text = format!(
        "{}\n```\ntiny\n```\n{}",
        "before ".repeat(40),
        "after ".repeat(40)
    );
    let chunks = split_checked(&text, utf16(500));
    assert_eq!(chunks.concat(), text);
    assert!(chunks.iter().all(|chunk| fences_balanced(chunk)));
}

#[test]
fn qm_core_prose_without_fences() {
    let text = "word ".repeat(1000);
    let chunks = split_checked(&text, utf16(300));
    assert_eq!(chunks.concat(), text);
    assert!(chunks.iter().all(|chunk| chunk.ends_with(' ')));
}

#[test]
fn a_ten_thousand_character_code_block() {
    let mut body = String::new();
    let mut i = 0;
    while body.len() < 10_000 {
        body.push_str(&format!("fn f{i}() -> u32 {{ {i} }}\n"));
        i += 1;
    }
    let text = format!("Here:\n\n```rust\n{body}```\n\nThat's all.");
    let limit = crate::slack::MESSAGE_LIMIT;
    let chunks = split_checked(&text, limit);
    assert!(chunks.len() >= 4, "{} chunks", chunks.len());
    for (i, chunk) in chunks.iter().enumerate() {
        assert!(chunk.chars().count() <= limit.max);
        assert!(fences_balanced(chunk), "chunk {i} is unbalanced");
        if i > 0 && i < chunks.len() - 1 {
            assert!(chunk.starts_with("```rust\n"), "chunk {i} isn't reopened");
            assert!(chunk.ends_with("\n```"), "chunk {i} isn't closed");
        }
    }
    assert_eq!(rejoin(&text, limit), text);

    let single_line = format!("```\n{}\n```", "x".repeat(10_000));
    let chunks = split_checked(&single_line, limit);
    assert_eq!(chunks.len(), 4);
    assert!(chunks.iter().all(|chunk| fences_balanced(chunk)));
    assert!(chunks[1].starts_with("```\nxxx") && chunks[1].ends_with("xxx\n```"));
}

#[test]
fn quoted_and_tilde_fences_reopen_with_their_prefix() {
    let text = "> ```sh\n> one\n> two\n> three\n> ```";
    let chunks = split_checked(text, chars(26));
    assert_eq!(
        chunks,
        ["> ```sh\n> one\n> two\n> ```", "> ```sh\n> three\n> ```"]
    );
    let text = "~~~\n```\ninner\n```\n~~~";
    let chunks = split_checked(text, chars(17));
    assert_eq!(chunks, ["~~~\n```\ninner\n~~~", "~~~\n```\n~~~"]);
}

#[test]
fn a_closing_fence_needs_the_opening_quote_depth() {
    let text = "```\n> ```\nbody\n```\nafter the block";
    let chunks = split_checked(text, chars(16));
    assert_eq!(
        chunks,
        ["```\n> ```\n```", "```\nbody\n```\n", "after the block"]
    );
}

#[test]
fn an_unclosed_fence_stays_unclosed_at_the_end() {
    let text = "```py\naaaa\nbbbb\ncccc";
    let chunks = split_checked(text, chars(14));
    assert_eq!(
        chunks,
        ["```py\naaaa\n```", "```py\nbbbb\n```", "```py\ncccc"]
    );
}

#[test]
fn a_cut_never_leaves_an_empty_block() {
    let text = "para one here\n```\ncode\n```\nafter";
    for max in 12..text.len() {
        for chunk in split_checked(text, chars(max)) {
            assert!(!chunk.contains("```\n```"), "max {max}: {chunk:?}");
        }
    }
}

#[test]
fn a_fence_too_long_to_repeat_is_split_like_text() {
    let text = format!("```{}\na\nb\n```", "i".repeat(20));
    let chunks = split_checked(&text, chars(30));
    assert_eq!(chunks.concat(), text);
}

#[test]
fn inline_backtick_runs_are_not_fences() {
    let text = "```a``` here\nand more text here";
    let chunks = split_checked(text, chars(20));
    assert_eq!(chunks, ["```a``` here\n", "and more text here"]);
}

#[test]
fn many_constructs_stay_linear() {
    let brackets = format!("{}x{}", "[".repeat(50_000), "](y)".repeat(50_000));
    assert_eq!(rejoin(&brackets, chars(3000)), brackets);
    let angles = "<a".repeat(100_000);
    assert_eq!(rejoin(&angles, chars(3000)), angles);
    let ticks: String = (1..400).map(|n| "`".repeat(n) + " ").collect();
    assert_eq!(rejoin(&ticks, chars(3000)), ticks);
}

/// A seeded xorshift generator, so property failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

const ATOMS: &[&str] = &[
    "<https://example.com/a/b|a label>",
    "<@U12345>",
    "<#C123|general>",
    "<https://x.io/very/long/path?q=1>",
    "&amp;",
    "&lt;",
    "&gt;",
    "[docs page](https://x.io/p)",
    "![alt text](i.png)",
    "👩\u{200D}👩\u{200D}👧",
    "🇯🇵",
    "👍🏽",
    "e\u{0301}",
    "😀",
];

const OPENERS: &[&str] = &["```", "```rust", "~~~", "~~~~ text", "> ```sh"];

/// Generated text and the byte ranges no cut may fall strictly inside.
struct Sample {
    text: String,
    atoms: Vec<Range<usize>>,
    unclosed: bool,
}

fn sample(rng: &mut Rng) -> Sample {
    let mut text = String::new();
    let mut atoms = Vec::new();
    let mut unclosed = false;
    let pieces = rng.below(80);
    for _ in 0..pieces {
        match rng.below(14) {
            0..=3 => {
                let len = 1 + rng.below(8);
                text.extend((0..len).map(|_| (b'a' + rng.below(26) as u8) as char));
            }
            4 | 5 => text.push(' '),
            6 => text.push('\n'),
            7 => text.push_str("\n\n"),
            8 => {
                let atom = rng.pick(ATOMS);
                atoms.push(text.len()..text.len() + atom.len());
                text.push_str(atom);
            }
            9 => {
                text.push(' ');
                let name = rng.pick(&["@ada.l", "@bob", "@renée-x"]);
                atoms.push(text.len()..text.len() + name.len());
                text.push_str(name);
                text.push(' ');
            }
            10 => text.push_str(rng.pick(&["`code span`", "*bold words*", "_em_", "日本語"])),
            11 => text.push_str(&"y".repeat(20 + rng.below(150))),
            _ => {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                let opener = rng.pick(OPENERS);
                let quoted = opener.starts_with('>');
                atoms.push(text.len()..text.len() + opener.len() + 1);
                text.push_str(opener);
                text.push('\n');
                for _ in 0..rng.below(12) {
                    if quoted {
                        text.push_str("> ");
                    }
                    let len = rng.below(40);
                    text.extend((0..len).map(|_| rng.pick(&["x", " ", "é", "😀", "<"])));
                    text.push('\n');
                }
                if rng.below(10) == 0 {
                    unclosed = true;
                    break;
                }
                let close: String = opener
                    .chars()
                    .take_while(|c| matches!(c, '>' | ' ' | '`' | '~'))
                    .collect();
                let close = close.trim_end();
                atoms.push(text.len()..text.len() + close.len() + 1);
                text.push_str(close);
                text.push('\n');
            }
        }
    }
    Sample {
        text,
        atoms,
        unclosed,
    }
}

#[test]
fn property_chunks_fit_and_rejoin() {
    for seed in 1..=3000u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let sample = sample(&mut rng);
        let max = 2 + rng.below(300);
        let limit = if rng.below(2) == 0 {
            chars(max)
        } else {
            utf16(max)
        };
        let result = std::panic::catch_unwind(|| {
            let chunks = split_checked(&sample.text, limit);
            assert_eq!(rejoin(&sample.text, limit), sample.text);
            chunks
        });
        let Ok(chunks) = result else {
            panic!("seed {seed}, {limit:?}, text {:?}", sample.text);
        };
        if limit.max >= 80 {
            let last = chunks.len().saturating_sub(1);
            for (i, chunk) in chunks.iter().enumerate() {
                assert!(
                    fences_balanced(chunk) || (i == last && sample.unclosed),
                    "seed {seed}, {limit:?}: chunk {i} is unbalanced: {chunk:?}"
                );
            }
            for cut in cuts(&sample.text, limit) {
                for atom in &sample.atoms {
                    assert!(
                        !(atom.start < cut && cut < atom.end),
                        "seed {seed}, {limit:?}: cut at {cut} inside {:?}",
                        &sample.text[atom.clone()]
                    );
                }
            }
        }
    }
}

struct Team;

impl MentionDirectory for Team {
    fn resolve(&self, name: &str) -> Option<String> {
        match name.to_lowercase().as_str() {
            "ada" => Some("U0ADA".into()),
            "bob smith" => Some("U0BOB".into()),
            _ => None,
        }
    }
}

const MARKDOWN: &[&str] = &[
    "**bold words**",
    "_em_",
    "[the label](https://x.io/a_b?c=d&e=f)",
    "https://bare.example/path",
    "a & b",
    "x < y > z",
    "<b>tag</b>",
    "@Ada",
    "@Bob Smith",
    "@here",
    "`code <x> & y`",
    "\n\n```rust\nlet a = b && c;\nif a < b { x }\n```\n\n",
    "\n\n- item one\n- item two\n\n",
    "\n\n| a | b |\n|---|---|\n| 1 | <2> |\n\n",
    "\n\n> quoted & <text>\n\n",
    "\n\n# Heading & more\n\n",
    "🎉",
    " ",
    "\n",
];

#[test]
fn property_render_then_split_keeps_slack_tokens_whole() {
    for seed in 1..=1500u64 {
        let mut rng = Rng(seed.wrapping_mul(0xD1B5_4A32_D192_ED03));
        let mut md = String::new();
        for _ in 0..rng.below(60) {
            md.push_str(rng.pick(MARKDOWN));
            md.push_str(rng.pick(&[" ", "", "word ", "\n"]));
        }
        let rendered = to_mrkdwn(&md, &Team);
        let max = 80 + rng.below(400);
        let limit = chars(max);
        let chunks = split_checked(&rendered, limit);
        assert_eq!(rejoin(&rendered, limit), rendered);
        assert!(chunks.iter().all(|chunk| fences_balanced(chunk)));
        let mut tokens = Vec::new();
        for (start, c) in rendered.char_indices() {
            let end = match c {
                '<' => rendered[start..].find('>').map(|i| start + i + 1),
                '&' => rendered[start..].find(';').map(|i| start + i + 1),
                _ => None,
            };
            if let Some(end) = end {
                tokens.push(start..end);
            }
        }
        for cut in cuts(&rendered, limit) {
            for token in &tokens {
                assert!(
                    !(token.start < cut && cut < token.end),
                    "seed {seed}, max {max}: cut at {cut} inside {:?}",
                    &rendered[token.clone()]
                );
            }
        }
    }
}

#[test]
fn render_then_split_moves_an_entity_and_a_link_whole() {
    let md = format!("{} & [docs](https://x.io)", "a".repeat(16));
    let rendered = to_mrkdwn(&md, &Team);
    assert_eq!(
        rendered,
        format!("{} &amp; <https://x.io|docs>", "a".repeat(16))
    );
    assert_eq!(
        split_checked(&rendered, chars(20)),
        [
            &format!("{} ", "a".repeat(16)),
            "&amp; ",
            "<https://x.io|docs>"
        ]
    );
}
