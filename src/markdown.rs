//! Markdown for notifications: the plain text of a message for push (`StripMarkdownAndDecode`) and its HTML for
//! email (`MarkdownToHTML`, GFM).

// Based on Mattermost server/channels/utils/markdown.go, Copyright (c) 2015-present Mattermost, Inc. Licensed
// under AGPL-3.0.

use std::sync::LazyLock;

use comrak::nodes::NodeValue;
use comrak::{Arena, Options};
use regex::Regex;

/// `StripMarkdown` with the notification renderer: text only, blocks separated by a space, code kept, links and
/// images by their text, raw HTML and `<autolinks>` dropped.
pub fn strip(md: &str) -> String {
    let arena = Arena::new();
    let mut opts = Options::default();
    opts.extension.strikethrough = true;
    let root = comrak::parse_document(&arena, md, &opts);
    let mut out = String::new();
    walk(root, md, &mut out);
    out.trim().to_string()
}

/// `StripMarkdownAndDecode`: stripped, entities decoded.
pub fn strip_and_decode(md: &str) -> String {
    html_escape::decode_html_entities(&strip(md)).into_owned()
}

fn source_char(src: &str, line: usize, col: usize) -> Option<u8> {
    let l = src.split('\n').nth(line.checked_sub(1)?)?;
    l.as_bytes().get(col.checked_sub(1)?).copied()
}

fn walk<'a>(n: &'a comrak::nodes::AstNode<'a>, src: &str, out: &mut String) {
    let (value, pos) = {
        let d = n.data();
        (d.value.clone(), d.sourcepos)
    };
    match &value {
        NodeValue::Text(t) => out.push_str(t),
        NodeValue::Code(c) => out.push_str(&c.literal),
        NodeValue::SoftBreak | NodeValue::LineBreak => out.push('\n'),
        NodeValue::CodeBlock(c) => out.push_str(&c.literal),
        NodeValue::HtmlBlock(_) | NodeValue::HtmlInline(_) | NodeValue::ThematicBreak(_) => {}
        // goldmark's AutoLink has no children: `<http://x>` leaves nothing.
        NodeValue::Link(_) if source_char(src, pos.start.line, pos.start.column) == Some(b'<') => {}
        _ => {
            for c in n.children() {
                walk(c, src, out);
            }
        }
    }
    let item = matches!(value, NodeValue::Heading(_) | NodeValue::Paragraph | NodeValue::Item(_));
    if item && n.next_sibling().is_some() {
        out.push(' ');
    }
}

static REL_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[(.*)]\((/.*)\)").unwrap());
static BLOCKQUOTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(^|\n)(&gt;)").unwrap());

/// `html.EscapeString` then `MarkdownToHTML(siteURL)` (GFM), as Go prepares a message for email. goldmark reads the
/// escaped quotes back as characters, as comrak does.
pub fn escaped_to_html(text: &str, site_url: &str) -> String {
    let mut esc = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '<' => esc.push_str("&lt;"),
            '>' => esc.push_str("&gt;"),
            '&' => esc.push_str("&amp;"),
            '\'' => esc.push_str("&#39;"),
            '"' => esc.push_str("&#34;"),
            c => esc.push(c),
        }
    }
    let abs =
        REL_LINK.replace_all(&esc, |c: &regex::Captures| format!("[{}]({}{})", &c[1], site_url, &c[2])).into_owned();
    let clean = BLOCKQUOTE.replace_all(&abs, "$1>").into_owned();
    let mut opts = Options::default();
    opts.extension.strikethrough = true;
    opts.extension.table = true;
    opts.extension.autolink = true;
    opts.extension.tasklist = true;
    comrak::markdown_to_html(&clean, &opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_like_go() {
        assert_eq!(strip_and_decode("**hi** _there_ `x<y`"), "hi there x<y");
        assert_eq!(strip_and_decode("# Title\n\npara one\nline two"), "Title para one\nline two");
        assert_eq!(strip_and_decode("see [link](http://a) and <http://b>"), "see link and");
        assert_eq!(strip_and_decode("a &amp; b"), "a & b");
    }

    #[test]
    fn html_for_email() {
        assert_eq!(escaped_to_html("it's **bold**", "http://s"), "<p>it's <strong>bold</strong></p>\n");
        assert_eq!(escaped_to_html("> quote", "http://s"), "<blockquote>\n<p>quote</p>\n</blockquote>\n");
        assert_eq!(escaped_to_html("[x](/y)", "http://s"), "<p><a href=\"http://s/y\">x</a></p>\n");
    }
}
