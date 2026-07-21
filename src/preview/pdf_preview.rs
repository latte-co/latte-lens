use anyhow::{Context, Result, bail};
use lopdf::{Dictionary, Document, LoadOptions, Object, decode_text_string};

use super::{
    PreviewContent, PreviewRequest,
    common::{BoundedPreview, ParseBudget, sanitized_error},
};

const MAX_PDF_PAGES: usize = 10_000;
const MAX_PDF_OBJECTS: usize = 200_000;
const MAX_PDF_STREAM_BYTES: usize = 16 * 1024 * 1024;

pub(super) fn preview(bytes: &[u8], request: &PreviewRequest<'_>) -> Result<PreviewContent> {
    let mut budget = ParseBudget::new(MAX_PDF_OBJECTS.saturating_add(MAX_PDF_PAGES));
    let document = Document::load_mem_with_options(
        bytes,
        LoadOptions {
            max_decompressed_size: Some(MAX_PDF_STREAM_BYTES),
            ..LoadOptions::default()
        },
    )
    .context("cannot parse PDF structure")?;
    budget.check_stage()?;
    if document.is_encrypted() {
        bail!("encrypted PDFs are not supported");
    }
    let pages = document.get_pages();
    validate_structure_counts(document.objects.len(), pages.len())?;

    let mut output = BoundedPreview::new(request.max_bytes, request.max_lines);
    output.push_line("Format: PDF");
    output.push_line(format!("Pages: {}", pages.len()));
    append_metadata(&document, &mut output);
    if has_active_content(&document) {
        output.push_line("Security: active content and external actions are ignored");
    }
    output.push_line("");

    let mut parsed_pages = 0usize;
    let mut text_pages = 0usize;
    for page_number in pages.keys().copied() {
        if let Err(error) = budget.check() {
            output.force_notice(format!(
                "Parsed {parsed_pages} / {} pages; {}",
                pages.len(),
                sanitized_error(&error)
            ));
            break;
        }
        if !output.push_line(format!("Page {page_number} / {}", pages.len())) {
            output.force_notice(format!(
                "Parsed {parsed_pages} / {} pages; output budget exceeded",
                pages.len()
            ));
            break;
        }
        match document.extract_text_with_limit(&[page_number], MAX_PDF_STREAM_BYTES) {
            Ok(text) => {
                let text = text.trim_matches(['\r', '\n', '\u{000c}']);
                if text.trim().is_empty() {
                    output.push_line("[No extractable text on this page]");
                } else {
                    text_pages = text_pages.saturating_add(1);
                    if !output.push_text(text) {
                        parsed_pages = parsed_pages.saturating_add(1);
                        output.force_notice(format!(
                            "Parsed {parsed_pages} / {} pages; output budget exceeded",
                            pages.len()
                        ));
                        break;
                    }
                }
            }
            Err(error) => {
                output.push_line(format!(
                    "[Unable to extract this page safely: {}]",
                    sanitized_error(&error)
                ));
            }
        }
        parsed_pages = parsed_pages.saturating_add(1);
        if parsed_pages < pages.len() && !output.push_line("") {
            output.force_notice(format!(
                "Parsed {parsed_pages} / {} pages; output budget exceeded",
                pages.len()
            ));
            break;
        }
    }

    if parsed_pages == pages.len() && !pages.is_empty() && text_pages == 0 {
        output.push_line("");
        output.push_line(
            "No pages contained extractable text. This may be a scanned PDF; OCR is not available.",
        );
    }
    Ok(output.finish())
}

fn validate_structure_counts(objects: usize, pages: usize) -> Result<()> {
    if objects > MAX_PDF_OBJECTS {
        bail!("PDF contains {objects} objects; the preview limit is {MAX_PDF_OBJECTS}");
    }
    if pages > MAX_PDF_PAGES {
        bail!("PDF contains {pages} pages; the safety ceiling is {MAX_PDF_PAGES}");
    }
    Ok(())
}

fn append_metadata(document: &Document, output: &mut BoundedPreview) {
    let Ok(info) = document.trailer.get(b"Info") else {
        return;
    };
    let Ok((_, info)) = document.dereference(info) else {
        return;
    };
    let Ok(info) = info.as_dict() else {
        return;
    };
    for (key, label) in [
        (b"Title".as_slice(), "Title"),
        (b"Author".as_slice(), "Author"),
        (b"Subject".as_slice(), "Subject"),
        (b"Keywords".as_slice(), "Keywords"),
    ] {
        if let Ok(value) = info.get(key)
            && let Ok(value) = decode_text_string(value)
            && !value.trim().is_empty()
        {
            output.push_line(format!("{label}: {}", value.trim()));
        }
    }
}

fn has_active_content(document: &Document) -> bool {
    document.objects.values().any(|object| match object {
        Object::Dictionary(dictionary) => dictionary_has_active_content(dictionary),
        Object::Stream(stream) => dictionary_has_active_content(&stream.dict),
        _ => false,
    })
}

