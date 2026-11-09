//! Deterministic presentation rendering and structural checks for thread records.
//!
//! The `.eml` files remain the lossless evidence. The PDF is a readable index
//! and chronology whose hash table points back to those exact source bytes.

use mail_parser::{MessageParser, MimeHeaders as _};
use printpdf::{
    Color, Mm, Op, ParsedFont, PdfDocument, PdfFontHandle, PdfPage, PdfParseOptions,
    PdfSaveOptions, Point, Pt, Rgb, TextItem,
};
use sha2::{Digest as _, Sha256};

use crate::{
    AgentmailError, AttachmentInfo, Result, ThreadRecordFile, ThreadRecordPreviewResponse,
};

const NOTO_SANS: &[u8] = include_bytes!("../assets/fonts/noto-sans/NotoSans-Variable.ttf");
const BODY_PRESENTATION_CHAR_LIMIT: usize = 100_000;
const PAGE_WIDTH_MM: f32 = 210.0;
const PAGE_HEIGHT_MM: f32 = 297.0;
const PAGE_LEFT_MM: f32 = 18.0;
const PAGE_TOP_MM: f32 = 278.0;
const PAGE_BOTTOM_MM: f32 = 18.0;

#[derive(Debug, Clone)]
pub(crate) struct RecordPresentationMessage {
    pub body: String,
    pub body_truncated: bool,
    pub attachments: Vec<AttachmentInfo>,
}

#[derive(Debug, Clone, Copy)]
enum LineStyle {
    Title,
    Heading,
    Subheading,
    Body,
    Mono,
    Muted,
}

impl LineStyle {
    fn size(self) -> f32 {
        match self {
            Self::Title => 24.0,
            Self::Heading => 16.0,
            Self::Subheading => 12.0,
            Self::Body => 9.5,
            Self::Mono => 8.0,
            Self::Muted => 8.5,
        }
    }

    fn line_height_mm(self) -> f32 {
        self.size() * 0.48
    }

    fn wrap_width(self) -> usize {
        match self {
            Self::Title => 42,
            Self::Heading => 60,
            Self::Subheading => 76,
            Self::Body => 96,
            Self::Mono => 108,
            Self::Muted => 104,
        }
    }

    fn color(self) -> Color {
        let (r, g, b) = match self {
            Self::Title => (0.08, 0.12, 0.2),
            Self::Heading => (0.12, 0.22, 0.38),
            Self::Subheading => (0.16, 0.31, 0.5),
            Self::Body => (0.12, 0.13, 0.16),
            Self::Mono => (0.18, 0.2, 0.24),
            Self::Muted => (0.38, 0.4, 0.45),
        };
        Color::Rgb(Rgb {
            r,
            g,
            b,
            icc_profile: None,
        })
    }
}

#[derive(Debug, Clone)]
struct PresentationLine {
    text: String,
    style: LineStyle,
    gap_before_mm: f32,
}

pub(crate) fn sha256_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn analyze_message(
    raw: &[u8],
    uid: u32,
    body_char_limit: usize,
) -> Result<RecordPresentationMessage> {
    let parsed = MessageParser::default().parse(raw).ok_or_else(|| {
        AgentmailError::Parse(format!("failed to parse saved RFC822 source for UID {uid}"))
    })?;
    let body = if let Some(html) = parsed.body_html(0) {
        crate::content::html_to_markdown(&html)
    } else if let Some(text) = parsed.body_text(0) {
        crate::content::plain_to_markdown(&text)
    } else {
        "[No readable text or HTML body was present.]".to_string()
    };
    let (body, body_truncated) = crate::content::truncate_for_context(
        &body,
        body_char_limit.clamp(1, BODY_PRESENTATION_CHAR_LIMIT),
    );
    let attachments = parsed
        .attachments()
        .map(|part| {
            let content_type = part
                .content_type()
                .map(|content_type| {
                    let mut value = content_type.c_type.to_string();
                    if let Some(subtype) = &content_type.c_subtype {
                        value.push('/');
                        value.push_str(subtype);
                    }
                    value
                })
                .unwrap_or_else(|| "application/octet-stream".to_string());
            AttachmentInfo {
                name: part.attachment_name().map(str::to_string),
                content_type,
                size: part.contents().len(),
                content_id: part.content_id().map(str::to_string),
            }
        })
        .collect();
    Ok(RecordPresentationMessage {
        body,
        body_truncated,
        attachments,
    })
}

