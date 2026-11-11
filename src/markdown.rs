//! Markdown support: GFM-flavoured parsing plus a default stylesheet, wrapped
//! into a standalone HTML document that Blitz can lay out.
//!
//! Kept deliberately small. The markdown is converted to HTML and then handed
//! to the ordinary `HtmlDocument` path, so there is no second rendering
//! pipeline to maintain — anything Blitz can style, markdown output inherits.

use std::borrow::Cow;

use pulldown_cmark::{Options, Parser, html};

/// The CSS lives in a macro so it can be used both as a plain `&str` and as a
/// NUL-terminated literal for the C accessor, without a runtime allocation.
macro_rules! default_css {
    () => {
        r#"
:root {
  color-scheme: light dark;
  --fg: #1f2328;
  --fg-muted: #59636e;
  --bg: #ffffff;
  --border: #d1d9e0;
  --code-bg: #f6f8fa;
  --link: #0969da;
}

@media (prefers-color-scheme: dark) {
  :root {
    --fg: #d1d7e0;
    --fg-muted: #9198a1;
    --bg: #0d1117;
    --border: #3d444d;
    --code-bg: #151b23;
    --link: #4493f8;
  }
}

html { background: var(--bg); }

body {
  margin: 0;
  padding: 40px;
  color: var(--fg);
  background: var(--bg);
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", "Noto Sans",
               "DejaVu Sans", Helvetica, Arial, sans-serif;
  font-size: 16px;
  line-height: 1.6;
  word-wrap: break-word;
}

h1, h2, h3, h4, h5, h6 {
  margin-top: 24px;
  margin-bottom: 16px;
  font-weight: 600;
  line-height: 1.25;
}

h1 { font-size: 2em; padding-bottom: 0.3em; border-bottom: 1px solid var(--border); }
h2 { font-size: 1.5em; padding-bottom: 0.3em; border-bottom: 1px solid var(--border); }
h3 { font-size: 1.25em; }
h4 { font-size: 1em; }
h5 { font-size: 0.875em; }
h6 { font-size: 0.85em; color: var(--fg-muted); }

body > *:first-child { margin-top: 0; }
body > *:last-child { margin-bottom: 0; }

p, blockquote, ul, ol, dl, table, pre { margin-top: 0; margin-bottom: 16px; }

a { color: var(--link); text-decoration: none; }

strong { font-weight: 600; }

blockquote {
  padding: 0 1em;
  color: var(--fg-muted);
  border-left: 0.25em solid var(--border);
}

ul, ol { padding-left: 2em; }
/* Blitz draws an outside ordered marker with no space after it, so "1." touches
   the text. Padding on the item moves the text and leaves the marker where it
   is. Bullets are not affected and need nothing. Drop this rule when Blitz puts
   the space in the marker. */
ol > li { padding-left: 0.4em; }
li + li { margin-top: 0.25em; }
li > p { margin-bottom: 0; }

code, pre, tt {
  font-family: ui-monospace, "SFMono-Regular", "Liberation Mono",
               "DejaVu Sans Mono", Menlo, Consolas, monospace;
  font-size: 0.85em;
}

code {
  padding: 0.2em 0.4em;
  background: var(--code-bg);
  border-radius: 6px;
}

pre {
  padding: 16px;
  overflow: auto;
  line-height: 1.45;
  background: var(--code-bg);
  border-radius: 6px;
}

pre > code {
  padding: 0;
  background: transparent;
  border-radius: 0;
}

table {
  display: table;
  border-collapse: collapse;
  border-spacing: 0;
  max-width: 100%;
}

th, td {
  padding: 6px 13px;
  border: 1px solid var(--border);
}

th { font-weight: 600; background: var(--code-bg); }

hr {
  height: 0.25em;
  padding: 0;
  margin: 24px 0;
  background: var(--border);
  border: 0;
}

img { max-width: 100%; }

del { text-decoration: line-through; }

/* Task list checkboxes are emitted as <input type="checkbox" disabled>. */
input[type="checkbox"] { margin-right: 0.5em; }

.footnote-definition { font-size: 0.85em; color: var(--fg-muted); }
.footnote-definition p { display: inline; }
"#
    };
}

/// Default stylesheet applied when the caller doesn't supply one.
pub const DEFAULT_STYLESHEET: &str = default_css!();

/// Same bytes, NUL-terminated, for handing out over FFI without allocating.
pub const DEFAULT_STYLESHEET_NUL: &str = concat!(default_css!(), "\0");

/// The width of a tab stop in a `<pre>` block. This is the CSS default for
/// `tab-size`.
const TAB_SIZE: usize = 8;

