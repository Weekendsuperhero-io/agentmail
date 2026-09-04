//! Content formatting utilities.
//!
//! HTML → Markdown conversion (via `html2md`), whitespace normalisation,
//! and context-window-safe truncation for IMAP message content.

use html2md;

/// Default maximum characters for content returned to callers.
pub const DEFAULT_CONTENT_MAX_CHARS: usize = 100_000;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Convert HTML to Markdown using `fast_html2md`, then clean up
/// tracking-URL noise common in marketing emails.
pub fn html_to_markdown(html: &str) -> String {
    let raw_md = html2md::rewrite_html(html, false);
    clean_markdown(&collapse_blank_lines(&raw_md))
}

/// Normalise plain text by collapsing excessive blank lines.
pub fn plain_to_markdown(value: &str) -> String {
    collapse_blank_lines(value)
}

/// The one inline style the rendered part carries.
///
/// Mail clients strip `<style>` blocks and never fetch external CSS, so
/// anything not inline is decoration only some readers see. A font stack, a
/// size and a line height are what a client's own "rich text" composer applies;
/// colours, widths and backgrounds are left alone so the reader's theme — and
/// dark mode in particular — still governs.
const EMAIL_BODY_STYLE: &str = "font-family:-apple-system,BlinkMacSystemFont,\
     'Segoe UI',Helvetica,Arial,sans-serif;font-size:14px;line-height:1.5";

/// URL schemes a rendered link or image may use.
///
/// An allowlist, not a denylist: the set of schemes that can execute is open
/// (`javascript:`, `data:`, `vbscript:`, whatever a client adds next), while the
/// set that belongs in mail is closed and short.
const SAFE_URL_SCHEMES: [&str; 4] = ["http", "https", "mailto", "tel"];

/// Whether a link or image destination is safe to make clickable.
///
/// A destination with NO scheme is relative and cannot execute, so it passes.
/// One with a scheme must name a member of [`SAFE_URL_SCHEMES`].
///
/// ASCII whitespace and control characters are stripped before the scheme is
/// read, because clients strip them too: `java\tscript:alert(1)` and
/// `java\nscript:alert(1)` are `javascript:` by the time anything acts on them,
/// and a checker that reads the raw string sees a scheme called `java`.
fn is_safe_url(dest: &str) -> bool {
    let collapsed: String = dest
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && !c.is_control())
        .collect();
    // A `:` after any of these is inside a path, query or fragment, not a
    // scheme — `foo/bar:baz` is relative.
    let scheme_end = collapsed.find([':', '/', '?', '#']);
    match scheme_end {
        Some(end) if collapsed.as_bytes()[end] == b':' => {
            let scheme = collapsed[..end].to_ascii_lowercase();
            SAFE_URL_SCHEMES.contains(&scheme.as_str())
        }
        // No scheme at all, or a delimiter that ends the scheme-shaped prefix
        // before any `:` — relative either way.
        _ => true,
    }
}

/// Render a Markdown draft body into the `text/html` half of a
/// `multipart/alternative` message.
///
/// Two things are refused, and they are different problems:
///
/// **Raw HTML is ESCAPED, never emitted.** A draft body is agent-authored text,
/// so treating `<...>` inside it as markup would let a tool call decide what
/// runs in a recipient's mail client. `pulldown_cmark` surfaces raw HTML as its
/// own events, and mapping those to TEXT is deliberate over dropping them: a
/// literal `<b>` reaches the reader as the characters that were written, rather
/// than silently disappearing. Because no markup passes through, there is no
/// HTML sanitiser to keep in step with — the passthrough is off at the source.
///
/// **Unsafe URL schemes lose their link.** `push_html` escapes a destination
/// for HTML but does not filter its scheme, so `[click](javascript:alert(1))`
/// renders as a working `<a href="javascript:...">` (verified, 2026-09-03).
/// Escaping raw HTML does nothing about this: the markup here is ours, and the
/// payload rides an attribute we generated. A link or image whose destination
/// fails the `is_safe_url` check is unwrapped — the TEXT survives, the
/// destination does not become clickable — and nothing is lost overall,
/// because the plain half of the `alternative` still carries the author's
/// Markdown verbatim.
///
/// Extensions are limited to tables and strikethrough — the GFM constructs that
/// mean something in correspondence. Smart punctuation is deliberately OFF: it
/// would rewrite quotes and dashes in the HTML part while the plain part kept
/// the originals, and the two halves of an `alternative` must say the same
/// thing.
pub fn markdown_to_email_html(markdown: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);

    // Markdown links and images do not nest, so at most one of each can be open
    // — a flag is enough to pair a suppressed Start with its End.
    let mut in_unsafe_link = false;
    let mut in_unsafe_image = false;
    let events = Parser::new_ext(markdown, options).filter_map(move |event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Some(Event::Text(raw)),
        Event::Start(Tag::Link { ref dest_url, .. }) if !is_safe_url(dest_url) => {
            in_unsafe_link = true;
            None
        }
        Event::End(TagEnd::Link) if in_unsafe_link => {
            in_unsafe_link = false;
            None
        }
        Event::Start(Tag::Image { ref dest_url, .. }) if !is_safe_url(dest_url) => {
            in_unsafe_image = true;
            None
        }
        Event::End(TagEnd::Image) if in_unsafe_image => {
            in_unsafe_image = false;
            None
        }
        other => Some(other),
    });

    let mut rendered = String::with_capacity(markdown.len() + 256);
    pulldown_cmark::html::push_html(&mut rendered, events);

    format!(
        "<!DOCTYPE html>\n<html>\n<head><meta charset=\"utf-8\"></head>\n\
         <body style=\"{EMAIL_BODY_STYLE}\">\n{rendered}</body>\n</html>\n"
    )
}