pub(crate) fn render_thread_record_pdf(
    purpose: &str,
    generated_at: &str,
    preview: &ThreadRecordPreviewResponse,
    files: &[ThreadRecordFile],
    presentations: &[RecordPresentationMessage],
    limitations: &[String],
) -> Result<Vec<u8>> {
    if files.len() != preview.messages.len() || presentations.len() != files.len() {
        return Err(AgentmailError::Other(
            "thread record renderer received mismatched message collections".to_string(),
        ));
    }

    let mut lines = Vec::new();
    push_line(&mut lines, "AgentMail Thread Record", LineStyle::Title, 0.0);
    push_line(
        &mut lines,
        "Readable presentation backed by exact RFC822 source files",
        LineStyle::Subheading,
        2.0,
    );
    push_line(
        &mut lines,
        format!("Purpose: {purpose}"),
        LineStyle::Body,
        7.0,
    );
    push_line(
        &mut lines,
        format!("Generated: {generated_at}"),
        LineStyle::Muted,
        0.0,
    );
    push_line(
        &mut lines,
        format!("Account: {}", preview.account),
        LineStyle::Muted,
        0.0,
    );
    push_line(
        &mut lines,
        format!("Selection digest: {}", preview.selection_digest),
        LineStyle::Mono,
        0.0,
    );
    push_line(&mut lines, "Why this is a record", LineStyle::Heading, 9.0);
    push_line(
        &mut lines,
        "The bundle preserves each selected message as exact RFC822 bytes, identifies every source by mailbox + UIDVALIDITY + UID, hashes each source with SHA-256, includes contemporaneous DKIM results, and records the deterministic selection method. The PDF is a presentation copy; the .eml files and manifest are the integrity-bearing artifacts.",
        LineStyle::Body,
        1.0,
    );
    push_line(&mut lines, "Selection method", LineStyle::Heading, 7.0);
    push_line(&mut lines, &preview.rationale, LineStyle::Body, 1.0);
    push_line(
        &mut lines,
        format!(
            "Seed: {} / UIDVALIDITY {} / UID {}",
            preview.seed.mailbox, preview.seed.uid_validity, preview.seed.uid
        ),
        LineStyle::Mono,
        1.0,
    );
    push_line(&mut lines, "Limitations", LineStyle::Heading, 7.0);
    for limitation in limitations {
        push_line(&mut lines, format!("• {limitation}"), LineStyle::Muted, 0.5);
    }

    push_line(&mut lines, "Chronology", LineStyle::Heading, 10.0);
    for (index, ((message, file), presentation)) in preview
        .messages
        .iter()
        .zip(files)
        .zip(presentations)
        .enumerate()
    {
        push_line(
            &mut lines,
            format!(
                "{}. {}",
                index + 1,
                empty_fallback(&message.subject, "(no subject)")
            ),
            LineStyle::Heading,
            10.0,
        );
        push_line(
            &mut lines,
            format!(
                "Date: {}",
                message
                    .date
                    .map(|date| date.to_rfc3339())
                    .unwrap_or_else(|| "not present".to_string())
            ),
            LineStyle::Body,
            1.0,
        );
        push_line(
            &mut lines,
            format!("From: {}", empty_fallback(&message.from, "not present")),
            LineStyle::Body,
            0.0,
        );
        push_line(
            &mut lines,
            format!(
                "Storage identity: {} / UIDVALIDITY {} / UID {}",
                message.identity.mailbox, message.identity.uid_validity, message.identity.uid
            ),
            LineStyle::Mono,
            0.0,
        );
        push_line(
            &mut lines,
            format!(
                "Message-ID: {}",
                message.message_id.as_deref().unwrap_or("not present")
            ),
            LineStyle::Mono,
            0.0,
        );
        push_line(
            &mut lines,
            format!(
                "In-Reply-To: {}",
                message.in_reply_to.as_deref().unwrap_or("not present")
            ),
            LineStyle::Mono,
            0.0,
        );
        push_line(
            &mut lines,
            format!(
                "References: {}",
                if message.references.is_empty() {
                    "not present".to_string()
                } else {
                    message.references.join(" ")
                }
            ),
            LineStyle::Mono,
            0.0,
        );
        for basis in &message.selection_basis {
            push_line(
                &mut lines,
                format!("Selection basis: {basis}"),
                LineStyle::Muted,
                0.0,
            );
        }
        push_line(
            &mut lines,
            format!("Source: {} · {} bytes", file.filename, file.bytes),
            LineStyle::Mono,
            1.0,
        );
        push_line(
            &mut lines,
            format!("SHA-256: {}", file.sha256),
            LineStyle::Mono,
            0.0,
        );
        push_line(
            &mut lines,
            format!(
                "DKIM: {}{}",
                file.dkim.result,
                file.dkim
                    .domain
                    .as_deref()
                    .map(|domain| format!(" ({domain})"))
                    .unwrap_or_default()
            ),
            LineStyle::Muted,
            0.0,
        );
        push_line(
            &mut lines,
            "Attachment inventory",
            LineStyle::Subheading,
            5.0,
        );
        if presentation.attachments.is_empty() {
            push_line(&mut lines, "None", LineStyle::Muted, 0.0);
        } else {
            for attachment in &presentation.attachments {
                push_line(
                    &mut lines,
                    format!(
                        "• {} · {} · {} bytes",
                        attachment.name.as_deref().unwrap_or("unnamed"),
                        attachment.content_type,
                        attachment.size
                    ),
                    LineStyle::Body,
                    0.0,
                );
            }
        }
        push_line(&mut lines, "Readable body", LineStyle::Subheading, 5.0);
        push_line(&mut lines, &presentation.body, LineStyle::Body, 0.0);
        if presentation.body_truncated {
            push_line(
                &mut lines,
                "[Presentation body truncated; the complete body remains in the hashed .eml source.]",
                LineStyle::Muted,
                1.0,
            );
        }
    }

    push_line(&mut lines, "Integrity table", LineStyle::Heading, 10.0);
    for file in files {
        push_line(
            &mut lines,
            format!("{}  {}", file.sha256, file.filename),
            LineStyle::Mono,
            0.0,
        );
    }

    render_lines(lines)
}