/// Replace each tab inside a `<pre>` element with the spaces that reach the next
/// tab stop.
///
/// Blitz does not lay out a tab in preformatted text. It draws the missing-glyph
/// box, and `tab-size` has no effect, so a Go or Makefile code block shows a
/// small hollow square where the indent should be. Spaces are laid out
/// correctly, so they are written here. The column counts characters, restarts
/// after a newline, and does not count tags. An entity such as `&amp;` counts as
/// one. Tabs outside `<pre>` are left alone, because they collapse to a space.
fn expand_tabs_in_pre(html: &str) -> Cow<'_, str> {
    if !html.contains('\t') || !html.contains("<pre") {
        return Cow::Borrowed(html);
    }

    let mut out = String::with_capacity(html.len() + 64);
    let mut rest = html;
    while let Some(start) = rest.find("<pre") {
        // `<pre` has to open a `pre` tag, and not a tag such as `<preview>`.
        let after = &rest[start + "<pre".len()..];
        if !(after.starts_with('>') || after.starts_with(|c: char| c.is_ascii_whitespace())) {
            out.push_str(&rest[..start + "<pre".len()]);
            rest = after;
            continue;
        }
        let Some(len) = rest[start..].find("</pre>") else {
            break;
        };
        let end = start + len + "</pre>".len();
        out.push_str(&rest[..start]);
        expand_tabs_in_block(&rest[start..end], &mut out);
        rest = &rest[end..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// Writes one `<pre>...</pre>` block to `out`, with its tabs expanded.
fn expand_tabs_in_block(block: &str, out: &mut String) {
    let bytes = block.as_bytes();
    let mut i = 0;
    let mut col = 0;
    // Every byte tested below is ASCII, and an ASCII byte never appears inside
    // a multi-byte UTF-8 sequence, so each slice is on a character boundary.
    while i < bytes.len() {
        match bytes[i] {
            b'<' => {
                let end = block[i..].find('>').map_or(block.len(), |n| i + n + 1);
                out.push_str(&block[i..end]);
                i = end;
            }
            b'&' => {
                let end = match block[i..].find(';') {
                    Some(n) if n <= 10 => i + n + 1,
                    _ => i + 1,
                };
                out.push_str(&block[i..end]);
                i = end;
                col += 1;
            }
            b'\t' => {
                let n = TAB_SIZE - col % TAB_SIZE;
                out.extend(std::iter::repeat_n(' ', n));
                col += n;
                i += 1;
            }
            b'\n' => {
                out.push('\n');
                col = 0;
                i += 1;
            }
            _ => {
                let ch = block[i..].chars().next().unwrap_or(' ');
                out.push(ch);
                col += 1;
                i += ch.len_utf8();
            }
        }
    }
}

fn parser_options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_SMART_PUNCTUATION);
    options.insert(Options::ENABLE_HEADING_ATTRIBUTES);
    options
}

/// Convert markdown into a complete HTML document, styled and ready to lay out.
///
/// `stylesheet` of `None` uses [`DEFAULT_STYLESHEET`]; `Some("")` produces an
/// unstyled document, which is occasionally what you want if the markdown
/// itself carries a `<style>` block.
pub fn to_html_document(markdown: &str, stylesheet: Option<&str>) -> String {
    let css = stylesheet.unwrap_or(DEFAULT_STYLESHEET);

    let mut body = String::with_capacity(markdown.len() * 3 / 2);
    html::push_html(&mut body, Parser::new_ext(markdown, parser_options()));
    let body = expand_tabs_in_pre(&body);

    let mut doc = String::with_capacity(body.len() + css.len() + 256);
    doc.push_str("<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n");
    if !css.is_empty() {
        doc.push_str("<style>\n");
        // Markdown can't produce a raw `</style>` through the parser, but it can
        // pass one through an inline HTML block, which would close the element
        // early and dump CSS into the body. Only the stylesheet goes here, so
        // guard that instead of trusting the caller.
        doc.push_str(&css.replace("</style", "<\\/style"));
        doc.push_str("\n</style>\n");
    }
    doc.push_str("</head>\n<body>\n");
    doc.push_str(&body);
    doc.push_str("\n</body>\n</html>\n");
    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_body_in_a_document() {
        let out = to_html_document("# Hi", None);
        assert!(out.starts_with("<!DOCTYPE html>"));
        assert!(out.contains("<h1>Hi</h1>"));
        assert!(out.contains("--fg"));
    }

    #[test]
    fn gfm_extensions_are_on() {
        let out = to_html_document("| a |\n| - |\n| 1 |\n\n~~x~~", None);
        assert!(out.contains("<table>"));
        assert!(out.contains("<del>"));
    }

    #[test]
    fn a_tab_in_a_code_block_reaches_the_next_tab_stop() {
        let out = to_html_document("```\na\tb\n\tc\n```", None);
        assert!(out.contains("a       b"), "7 spaces after a: {out}");
        assert!(out.contains("\n        c"), "8 spaces at the start: {out}");
        let body = out.split("<body>").nth(1).unwrap();
        assert!(!body.contains('\t'), "no tab is left in the body: {body}");
    }

    #[test]
    fn an_entity_counts_as_one_column_and_a_tag_as_none() {
        let out = to_html_document("```go\n&\tx\n```", None);
        assert!(out.contains("&amp;       x"), "&amp; is one column: {out}");
        let out = to_html_document("<pre><b>ab</b>\tx</pre>", None);
        assert!(
            out.contains("<b>ab</b>      x"),
            "a tag is no column: {out}"
        );
    }

    #[test]
    fn the_column_starts_again_after_a_newline() {
        let out = to_html_document("```\nabc\td\nabcdefg\th\n```", None);
        assert!(out.contains("abc     d"), "5 spaces: {out}");
        assert!(out.contains("abcdefg h"), "1 space: {out}");
    }

    #[test]
    fn a_tab_outside_pre_is_left_alone() {
        let out = to_html_document("a\tb", None);
        assert!(out.contains("a\tb"), "{out}");
        let out = to_html_document("<preview>\tx</preview>", None);
        assert!(out.contains("<preview>\tx"), "{out}");
    }

    #[test]
    fn an_ordered_list_item_has_padding_for_the_marker() {
        assert!(DEFAULT_STYLESHEET.contains("ol > li { padding-left"));
    }

    #[test]
    fn empty_stylesheet_omits_the_style_element() {
        let out = to_html_document("# Hi", Some(""));
        assert!(!out.contains("<style>"));
    }
}