/// Truncate text to `max_chars` on a char boundary.
///
/// Returns `(truncated_text, was_truncated)`.
pub fn truncate_for_context(value: &str, max_chars: usize) -> (String, bool) {
    if max_chars == 0 {
        return (String::new(), !value.is_empty());
    }

    let mut byte_end = 0usize;
    for (count, ch) in value.chars().enumerate() {
        if count >= max_chars {
            return (
                format!(
                    "{}...(truncated, {} total)",
                    &value[..byte_end],
                    value.len()
                ),
                true,
            );
        }
        byte_end += ch.len_utf8();
    }

    (value.to_string(), false)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Collapse runs of blank lines so at most two consecutive newlines remain.
pub fn collapse_blank_lines(value: &str) -> String {
    let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::new();
    let mut blank_run = 0usize;
    for line in normalized.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 2 {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }
    out.trim().to_string()
}

/// Post-process markdown to remove noise common in marketing emails.
///
/// - Drops `![img](url)` images entirely (tracking pixels, layout images).
/// - Drops empty links `[](url)` and strips tracking URLs (> 150 chars)
///   from links, keeping the visible text.
/// - Strips table pipe characters `|` that result from layout-table HTML.
/// - Decodes leftover `&amp;` → `&`.
/// - Trims whitespace, collapses blank lines.
fn clean_markdown(value: &str) -> String {
    // Pass 1: Strip images ![alt](url)
    let no_images = strip_markdown_images(value);
    // Pass 2: Strip/simplify links with tracking URLs
    let no_tracking = strip_tracking_links(&no_images);

    // Line-level cleanup
    let cleaned: Vec<String> = no_tracking
        .lines()
        .map(|line| {
            let stripped = line.trim().trim_matches('|').trim();
            stripped.replace("&amp;", "&")
        })
        .collect();

    collapse_blank_lines(&cleaned.join("\n"))
}

/// Remove all markdown image references `![alt](url)`.
fn strip_markdown_images(value: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = value.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '!'
            && i + 1 < chars.len()
            && chars[i + 1] == '['
            && let Some(end) = skip_markdown_link(&chars, i + 1)
        {
            i = end;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Strip or simplify links whose URL is > 150 chars (tracking redirects).
fn strip_tracking_links(value: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = value.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '['
            && let Some((link_text, _url, url_len, end)) = parse_markdown_link(&chars, i)
            && (link_text.trim().is_empty() || url_len > 150)
        {
            let clean = link_text.trim();
            if !clean.is_empty() {
                out.push_str(clean);
            }
            i = end;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Parse a markdown link `[text](url)` starting at position `start` (the `[`).
/// Returns `(link_text, url, url_char_len, end_pos)`.
fn parse_markdown_link(chars: &[char], start: usize) -> Option<(String, String, usize, usize)> {
    let close_bracket = chars[start + 1..].iter().position(|&c| c == ']')?;
    let link_text: String = chars[start + 1..start + 1 + close_bracket].iter().collect();
    let after = start + 1 + close_bracket + 1;
    if after >= chars.len() || chars[after] != '(' {
        return None;
    }
    let close_paren = chars[after + 1..].iter().position(|&c| c == ')')?;
    let url: String = chars[after + 1..after + 1 + close_paren].iter().collect();
    let end = after + 1 + close_paren + 1;
    Some((link_text, url.clone(), url.len(), end))
}

/// Skip past a markdown link `[...](...)` starting at `start` (the `[`).
/// Returns the position after the closing `)`, or `None` if not a valid link.
fn skip_markdown_link(chars: &[char], start: usize) -> Option<usize> {
    let close_bracket = chars[start + 1..].iter().position(|&c| c == ']')?;
    let after = start + 1 + close_bracket + 1;
    if after >= chars.len() || chars[after] != '(' {
        return None;
    }
    let close_paren = chars[after + 1..].iter().position(|&c| c == ')')?;
    Some(after + 1 + close_paren + 1)
}

#[cfg(test)]
mod tests {
    use super::{is_safe_url, markdown_to_email_html};

    /// The scheme allowlist. `javascript:` is the obvious one; `data:` carries
    /// a whole document, and a client that honours it is running author-chosen
    /// HTML. Whitespace and control characters are stripped first because
    /// clients strip them too — a checker reading the raw string would see a
    /// scheme called `java` and wave `java\tscript:` through.
    #[test]
    fn only_mail_shaped_url_schemes_are_clickable() {
        for safe in [
            "https://example.org/a?b=1",
            "http://example.org",
            "HTTPS://EXAMPLE.ORG",
            "mailto:someone@example.org",
            "tel:+15551234",
            "/relative/path",
            "#fragment",
            "relative/path:with-colon",
            "",
        ] {
            assert!(is_safe_url(safe), "`{safe}` should be clickable");
        }
        for unsafe_url in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "java\tscript:alert(1)",
            "java\nscript:alert(1)",
            " javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "vbscript:msgbox(1)",
            "file:///etc/passwd",
        ] {
            assert!(
                !is_safe_url(unsafe_url),
                "`{unsafe_url}` must not be a link"
            );
        }
    }

    /// Two DIFFERENT injection routes, both closed. Raw HTML never reaches the
    /// output because passthrough is off at the parser; an unsafe destination
    /// never becomes an attribute because the link is unwrapped. Escaping raw
    /// HTML does nothing for the second — that markup is ours, and the payload
    /// rides an `href` we generated.
    #[test]
    fn neither_raw_html_nor_an_unsafe_scheme_survives_rendering() {
        let html = markdown_to_email_html(
            "[click](javascript:alert(1))\n\n\
             ![img](data:text/html,<script>alert(2)</script>)\n\n\
             <script>alert(3)</script>\n\n\
             <img src=x onerror=alert(4)>\n",
        );

        assert!(
            !html.contains("javascript:") && !html.contains("data:text/html"),
            "an unsafe destination must not reach an attribute: {html}"
        );
        // Assert on the TAG, not the substring: `onerror` also appears inside
        // the escaped text, where it is inert. What matters is that no `<` ever
        // opens an author-supplied element.
        assert!(
            !html.contains("<script") && !html.contains("<img src=x"),
            "author markup must never be emitted as markup: {html}"
        );
        assert!(
            html.contains("&lt;script&gt;alert(3)&lt;/script&gt;")
                && html.contains("&lt;img src=x onerror=alert(4)&gt;"),
            "…and must arrive escaped rather than deleted: {html}"
        );
        // Unwrapped, not deleted: the words survive, only the link does not.
        assert!(html.contains("click") && html.contains("img"), "{html}");
    }

    /// The filter must not cost ordinary mail its links and images.
    #[test]
    fn safe_links_and_images_render_untouched() {
        let html = markdown_to_email_html(
            "[ok](https://example.org/a?b=1&c=2) and [mail](mailto:a@b.co)\n\n\
             ![pic](https://example.org/p.png)\n\n\
             <https://autolink.example>\n",
        );
        for expected in [
            "<a href=\"https://example.org/a?b=1&amp;c=2\">ok</a>",
            "<a href=\"mailto:a@b.co\">mail</a>",
            "<img src=\"https://example.org/p.png\" alt=\"pic\" />",
            "<a href=\"https://autolink.example\">",
        ] {
            assert!(html.contains(expected), "missing {expected} in {html}");
        }
    }
}