pub(crate) fn verify_pdf(bytes: &[u8]) -> Result<usize> {
    let mut warnings = Vec::new();
    let parsed = PdfDocument::parse(
        bytes,
        &PdfParseOptions {
            fail_on_error: true,
        },
        &mut warnings,
    )
    .map_err(|error| AgentmailError::Other(format!("generated PDF did not reopen: {error}")))?;
    if parsed.pages.is_empty() {
        return Err(AgentmailError::Other(
            "generated PDF reopened with no pages".to_string(),
        ));
    }
    Ok(parsed.pages.len())
}

fn render_lines(lines: Vec<PresentationLine>) -> Result<Vec<u8>> {
    let mut font_warnings = Vec::new();
    let font = ParsedFont::from_bytes(NOTO_SANS, 0, &mut font_warnings).ok_or_else(|| {
        AgentmailError::Other("embedded Noto Sans font could not be parsed".to_string())
    })?;
    let mut document = PdfDocument::new("AgentMail Thread Record");
    let font_id = document.add_font(&font);
    let font_handle = PdfFontHandle::External(font_id);
    let mut pages = Vec::new();
    let mut ops = Vec::new();
    let mut y = PAGE_TOP_MM;
    let mut page_number = 1usize;

    for line in lines {
        let needed = line.gap_before_mm + line.style.line_height_mm();
        if y - needed < PAGE_BOTTOM_MM {
            finish_page(&mut ops, &font_handle, page_number);
            pages.push(PdfPage::new(Mm(PAGE_WIDTH_MM), Mm(PAGE_HEIGHT_MM), ops));
            ops = Vec::new();
            y = PAGE_TOP_MM;
            page_number += 1;
        }
        y -= line.gap_before_mm;
        ops.push(Op::StartTextSection);
        ops.push(Op::SetTextCursor {
            pos: Point::new(Mm(PAGE_LEFT_MM), Mm(y)),
        });
        ops.push(Op::SetFont {
            font: font_handle.clone(),
            size: Pt(line.style.size()),
        });
        ops.push(Op::SetFillColor {
            col: line.style.color(),
        });
        ops.push(Op::ShowText {
            items: vec![TextItem::Text(line.text)],
        });
        ops.push(Op::EndTextSection);
        y -= line.style.line_height_mm();
    }
    finish_page(&mut ops, &font_handle, page_number);
    pages.push(PdfPage::new(Mm(PAGE_WIDTH_MM), Mm(PAGE_HEIGHT_MM), ops));

    let mut save_warnings = Vec::new();
    let bytes = document
        .with_pages(pages)
        .save(&PdfSaveOptions::default(), &mut save_warnings);
    if bytes.is_empty() {
        return Err(AgentmailError::Other(
            "PDF renderer returned an empty document".to_string(),
        ));
    }
    Ok(bytes)
}

