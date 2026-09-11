//! Markdown support: GFM-flavoured parsing plus a default stylesheet, wrapped
//! into a standalone HTML document that Blitz can lay out.
//!
//! Kept deliberately small. The markdown is converted to HTML and then handed
//! to the ordinary `HtmlDocument` path, so there is no second rendering
//! pipeline to maintain — anything Blitz can style, markdown output inherits.

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
    fn empty_stylesheet_omits_the_style_element() {
        let out = to_html_document("# Hi", Some(""));
        assert!(!out.contains("<style>"));
    }
}