fn dictionary_has_active_content(dictionary: &Dictionary) -> bool {
    if [
        b"JS".as_slice(),
        b"JavaScript",
        b"OpenAction",
        b"AA",
        b"EmbeddedFiles",
    ]
    .iter()
    .any(|key| dictionary.has(key))
    {
        return true;
    }
    dictionary
        .get(b"S")
        .and_then(Object::as_name)
        .is_ok_and(|name| matches!(name, b"JavaScript" | b"Launch" | b"URI" | b"SubmitForm"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use anyhow::Result;
    use lopdf::{
        Document, Object, Stream,
        content::{Content, Operation},
        dictionary,
    };

    use super::*;

    #[test]
    fn extracts_every_pdf_page_with_boundaries_and_metadata() -> Result<()> {
        let bytes = pdf_fixture(&[Some("first page"), Some("second page")])?;
        let request = PreviewRequest::new(Path::new("fixture.pdf"), Path::new("fixture.pdf"))
            .with_limits(32_768, 256);
        let content = preview(&bytes, &request)?;
        assert!(content.lines.contains(&"Pages: 2".to_owned()));
        assert!(content.lines.contains(&"Title: Preview fixture".to_owned()));
        assert!(content.lines.contains(&"Page 1 / 2".to_owned()));
        assert!(content.lines.iter().any(|line| line.contains("first page")));
        assert!(content.lines.contains(&"Page 2 / 2".to_owned()));
        assert!(
            content
                .lines
                .iter()
                .any(|line| line.contains("second page"))
        );
        assert!(!content.truncated);
        Ok(())
    }

    #[test]
    fn distinguishes_an_all_image_pdf_from_a_single_blank_page() -> Result<()> {
        let bytes = pdf_fixture(&[None])?;
        let request = PreviewRequest::new(Path::new("scan.pdf"), Path::new("scan.pdf"))
            .with_limits(32_768, 256);
        let content = preview(&bytes, &request)?;
        assert!(
            content
                .lines
                .contains(&"[No extractable text on this page]".to_owned())
        );
        assert!(
            content
                .lines
                .iter()
                .any(|line| line.contains("scanned PDF"))
        );
        Ok(())
    }

    #[test]
    fn output_budget_reports_exact_partial_page_progress() -> Result<()> {
        let bytes = pdf_fixture(&[Some("first page"), Some("second page")])?;
        let request = PreviewRequest::new(Path::new("fixture.pdf"), Path::new("fixture.pdf"))
            .with_limits(32_768, 8);
        let content = preview(&bytes, &request)?;
        assert!(content.truncated);
        assert!(
            content
                .lines
                .iter()
                .any(|line| line.contains("Parsed 1 / 2 pages"))
        );
        Ok(())
    }

    #[test]
    fn active_pdf_content_is_reported_but_never_executed() -> Result<()> {
        let bytes = pdf_fixture(&[Some("safe text")])?;
        let mut document = Document::load_mem(&bytes)?;
        document.add_object(dictionary! {
            "S" => "JavaScript",
            "JS" => Object::string_literal("app.launchURL('https://invalid.example')"),
        });
        let mut active_bytes = Vec::new();
        document.save_to(&mut active_bytes)?;
        let request = PreviewRequest::new(Path::new("active.pdf"), Path::new("active.pdf"));
        let content = preview(&active_bytes, &request)?;
        assert!(
            content
                .lines
                .iter()
                .any(|line| line.contains("active content") && line.contains("ignored"))
        );
        assert!(content.lines.iter().any(|line| line.contains("safe text")));
        Ok(())
    }

    #[test]
    fn page_and_object_safety_ceilings_are_inclusive() {
        assert!(validate_structure_counts(MAX_PDF_OBJECTS, MAX_PDF_PAGES).is_ok());
        assert!(validate_structure_counts(MAX_PDF_OBJECTS + 1, 1).is_err());
        assert!(validate_structure_counts(1, MAX_PDF_PAGES + 1).is_err());
    }

    fn pdf_fixture(pages: &[Option<&str>]) -> Result<Vec<u8>> {
        let mut document = Document::with_version("1.5");
        let pages_id = document.new_object_id();
        let font_id = document.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let resources_id = document.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let mut page_ids = Vec::new();
        for text in pages {
            let operations = text.map_or_else(Vec::new, |text| {
                vec![
                    Operation::new("BT", vec![]),
                    Operation::new("Tf", vec![Object::Name(b"F1".to_vec()), 12.into()]),
                    Operation::new("Td", vec![48.into(), 700.into()]),
                    Operation::new("Tj", vec![Object::string_literal(text)]),
                    Operation::new("ET", vec![]),
                ]
            });
            let content = Content { operations }.encode()?;
            let content_id = document.add_object(Stream::new(dictionary! {}, content));
            let page_id = document.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "Contents" => content_id,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            });
            page_ids.push(page_id);
        }
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => page_ids.iter().copied().map(Object::Reference).collect::<Vec<_>>(),
                "Count" => i64::try_from(page_ids.len()).unwrap_or(i64::MAX),
            }),
        );
        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        let info_id = document.add_object(dictionary! {
            "Title" => Object::string_literal("Preview fixture"),
            "Author" => Object::string_literal("Latte Lens"),
        });
        document.trailer.set("Root", catalog_id);
        document.trailer.set("Info", info_id);
        let mut bytes = Vec::new();
        document.save_to(&mut bytes)?;
        Ok(bytes)
    }
}
