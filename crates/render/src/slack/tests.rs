//! Tests for [`to_mrkdwn`].
//!
//! Cases marked `qm-core` are ported from qm-core's
//! `test/slack-mrkdwn.test.ts`. A few expectations differ on purpose, because
//! this renderer follows the CommonMark parse tree and escapes all literal
//! Slack control sequences; the case names say why. The rest cover behavior
//! qm-core doesn't test.

use super::*;

struct Directory(&'static [(&'static str, &'static str)]);

impl MentionDirectory for Directory {
    fn resolve(&self, name: &str) -> Option<String> {
        let name = name.to_lowercase();
        self.0
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, id)| (*id).to_string())
    }
}

const NOBODY: Directory = Directory(&[]);

const TEAM: Directory = Directory(&[
    ("ankit", "U111"),
    ("alex", "U222"),
    ("alex morgan", "U222"),
    ("ren", "U888"),
    ("renée", "U999"),
    ("here", "U911"),
    ("channel", "U912"),
]);

type Case = (&'static str, &'static str, &'static str);

fn check(directory: &Directory, cases: &[Case]) {
    let failures: Vec<String> = cases
        .iter()
        .filter_map(|&(name, input, expected)| {
            let actual = to_mrkdwn(input, directory);
            (actual != expected)
                .then(|| format!("{name}\n  input:    {input:?}\n  expected: {expected:?}\n  actual:   {actual:?}"))
        })
        .collect();
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn emphasis() {
    check(
        &NOBODY,
        &[
            (
                "qm-core: bold uses a single *",
                "**Git commands**",
                "*Git commands*",
            ),
            ("qm-core: __bold__", "__also bold__", "*also bold*"),
            ("qm-core: two bold runs", "**a** and **b**", "*a* and *b*"),
            ("qm-core: *italic* becomes _italic_", "*italic*", "_italic_"),
            ("qm-core: _italic_ stays", "_already_", "_already_"),
            (
                "qm-core: bold next to italic",
                "use **bold** not *thin*",
                "use *bold* not _thin_",
            ),
            ("qm-core: ~~strike~~", "~~gone~~", "~gone~"),
            (
                "qm-core: strike tildes are not a fence",
                "~~gone~~ stays strike",
                "~gone~ stays strike",
            ),
            ("single-tilde strike", "~gone~", "~gone~"),
            ("bold italic", "***both***", "_*both*_"),
            ("nested bold emits one pair", "**a **b** c**", "*a b c*"),
            ("nested emphasis emits one pair", "*a *b* c*", "_a b c_"),
            ("nested strike emits one pair", "~~a ~~b~~ c~~", "~a b c~"),
            ("italic inside bold", "**a *b* c**", "*a _b_ c*"),
            ("strike inside bold", "**a ~~b~~**", "*a ~b~*"),
            (
                "empty emphasis around an empty image",
                "**![](https://x.io/i.png)**",
                "*<https://x.io/i.png>*",
            ),
        ],
    );
}

#[test]
fn plain_text() {
    check(
        &NOBODY,
        &[
            ("qm-core: plain text", "hello world", "hello world"),
            ("qm-core: stray asterisks", "2 * 3 * 4", "2 * 3 * 4"),
            (
                "qm-core: word underscores",
                "file_name_here",
                "file_name_here",
            ),
            ("qm-core: empty input", "", ""),
            (
                "qm-core: a lone pipe is not a table",
                "use a | b here",
                "use a | b here",
            ),
            ("soft breaks stay line breaks", "one\ntwo", "one\ntwo"),
            ("hard breaks become line breaks", "one\\\ntwo", "one\ntwo"),
            (
                "paragraphs keep their blank line",
                "one\n\ntwo",
                "one\n\ntwo",
            ),
            (
                "extra blank lines collapse to one",
                "one\n\n\n\ntwo",
                "one\n\ntwo",
            ),
            ("trailing newlines are dropped", "one\n\n", "one"),
            ("whitespace-only input", "   \n  ", ""),
            (
                "CRLF line endings",
                "one\r\ntwo\r\n\r\n- a\r\n- b",
                "one\ntwo\n\n• a\n• b",
            ),
        ],
    );
}

#[test]
fn escaping() {
    check(
        &NOBODY,
        &[
            (
                "ampersand and angle brackets",
                "a & b < c > d",
                "a &amp; b &lt; c &gt; d",
            ),
            (
                "entities are decoded, then escaped",
                "&amp; &lt; &gt; &copy;",
                "&amp; &lt; &gt; ©",
            ),
            (
                "inline HTML is shown as text",
                "a <b>bold</b> c",
                "a &lt;b&gt;bold&lt;/b&gt; c",
            ),
            (
                "an HTML block is shown as text",
                "<div>\nhi & bye\n</div>",
                "&lt;div&gt;\nhi &amp; bye\n&lt;/div&gt;",
            ),
            (
                "an HTML comment is shown as text",
                "<!-- note -->",
                "&lt;!-- note --&gt;",
            ),
            (
                "qm-core adapted: a literal user token is escaped, not passed through",
                "already <@U111> encoded",
                "already &lt;@U111&gt; encoded",
            ),
            (
                "qm-core adapted: a literal user-group token is escaped",
                "hey <!subteam^S0B123> team",
                "hey &lt;!subteam^S0B123&gt; team",
            ),
            (
                "qm-core adapted: a literal date token is escaped",
                "<!date^1234^{ago}|then>",
                "&lt;!date^1234^{ago}|then&gt;",
            ),
            (
                "a literal channel link is escaped",
                "see <#C123|general>",
                "see &lt;#C123|general&gt;",
            ),
        ],
    );
}

#[test]
fn code_is_untouched_except_for_escaping() {
    check(
        &TEAM,
        &[
            (
                "required: a fenced block with **, < and @here",
                "```\n**not bold** if a < b then @here & @ankit\n```",
                "```\n**not bold** if a &lt; b then @here &amp; @ankit\n```",
            ),
            (
                "required: inline code with **, < and @here",
                "run `**x** < y @here @ankit` now",
                "run `**x** &lt; y @here @ankit` now",
            ),
            (
                "qm-core: inline code",
                "run `git **status**` now",
                "run `git **status**` now",
            ),
            (
                "qm-core: a fenced block is not converted",
                "```\n# not a header\n- not a bullet\n**not bold**\n```",
                "```\n# not a header\n- not a bullet\n**not bold**\n```",
            ),
            (
                "qm-core: code between converted text",
                "**bold** then `code` then *it*",
                "*bold* then `code` then _it_",
            ),
            (
                "qm-core: a URL in code is not wrapped",
                "run `curl https://x.io/raw**` now",
                "run `curl https://x.io/raw**` now",
            ),
            (
                "qm-core adapted: wire broadcasts in code are escaped, not raw",
                "code stays verbatim: `<!here>` and\n```\nnotify '<!channel>'\n```",
                "code stays verbatim: `&lt;!here&gt;` and\n```\nnotify '&lt;!channel&gt;'\n```",
            ),
            (
                "qm-core: mentions in code are not resolved",
                "`@ankit` and\n```\n@ankit\n```",
                "`@ankit` and\n```\n@ankit\n```",
            ),
            (
                "qm-core: a table inside a fence stays",
                "```\n| x | y |\n```",
                "```\n| x | y |\n```",
            ),
            (
                "the info string is kept",
                "```rust\nlet a = 1;\n```",
                "```rust\nlet a = 1;\n```",
            ),
            (
                "the info string is escaped",
                "```<x>\ny\n```",
                "```&lt;x&gt;\ny\n```",
            ),
            ("an empty fenced block", "```\n```", "```\n```"),
            (
                "a blank line inside a fence is kept",
                "```\na\n\nb\n```",
                "```\na\n\nb\n```",
            ),
            (
                "a fence holding only a blank line",
                "```\n\n```",
                "```\n\n```",
            ),
            (
                "an indented code block",
                "    let a = 1;\n    let b = 2;",
                "```\nlet a = 1;\nlet b = 2;\n```",
            ),
            (
                "qm-core: tilde fences become backtick fences",
                "~~~\n**not bold** [x](y)\n~~~",
                "```\n**not bold** [x](y)\n```",
            ),
            (
                "qm-core: a tilde fence keeps its info string",
                "~~~python\nprint('hi')\n~~~",
                "```python\nprint('hi')\n```",
            ),
            (
                "qm-core adapted: backtick runs in a code body are broken, not tilde-fenced",
                "~~~\nUse ```bash\nls\n``` to fence\n~~~",
                "```\nUse ``\u{200B}`bash\nls\n``\u{200B}` to fence\n```",
            ),
            (
                "qm-core: an unclosed backtick run after a tilde fence",
                "~~~\na\n~~~\nthen ``` dangling",
                "```\na\n```\nthen ``` dangling",
            ),
            (
                "an unclosed fence runs to the end, as CommonMark says",
                "```\n**a**\n\nb",
                "```\n**a**\n\nb\n```",
            ),
            ("a code span holding a backtick", "`` a`b ``", "`a`b`"),
        ],
    );
}

#[test]
fn links_and_images() {
    check(
        &NOBODY,
        &[
            (
                "qm-core: link",
                "see [GitHub](https://github.com)",
                "see <https://github.com|GitHub>",
            ),
            (
                "qm-core: link alone",
                "[plain](https://x.io)",
                "<https://x.io|plain>",
            ),
            (
                "qm-core: image becomes a link",
                "![alt](https://x.io/a.png)",
                "<https://x.io/a.png|alt>",
            ),
            (
                "qm-core: an autolink stays",
                "<https://x.io> stays",
                "<https://x.io> stays",
            ),
            (
                "qm-core adapted: a pipe in an autolink is percent-encoded",
                "<https://x.io|label> stays",
                "<https://x.io%7Clabel> stays",
            ),
            (
                "an image without alt text",
                "![](https://x.io/a.png)",
                "<https://x.io/a.png>",
            ),
            (
                "a link without a label",
                "[](https://x.io)",
                "<https://x.io>",
            ),
            (
                "a link title is dropped",
                "[x](https://x.io \"Title\")",
                "<https://x.io|x>",
            ),
            (
                "a reference link",
                "[x][r]\n\n[r]: https://x.io",
                "<https://x.io|x>",
            ),
            (
                "an email autolink",
                "<ada@example.com>",
                "<mailto:ada@example.com|ada@example.com>",
            ),
            (
                "a formatted label",
                "[**bold** `code`](https://x.io)",
                "<https://x.io|*bold* `code`>",
            ),
            (
                "a label with a line break",
                "[a\nb](https://x.io)",
                "<https://x.io|a b>",
            ),
            (
                "a label is escaped",
                "[a < b & c](https://x.io)",
                "<https://x.io|a &lt; b &amp; c>",
            ),
            (
                "a URL is escaped",
                "[x](https://x.io/?a=1&b=2)",
                "<https://x.io/?a=1&amp;b=2|x>",
            ),
            (
                "a pipe in a URL is encoded",
                "[x](https://x.io/a|b)",
                "<https://x.io/a%7Cb|x>",
            ),
            (
                "a space in a URL is encoded",
                "[x](<https://x.io/a b>)",
                "<https://x.io/a%20b|x>",
            ),
            (
                "a URL can't become a broadcast",
                "[x](<!here>) [y](<!channel|z>)",
                "<%21here|x> <%21channel%7Cz|y>",
            ),
            (
                "a URL can't become a user mention",
                "[x](@U123)",
                "<%40U123|x>",
            ),
            (
                "a URL can't become a channel link",
                "[x](#C123)",
                "<%23C123|x>",
            ),
            (
                "a later ! or @ in a URL stays",
                "[x](https://x.io/@a!b)",
                "<https://x.io/@a!b|x>",
            ),
            (
                "an image inside a link keeps its alt text",
                "[![logo](https://x.io/l.png)](https://x.io)",
                "<https://x.io|logo>",
            ),
            (
                "an image alt with formatting is flattened",
                "![a **b** `c`](https://x.io/i.png)",
                "<https://x.io/i.png|a b c>",
            ),
            (
                "a URL in a label naming another host goes next to the link",
                "[see https://y.io](https://x.io)",
                "see https://y.io (<https://x.io>)",
            ),
        ],
    );
}

#[test]
fn bare_urls() {
    check(
        &NOBODY,
        &[
            (
                "qm-core: a bold bare URL keeps * outside the link",
                "**https://example.awsapps.com/start/#/device**",
                "*<https://example.awsapps.com/start/#/device>*",
            ),
            (
                "qm-core: an italic bare URL",
                "*https://x.io/#/y*",
                "_<https://x.io/#/y>_",
            ),
            (
                "qm-core: an underscore-bold bare URL",
                "__https://x.io/a__",
                "*<https://x.io/a>*",
            ),
            (
                "qm-core: a trailing period stays outside",
                "see https://x.io/a.",
                "see <https://x.io/a>.",
            ),
            (
                "qm-core: an unmatched paren stays outside",
                "(see https://x.io/a)",
                "(see <https://x.io/a>)",
            ),
            (
                "qm-core: matched parens stay inside",
                "https://en.wikipedia.org/wiki/Foo_(bar)",
                "<https://en.wikipedia.org/wiki/Foo_(bar)>",
            ),
            (
                "qm-core: underscores in a URL are not emphasis",
                "https://x.io/a_b_c and *more*",
                "<https://x.io/a_b_c> and _more_",
            ),
            (
                "plain http",
                "go to http://x.io now",
                "go to <http://x.io> now",
            ),
            (
                "an ampersand in a bare URL",
                "https://x.io/?a=1&b=2",
                "<https://x.io/?a=1&amp;b=2>",
            ),
            (
                "a URL after a letter is not wrapped",
                "xhttps://x.io",
                "xhttps://x.io",
            ),
            (
                "a scheme alone is not a URL",
                "https:// is a scheme",
                "https:// is a scheme",
            ),
            ("a scheme with only punctuation", "https://.", "https://."),
            (
                "a pipe ends a bare URL",
                "https://x.io|y",
                "<https://x.io>|y",
            ),
            (
                "brackets and braces",
                "[https://x.io/a] {https://x.io/b}",
                "[<https://x.io/a>] {<https://x.io/b>}",
            ),
            (
                "matched brackets stay inside",
                "https://x.io/[a]",
                "<https://x.io/[a]>",
            ),
            (
                "quotes stay outside",
                "\"https://x.io/a\"",
                "\"<https://x.io/a>\"",
            ),
            (
                "a bare URL in a heading",
                "# https://x.io",
                "*<https://x.io>*",
            ),
            (
                "a www address is left to Slack",
                "www.example.com",
                "www.example.com",
            ),
            (
                "underscore emphasis inside a URL path",
                "https://example.com/_next_/x",
                "<https://example.com/_next_/x>",
            ),
            (
                "star emphasis inside a URL path",
                "https://x.io/a*b*c",
                "<https://x.io/a*b*c>",
            ),
            (
                "underscore bold inside a URL path",
                "see https://docs.python.org/3/library/__main__.html",
                "see <https://docs.python.org/3/library/__main__.html>",
            ),
            (
                "strikethrough inside a URL path",
                "https://x.io/~~a~~/b",
                "<https://x.io/~~a~~/b>",
            ),
            (
                "emphasis closing where the source URL goes on",
                "*see https://x.io/a*b",
                "*see <https://x.io/a>*b",
            ),
            (
                "bold wrapping a URL before a suffix",
                "**https://x.io/a**'s",
                "*<https://x.io/a>*'s",
            ),
            (
                "an entity that ends a URL ends it before emphasis",
                "https://x.io/a&lt;*b*",
                "<https://x.io/a>&lt;_b_",
            ),
            (
                "a space entity ends a URL before emphasis",
                "https://x.io/a&#32;*b*",
                "<https://x.io/a> _b_",
            ),
            (
                "inline code ends a URL before emphasis",
                "https://x.io/a`c`*b*",
                "<https://x.io/a>`c`_b_",
            ),
            (
                "a URL in a link label ends with the label",
                "[https://x.io/a](https://y.io)*b*",
                "https://x.io/a (<https://y.io>)_b_",
            ),
            (
                "an entity that stays in a URL keeps emphasis in it",
                "https://x.io/?a&amp;_b_/c",
                "<https://x.io/?a&amp;_b_/c>",
            ),
            (
                "emphasis after a URL still formats",
                "https://x.io/a *b*",
                "<https://x.io/a> _b_",
            ),
            (
                "markup inside emphasis that starts in a URL is kept",
                "https://x.io/_a [b](https://y.io)_",
                "<https://x.io/_a> <https://y.io|b>_",
            ),
            (
                "a broadcast after such a URL stays neutralized",
                "https://x.io/_a_/b @here",
                "<https://x.io/_a_/b> @\u{200B}here",
            ),
        ],
    );
}

#[test]
fn headings() {
    check(
        &NOBODY,
        &[
            ("qm-core: a heading becomes bold", "# Title", "*Title*"),
            (
                "qm-core: a heading before text",
                "### Deep\nbody",
                "*Deep*\nbody",
            ),
            (
                "qm-core: bold inside a heading",
                "## **Summary**",
                "*Summary*",
            ),
            (
                "qm-core: partial bold inside a heading",
                "# Hello **World**",
                "*Hello World*",
            ),
            ("a closing hash sequence", "## Title ##", "*Title*"),
            ("a setext heading", "Title\n=====\nbody", "*Title*\nbody"),
            ("a multi-line setext heading", "one\ntwo\n---", "*one two*"),
            ("an empty heading is dropped", "#\nbody", "body"),
            ("italic inside a heading", "# a *b*", "*a _b_*"),
            ("code inside a heading", "# run `ls`", "*run `ls`*"),
            (
                "a heading with a blank line after",
                "# T\n\nbody",
                "*T*\n\nbody",
            ),
        ],
    );
}

#[test]
fn lists() {
    check(
        &NOBODY,
        &[
            ("qm-core: dash bullets", "- one\n- two", "• one\n• two"),
            (
                "qm-core: star and plus bullets",
                "* star\n+ plus",
                "• star\n• plus",
            ),
            (
                "qm-core: ordered lists",
                "1. first\n2. second",
                "1. first\n2. second",
            ),
            (
                "qm-core: bold in a bullet",
                "- **Auth** creds",
                "• *Auth* creds",
            ),
            (
                "an ordered list keeps its start",
                "3. c\n4. d",
                "3. c\n4. d",
            ),
            (
                "ordered numbers are renumbered in order",
                "1. a\n1. b\n1. c",
                "1. a\n2. b\n3. c",
            ),
            (
                "nested bullets indent two spaces",
                "- a\n  - b\n    - c\n- d",
                "• a\n  • b\n    • c\n• d",
            ),
            (
                "a numbered list inside bullets",
                "- a\n  1. b\n  2. c",
                "• a\n  1. b\n  2. c",
            ),
            (
                "bullets inside a numbered list",
                "1. a\n   - b",
                "1. a\n  • b",
            ),
            (
                "a loose list keeps its blank lines",
                "- a\n\n- b",
                "• a\n\n• b",
            ),
            (
                "a second paragraph in an item is indented",
                "- a\n\n  more\n- b",
                "• a\n\n  more\n• b",
            ),
            ("a continuation line is indented", "- a\n  b", "• a\n  b"),
            ("an empty item", "-\n- b", "•\n• b"),
            (
                "code in an item is not indented",
                "- run:\n  ```\n  ls  -la\n  ```\n- done",
                "• run:\n```\nls  -la\n```\n• done",
            ),
            (
                "an item starting with code",
                "- ```\n  x\n  ```",
                "• ```\nx\n```",
            ),
            ("an item that is a nested list", "- - a", "• • a"),
            ("two adjacent lists", "- a\n\n1. b", "• a\n\n1. b"),
            (
                "a list after a paragraph",
                "Steps:\n- a\n- b",
                "Steps:\n• a\n• b",
            ),
            ("a quote inside an item", "- a\n  > q", "• a\n  > q"),
            (
                "task markers stay text",
                "- [ ] todo\n- [x] done",
                "• [ ] todo\n• [x] done",
            ),
        ],
    );
}

#[test]
fn blockquotes_and_rules() {
    check(
        &NOBODY,
        &[
            ("a blockquote", "> quoted", "> quoted"),
            ("a multi-line blockquote", "> one\n> two", "> one\n> two"),
            (
                "a blockquote with two paragraphs",
                "> one\n>\n> two",
                "> one\n>\n> two",
            ),
            ("a lazy continuation line", "> one\ntwo", "> one\n> two"),
            (
                "formatting in a blockquote",
                "> **bold** @here",
                "> *bold* @\u{200B}here",
            ),
            ("a nested blockquote", "> a\n> > b", "> a\n> > b"),
            ("a list in a blockquote", "> - a\n> - b", "> • a\n> • b"),
            (
                "code in a blockquote keeps the prefix",
                "> ```\n> x\n> ```",
                "> ```\n> x\n> ```",
            ),
            ("an empty blockquote", ">", ""),
            (
                "qm-core adapted: a rule between paragraphs",
                "above\n\n---\n\nbelow",
                "above\n\n──────────\n\nbelow",
            ),
            (
                "qm-core adapted: text over --- is a setext heading",
                "above\n---\nbelow",
                "*above*\nbelow",
            ),
            ("a star rule", "***", "──────────"),
            ("an underscore rule", "a\n\n___", "a\n\n──────────"),
        ],
    );
}

#[test]
fn tables() {
    check(
        &NOBODY,
        &[
            (
                "qm-core: a table becomes an aligned code block",
                "| Name | Score |\n|------|-------|\n| Alice | 91 |\n| Bo | 7 |",
                "```\nName  | Score\n------+------\nAlice | 91\nBo    | 7\n```",
            ),
            (
                "column alignment is honored",
                "| L | C | R |\n|:--|:-:|--:|\n| a | b | c |\n| long | mid | 1 |",
                "```\nL    |  C  | R\n-----+-----+--\na    |  b  | c\nlong | mid | 1\n```",
            ),
            (
                "formatting in cells is flattened",
                "| a | b |\n|---|---|\n| **x** `y` | [l](https://x.io) |",
                "```\na   | b\n----+-----------------\nx y | l (https://x.io)\n```",
            ),
            (
                "an autolink and an image in a cell",
                "| a | b |\n|---|---|\n| <https://x.io> | ![i](https://x.io/i.png) |",
                "```\na            | b\n-------------+-----------------------\nhttps://x.io | i (https://x.io/i.png)\n```",
            ),
            (
                "an empty-label link and image in cells",
                "| a | b |\n|---|---|\n| [](https://x.io) | ![](https://y.io) |",
                "```\na            | b\n-------------+-------------\nhttps://x.io | https://y.io\n```",
            ),
            (
                "cells are escaped but mentions and broadcasts stay code",
                "| who | note |\n|---|---|\n| @ankit | a < b & <!here> @here |",
                "```\nwho    | note\n-------+----------------------\n@ankit | a &lt; b &amp; &lt;!here&gt; @here\n```",
            ),
            (
                "a short row keeps the column separator",
                "| a | b |\n|---|---|\n| 1 |",
                "```\na | b\n--+--\n1 |\n```",
            ),
            (
                "width counts characters, not bytes",
                "| é | x |\n|---|---|\n| ab | y |",
                "```\né  | x\n---+--\nab | y\n```",
            ),
            (
                "an escaped pipe in a cell",
                "| a |\n|---|\n| x \\| y |",
                "```\na\n-----\nx | y\n```",
            ),
            (
                "a table holding a backtick fence breaks the run",
                "| a |\n|---|\n| ```` ``` ```` |",
                "```\na\n---\n``\u{200B}`\n```",
            ),
            (
                "a table between paragraphs",
                "before\n\n| a |\n|---|\n| b |\n\nafter",
                "before\n\n```\na\n-\nb\n```\n\nafter",
            ),
            (
                "a table in a list item is not indented",
                "- t:\n\n  | a |\n  |---|\n  | b |",
                "• t:\n\n```\na\n-\nb\n```",
            ),
        ],
    );
}

#[test]
fn broadcasts_are_neutralized() {
    check(
        &TEAM,
        &[
            (
                "qm-core: wire broadcasts are disarmed",
                "ping <!here> and <!channel|channel> and <!EVERYONE>",
                "ping @\u{200B}here and @\u{200B}channel and @\u{200B}everyone",
            ),
            (
                "qm-core: wire broadcasts with and without labels",
                "<!here> <!here|here> <!Channel>",
                "@\u{200B}here @\u{200B}here @\u{200B}channel",
            ),
            (
                "qm-core: a wire broadcast opening a line",
                "<!here> look",
                "@\u{200B}here look",
            ),
            (
                "qm-core adapted: typed broadcasts get a zero-width space",
                "@here @channel @everyone",
                "@\u{200B}here @\u{200B}channel @\u{200B}everyone",
            ),
            (
                "qm-core adapted: a typed broadcast never resolves to a member named like it",
                "hey @here look",
                "hey @\u{200B}here look",
            ),
            (
                "typed broadcasts keep their case",
                "@Here @CHANNEL",
                "@\u{200B}Here @\u{200B}CHANNEL",
            ),
            (
                "qm-core: an email address is not a broadcast",
                "me@here.com",
                "me@here.com",
            ),
            (
                "a longer word is not a broadcast",
                "@heresy @channels @here_x",
                "@heresy @channels @here_x",
            ),
            (
                "punctuation ends a typed broadcast",
                "@here, @channel.",
                "@\u{200B}here, @\u{200B}channel.",
            ),
            ("a broadcast after @", "@@here", "@@\u{200B}here"),
            (
                "a broadcast in emphasis",
                "**@channel**",
                "*@\u{200B}channel*",
            ),
            (
                "a broadcast in a heading",
                "# @everyone",
                "*@\u{200B}everyone*",
            ),
            (
                "a broadcast in a list item",
                "- <!here> now",
                "• @\u{200B}here now",
            ),
            (
                "a broadcast in a link label",
                "[@here <!channel>](https://x.io)",
                "<https://x.io|@\u{200B}here @\u{200B}channel>",
            ),
            (
                "a broadcast in image alt text",
                "![@here](https://x.io/i.png)",
                "<https://x.io/i.png|@\u{200B}here>",
            ),
            (
                "an unknown wire command is escaped",
                "<!herex> <!foo>",
                "&lt;!herex&gt; &lt;!foo&gt;",
            ),
            (
                "an unclosed wire broadcast is escaped",
                "a <!here|x b",
                "a &lt;!here|x b",
            ),
            (
                "a wire broadcast needs > or | right after the word",
                "a <!here x> <!here1>",
                "a &lt;!here x&gt; &lt;!here1&gt;",
            ),
            (
                "a backslash-escaped wire broadcast",
                "\\<!here>",
                "@\u{200B}here",
            ),
            (
                "a wire broadcast in an HTML block",
                "<!here|x>\n@ankit",
                "@\u{200B}here\n<@U111>",
            ),
        ],
    );
}

#[test]
fn mentions_resolve_through_the_directory() {
    check(
        &TEAM,
        &[
            (
                "qm-core: a mention before punctuation",
                "thanks @ankit!",
                "thanks <@U111>!",
            ),
            (
                "qm-core: a two-word name",
                "@Alex Morgan said so",
                "<@U222> said so",
            ),
            (
                "qm-core: a shorter name before lowercase words",
                "cc @alex can you look",
                "cc <@U222> can you look",
            ),
            (
                "qm-core: an unknown name stays",
                "ping @unknown-person",
                "ping @unknown-person",
            ),
            (
                "qm-core: an email address",
                "email me a@ankit.com",
                "email me a@ankit.com",
            ),
            (
                "qm-core: a mention inside a URL",
                "see https://medium.com/@ankit/post",
                "see <https://medium.com/@ankit/post>",
            ),
            (
                "qm-core: a package scope",
                "install @ankit/shared please",
                "install @ankit/shared please",
            ),
            (
                "qm-core: a mention in a link label stays text",
                "[ping @ankit](https://x.com)",
                "<https://x.com|ping @ankit>",
            ),
            (
                "qm-core: a mention in bold",
                "**@ankit** owns it",
                "*<@U111>* owns it",
            ),
            (
                "qm-core: a mention in italics",
                "_@ankit_ too",
                "_<@U111>_ too",
            ),
            (
                "qm-core: a non-ASCII name",
                "ping @Renée about it",
                "ping <@U999> about it",
            ),
            (
                "qm-core: a longer unknown name",
                "hi @Ankit Gupta Sharma Rao",
                "hi @Ankit Gupta Sharma Rao",
            ),
            (
                "qm-core: a capitalized next word is another person",
                "hi @Ankit Torres",
                "hi @Ankit Torres",
            ),
            (
                "the rest after a resolved name is text",
                "@ankit & co",
                "<@U111> &amp; co",
            ),
            ("a mention in a heading", "# Ask @ankit", "*Ask <@U111>*"),
            (
                "a mention in a blockquote",
                "> @ankit said",
                "> <@U111> said",
            ),
            ("a mention in a list item", "- @alex morgan", "• <@U222>"),
            ("a mention after an @", "@@ankit", "@@ankit"),
            ("a mention after <", "<@ankit", "&lt;@ankit"),
            ("a name with a trailing dot", "ask @ankit.", "ask <@U111>."),
            (
                "a possessive is part of the name",
                "@ankit's idea",
                "@ankit's idea",
            ),
            (
                "words after an unresolved name are still scanned",
                "@nobody https://x.io",
                "@nobody <https://x.io>",
            ),
            ("an @ alone", "a @ b", "a @ b"),
            ("an @ at the end", "mail me @", "mail me @"),
            (
                "a name that runs into @ drops its last word",
                "@alex morgan@x",
                "<@U222> morgan@x",
            ),
            ("a dotted name is one word", "@ankit.gupta", "@ankit.gupta"),
            (
                "a name split across a soft break",
                "@alex\nmorgan",
                "<@U222>\nmorgan",
            ),
            (
                "a name at the start of a line after text",
                "hi\n@ren",
                "hi\n<@U888>",
            ),
        ],
    );
}

#[test]
fn unknown_mentions_stay_text_without_a_directory() {
    check(
        &NOBODY,
        &[
            ("an unknown name", "thanks @ankit!", "thanks @ankit!"),
            ("broadcasts are still neutralized", "@here", "@\u{200B}here"),
        ],
    );
}

#[test]
fn qm_core_full_agent_reply_converts_end_to_end() {
    let md = [
        "# GitHub access",
        "",
        "1. **Git commands** — I can run `git` directly.",
        "2. **GitHub API** — HTTP requests via [the REST API](https://api.github.com).",
        "",
        "- You provide *authentication*",
        "- Or repo URLs",
    ]
    .join("\n");
    let expected = [
        "*GitHub access*",
        "",
        "1. *Git commands* — I can run `git` directly.",
        "2. *GitHub API* — HTTP requests via <https://api.github.com|the REST API>.",
        "",
        "• You provide _authentication_",
        "• Or repo URLs",
    ]
    .join("\n");
    assert_eq!(to_mrkdwn(&md, &NOBODY), expected);
}

#[test]
fn qm_core_many_tilde_fences_all_convert() {
    let md = (0..500)
        .map(|i| format!("~~~\nblock {i} **kept**\n~~~"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let out = to_mrkdwn(&md, &NOBODY);
    assert!(!out.contains("~~~"));
    assert!(out.contains("```\nblock 499 **kept**\n```"));
}

#[test]
fn long_bracket_runs_stay_linear() {
    let md = format!("https://x.io/{}", ")".repeat(200_000));
    let out = to_mrkdwn(&md, &NOBODY);
    assert!(out.starts_with("<https://x.io/>)"));
    assert_eq!(out.len(), md.len() + 2);
}

#[test]
fn many_cut_bare_urls_stay_linear() {
    let count = 10_000;
    for (unit, link) in [
        ("`c`https://a", "<https://a>"),
        ("&lt;https://a", "<https://a>"),
        ("&#32;https://a/", "<https://a/>"),
    ] {
        let md = unit.repeat(count);
        let started = std::time::Instant::now();
        let out = to_mrkdwn(&md, &NOBODY);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{unit:?}"
        );
        assert_eq!(out.matches(link).count(), count, "{unit:?}");
    }
}

#[test]
fn mention_scan_ignores_a_position_without_an_at() {
    assert!(mention::scan("ankit", 0, BROADCASTS, &TEAM).is_none());
    assert!(mention::broadcast("here", 0, BROADCASTS).is_none());
}

#[test]
fn deep_nesting_is_flattened_instead_of_overflowing_the_stack() {
    let quotes = format!("{} deep", ">".repeat(100_000));
    let expected = format!("{}deep", "> ".repeat(MAX_DEPTH));
    assert_eq!(to_mrkdwn(&quotes, &NOBODY), expected);

    let emphasis = format!("{}a{}", "*".repeat(100_000), "*".repeat(100_000));
    assert_eq!(to_mrkdwn(&emphasis, &NOBODY), "*a*");

    let lists = format!("{}- @here", "> - ".repeat(50_000));
    assert!(to_mrkdwn(&lists, &NOBODY).ends_with("@\u{200B}here"));
}

#[test]
fn line_breaks_inside_labels_and_headings_become_spaces() {
    check(
        &NOBODY,
        &[
            (
                "inline HTML across lines in a label",
                "[<a\nb>](https://x.io)",
                "<https://x.io|&lt;a b&gt;>",
            ),
            (
                "inline HTML across lines in a heading",
                "x <a\nb>\n===",
                "*x &lt;a b&gt;*",
            ),
        ],
    );
}

#[test]
fn a_label_naming_another_host_goes_next_to_the_link() {
    check(
        &NOBODY,
        &[
            (
                "a URL label",
                "[https://good.com](https://evil.com)",
                "https://good.com (<https://evil.com>)",
            ),
            (
                "a domain label",
                "[good.com](https://evil.com)",
                "good.com (<https://evil.com>)",
            ),
            (
                "a www label with a path",
                "[www.good.com/x](https://evil.com/x)",
                "www.good.com/x (<https://evil.com/x>)",
            ),
            (
                "a domain in a longer label",
                "[Log in at good.com now](https://evil.com)",
                "Log in at good.com now (<https://evil.com>)",
            ),
            (
                "a bracketed domain",
                "[(good.com)](https://evil.com)",
                "(good.com) (<https://evil.com>)",
            ),
            (
                "a domain with a port",
                "[good.com:8080](https://evil.com)",
                "good.com:8080 (<https://evil.com>)",
            ),
            (
                "the label's host as a subdomain of another",
                "[good.com](https://good.com.evil.io)",
                "good.com (<https://good.com.evil.io>)",
            ),
            (
                "the label's host as user info",
                "[good.com](https://good.com@evil.io)",
                "good.com (<https://good.com@evil.io>)",
            ),
            (
                "the label's host in the path",
                "[good.com](https://evil.io/good.com)",
                "good.com (<https://evil.io/good.com>)",
            ),
            (
                "a punycode lookalike URL",
                "[apple.com](https://xn--pple-43d.com)",
                "apple.com (<https://xn--pple-43d.com>)",
            ),
            (
                "a Cyrillic lookalike label",
                "[\u{0430}pple.com](https://apple.com)",
                "\u{0430}pple.com (<https://apple.com>)",
            ),
            (
                "a Cyrillic lookalike URL",
                "[apple.com](https://\u{0430}pple.com)",
                "apple.com (<https://\u{0430}pple.com>)",
            ),
            (
                "an ideographic full stop",
                "[good\u{3002}com](https://evil.com)",
                "good\u{3002}com (<https://evil.com>)",
            ),
            (
                "a one-dot leader",
                "[good\u{2024}com](https://evil.com)",
                "good\u{2024}com (<https://evil.com>)",
            ),
            (
                "a zero-width space inside a domain",
                "[good\u{200B}.com](https://evil.com)",
                "good\u{200B}.com (<https://evil.com>)",
            ),
            (
                "a right-to-left override",
                "[\u{202E}moc.doog](https://good.com)",
                "\u{202E}moc.doog (<https://good.com>)",
            ),
            (
                "same host: an invisible character in the label",
                "[good\u{00AD}.com](https://good.com)",
                "<https://good.com|good\u{00AD}.com>",
            ),
            (
                "an IPv4 label",
                "[10.0.0.1](https://10.6.6.6)",
                "10.0.0.1 (<https://10.6.6.6>)",
            ),
            (
                "an email label for another address's domain",
                "[ada@good.com](mailto:ada@evil.com)",
                "ada@good.com (<mailto:ada@evil.com>)",
            ),
            (
                "a URL without a scheme",
                "[good.com](evil.com)",
                "good.com (<evil.com>)",
            ),
            (
                "a formatted label",
                "[**good.com**](https://evil.com)",
                "*good.com* (<https://evil.com>)",
            ),
            (
                "an image's alt text",
                "![good.com](https://evil.com/i.png)",
                "good.com (<https://evil.com/i.png>)",
            ),
            (
                "an image inside a link",
                "[![good.com](https://good.com/l.png)](https://evil.com)",
                "good.com (<https://evil.com>)",
            ),
            (
                "same host: a word label",
                "[docs](https://good.com/a)",
                "<https://good.com/a|docs>",
            ),
            (
                "same host: a domain label",
                "[good.com](https://good.com)",
                "<https://good.com|good.com>",
            ),
            (
                "same host: case and www differ",
                "[GOOD.com](https://www.Good.COM/x)",
                "<https://www.Good.COM/x|GOOD.com>",
            ),
            (
                "same host: a www label with a path",
                "[www.good.com/a](https://good.com/a)",
                "<https://good.com/a|www.good.com/a>",
            ),
            (
                "same host: a port and a trailing dot",
                "[good.com.](https://good.com:443/)",
                "<https://good.com:443/|good.com.>",
            ),
            (
                "same host: an email address",
                "[ada@good.com](mailto:ada@good.com)",
                "<mailto:ada@good.com|ada@good.com>",
            ),
            (
                "same host: a backslash ends the host, as in browsers",
                "[good.com](https://good.com\\\\@evil.io)",
                "<https://good.com\\@evil.io|good.com>",
            ),
            (
                "words with dots that aren't domains",
                "[e.g. v1.2 of @ankit](https://x.io)",
                "<https://x.io|e.g. v1.2 of @ankit>",
            ),
        ],
    );
}

#[test]
fn a_link_without_a_url_shows_only_its_label() {
    check(
        &NOBODY,
        &[
            ("an empty URL", "a [x]() b", "a x b"),
            ("an empty bracketed URL", "[**x**](<>)", "*x*"),
            ("an image with an empty URL", "![alt]()", "alt"),
            ("an empty label and URL", "a []() b", "a  b"),
            (
                "a table cell",
                "| a |\n|---|\n| [x]() |",
                "```\na\n-\nx\n```",
            ),
        ],
    );
}

#[test]
fn emphasis_inside_a_word_keeps_its_delimiter() {
    check(
        &NOBODY,
        &[
            ("digits", "5*3*2", "5*3*2"),
            ("letters", "a*b*c", "a*b*c"),
            ("bold inside a word", "foo**bar**baz", "foo*bar*baz"),
            ("strike inside a word", "a~~b~~c", "a~b~c"),
            ("only the start touches a word", "foo*bar*", "foo*bar*"),
            ("only the end touches a word", "*foo*bar", "*foo*bar"),
            (
                "underscores inside a word",
                "snake_case_name",
                "snake_case_name",
            ),
            ("emphasis still converts", "*x*", "_x_"),
            ("bold still converts", "**x**", "*x*"),
            ("inside bold", "**5*3*2**", "*5*3*2*"),
            ("in a heading", "# 5*3*2", "*5*3*2*"),
            (
                "in a table cell",
                "| a |\n|---|\n| 5*3*2 |",
                "```\na\n-----\n5*3*2\n```",
            ),
        ],
    );
}

#[test]
fn escaped_formatting_characters_stay_literal() {
    check(
        &TEAM,
        &[
            (
                "asterisks",
                "\\*not bold\\*",
                "\u{200B}*\u{200B}not bold\u{200B}*\u{200B}",
            ),
            (
                "underscores",
                "\\_x\\_",
                "\u{200B}_\u{200B}x\u{200B}_\u{200B}",
            ),
            ("tildes", "\\~x\\~", "\u{200B}~\u{200B}x\u{200B}~\u{200B}"),
            (
                "backticks",
                "\\`x\\`",
                "\u{200B}`\u{200B}x\u{200B}`\u{200B}",
            ),
            (
                "character references",
                "&ast;x&#95;",
                "\u{200B}*\u{200B}x\u{200B}_\u{200B}",
            ),
            (
                "inside a word nothing is added",
                "snake\\_case\\_name",
                "snake_case_name",
            ),
            ("an escaped backslash before emphasis", "\\\\*x*", "\\_x_"),
            (
                "a literal star after an escaped backslash",
                "\\\\* x",
                "\\\u{200B}*\u{200B} x",
            ),
            (
                "in a link label",
                "[\\*x\\*](https://x.io)",
                "<https://x.io|\u{200B}*\u{200B}x\u{200B}*\u{200B}>",
            ),
            (
                "in a heading",
                "# \\*x\\*",
                "*\u{200B}*\u{200B}x\u{200B}*\u{200B}*",
            ),
            (
                "next to a mention",
                "\\*@ankit\\*",
                "\u{200B}*\u{200B}<@U111>\u{200B}*\u{200B}",
            ),
            (
                "in alt text",
                "![\\*x\\*](https://x.io/i.png)",
                "<https://x.io/i.png|\u{200B}*\u{200B}x\u{200B}*\u{200B}>",
            ),
            (
                "code in alt text",
                "![`*x*`](https://x.io/i.png)",
                "<https://x.io/i.png|\u{200B}*\u{200B}x\u{200B}*\u{200B}>",
            ),
            (
                "in a URL nothing is added",
                "https://x.io/a\\_b",
                "<https://x.io/a_b>",
            ),
            (
                "in a table cell nothing is added",
                "| a |\n|---|\n| \\*x\\* |",
                "```\na\n---\n*x*\n```",
            ),
        ],
    );
}

#[test]
fn backtick_runs_cant_close_a_code_block() {
    check(
        &NOBODY,
        &[
            (
                "three backticks",
                "````\n```\n````",
                "```\n``\u{200B}`\n```",
            ),
            (
                "six backticks",
                "~~~\n``````\n~~~",
                "```\n``\u{200B}``\u{200B}``\n```",
            ),
            ("two backticks stay", "~~~\n``\n~~~", "```\n``\n```"),
            (
                "an info string starting with a backtick",
                "~~~`x\ny\n~~~",
                "```\u{200B}`x\ny\n```",
            ),
        ],
    );
}

#[test]
fn a_wire_broadcast_label_is_searched_only_briefly() {
    let long = format!("<!here|{}>", "x".repeat(MAX_WIRE_LABEL));
    assert_eq!(to_mrkdwn(&long, &NOBODY), escape(&long));
    let short = format!("<!here|{}>", "x".repeat(MAX_WIRE_LABEL - 2));
    assert_eq!(to_mrkdwn(&short, &NOBODY), "@\u{200B}here");
    check(
        &NOBODY,
        &[(
            "a label doesn't run past a line break",
            "a <!here|x\nb> c",
            "a &lt;!here|x\nb&gt; c",
        )],
    );
}

#[test]
fn many_unclosed_wire_broadcasts_stay_linear() {
    let md = "<!here|".repeat(300_000);
    let started = std::time::Instant::now();
    let out = to_mrkdwn(&md, &NOBODY);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(out, escape(&md));
}