fn finish_page(ops: &mut Vec<Op>, font: &PdfFontHandle, page_number: usize) {
    ops.push(Op::StartTextSection);
    ops.push(Op::SetTextCursor {
        pos: Point::new(Mm(180.0), Mm(9.0)),
    });
    ops.push(Op::SetFont {
        font: font.clone(),
        size: Pt(8.0),
    });
    ops.push(Op::SetFillColor {
        col: LineStyle::Muted.color(),
    });
    ops.push(Op::ShowText {
        items: vec![TextItem::Text(format!("Page {page_number}"))],
    });
    ops.push(Op::EndTextSection);
}

fn push_line(
    output: &mut Vec<PresentationLine>,
    text: impl AsRef<str>,
    style: LineStyle,
    gap_before_mm: f32,
) {
    let mut first = true;
    for source_line in text.as_ref().replace('\r', "").split('\n') {
        let wrapped = wrap_text(source_line, style.wrap_width());
        if wrapped.is_empty() {
            output.push(PresentationLine {
                text: String::new(),
                style,
                gap_before_mm: if first { gap_before_mm } else { 0.0 },
            });
            first = false;
            continue;
        }
        for line in wrapped {
            output.push(PresentationLine {
                text: sanitize_pdf_text(&line),
                style,
                gap_before_mm: if first { gap_before_mm } else { 0.0 },
            });
            first = false;
        }
    }
}

fn wrap_text(value: &str, width: usize) -> Vec<String> {
    if value.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in value.split_whitespace() {
        if current.chars().count() + usize::from(!current.is_empty()) + word.chars().count()
            <= width
        {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
            continue;
        }
        if !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        let mut chunk = String::new();
        for character in word.chars() {
            chunk.push(character);
            if chunk.chars().count() == width {
                lines.push(std::mem::take(&mut chunk));
            }
        }
        current = chunk;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn sanitize_pdf_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character == '\t' {
                ' '
            } else if character.is_control() {
                '�'
            } else {
                character
            }
        })
        .collect()
}

fn empty_fallback<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.trim().is_empty() {
        fallback
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_long_unbroken_values_without_losing_characters() {
        let source = "a".repeat(23);
        let wrapped = wrap_text(&source, 10);
        assert_eq!(wrapped, vec!["a".repeat(10), "a".repeat(10), "a".repeat(3)]);
        assert_eq!(wrapped.concat(), source);
    }

    #[test]
    fn presentation_analysis_honors_the_per_record_body_budget() {
        let raw = b"From: sender@example.com\r\nTo: recipient@example.com\r\nSubject: bounded\r\nMessage-ID: <bounded@example.com>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nabcdefghij";
        let presentation = analyze_message(raw, 7, 4).expect("analyze message");
        assert!(presentation.body.starts_with("abcd"));
        assert!(presentation.body_truncated);
    }

    #[test]
    fn embedded_font_builds_a_pdf_that_reopens() {
        let pdf = render_lines(vec![PresentationLine {
            text: "Evidence — こんにちは".to_string(),
            style: LineStyle::Body,
            gap_before_mm: 0.0,
        }])
        .expect("render PDF");
        assert!(pdf.starts_with(b"%PDF-"));
        assert_eq!(verify_pdf(&pdf).expect("parse PDF"), 1);
    }
}
