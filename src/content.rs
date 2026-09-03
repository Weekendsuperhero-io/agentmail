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

/// Render a Markdown draft body into the `text/html` half of a
/// `multipart/alternative` message.
///
/// Raw HTML in the source is ESCAPED, never emitted. A draft body is
/// agent-authored text, so treating `<...>` inside it as markup would let a
/// tool call decide what runs in a recipient's mail client. `pulldown_cmark`
/// surfaces raw HTML as its own events, and mapping those to TEXT is
/// deliberate over dropping them: a literal `<b>` reaches the reader as the
/// characters that were written, rather than silently disappearing. There is
/// no sanitiser to keep in step with, because no markup ever passes through.
///
/// Extensions are limited to tables and strikethrough — the GFM constructs that
/// mean something in correspondence. Smart punctuation is deliberately OFF: it
/// would rewrite quotes and dashes in the HTML part while the plain part kept
/// the originals, and the two halves of an `alternative` must say the same
/// thing.
pub fn markdown_to_email_html(markdown: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser};

    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);

    let events = Parser::new_ext(markdown, options).map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
        other => other,
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
