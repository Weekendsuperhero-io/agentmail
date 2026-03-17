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
