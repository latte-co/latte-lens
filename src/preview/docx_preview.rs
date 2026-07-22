use std::io::{Cursor, Read};

use anyhow::{Context, Result, bail};
use quick_xml::{Reader, events::Event};
use zip::ZipArchive;

use super::{
    PreviewContent, PreviewRequest,
    common::{BoundedPreview, ParseBudget},
};

const CONTENT_TYPES: &str = "[Content_Types].xml";
const DOCUMENT_XML: &str = "word/document.xml";
const CORE_PROPERTIES: &str = "docProps/core.xml";
const DOCX_CONTENT_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml";
const MACRO_CONTENT_TYPE: &str = "application/vnd.ms-word.document.macroEnabled.main+xml";

const MAX_ZIP_ENTRIES: usize = 4_096;
const MAX_ZIP_ENTRY_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ZIP_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_XML_DEPTH: usize = 128;
const MAX_XML_EVENTS: usize = 1_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DocxDetection {
    Docx,
    MacroEnabled,
    NotDocx,
}

pub(super) struct InspectedDocx<'a> {
    archive: ZipArchive<Cursor<&'a [u8]>>,
    detection: DocxDetection,
    input_len: usize,
}

impl InspectedDocx<'_> {
    pub(super) const fn detection(&self) -> DocxDetection {
        self.detection
    }
}

pub(super) fn inspect(bytes: &[u8]) -> Result<InspectedDocx<'_>> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).context("cannot read ZIP container")?;
    if archive.offset() != 0 {
        return Ok(InspectedDocx {
            archive,
            detection: DocxDetection::NotDocx,
            input_len: bytes.len(),
        });
    }
    let mut has_content_types = false;
    let mut has_document = false;
    for index in 0..archive.len() {
        let file = archive
            .by_index(index)
            .context("cannot inspect ZIP entry")?;
        has_content_types |= file.name() == CONTENT_TYPES;
        has_document |= file.name() == DOCUMENT_XML;
    }
    if !has_content_types || !has_document {
        return Ok(InspectedDocx {
            archive,
            detection: DocxDetection::NotDocx,
            input_len: bytes.len(),
        });
    }
    preflight_archive(&mut archive)?;
    let content_types = read_entry(&mut archive, CONTENT_TYPES)?;
    let detection = content_type_detection(&content_types)?;
    Ok(InspectedDocx {
        archive,
        detection,
        input_len: bytes.len(),
    })
}

pub(super) fn preview(
    mut inspected: InspectedDocx<'_>,
    request: &PreviewRequest<'_>,
) -> Result<PreviewContent> {
    let archive = &mut inspected.archive;
    if archive.offset() != 0 {
        bail!("DOCX container has prepended data and is rejected as a polyglot");
    }
    match inspected.detection {
        DocxDetection::Docx => {}
        DocxDetection::MacroEnabled => bail!("macro-enabled Word documents are not supported"),
        DocxDetection::NotDocx => bail!("ZIP container is not a standard DOCX document"),
    }

    let core_properties = read_optional_entry(archive, CORE_PROPERTIES)?;
    let document_xml = read_entry(archive, DOCUMENT_XML)?;
    let mut output = BoundedPreview::new(request.max_bytes, request.max_lines);
    output.push_line("Format: DOCX");
    output.push_line(format!("File size: {} bytes", inspected.input_len));
    if let Some(core_properties) = core_properties
        && let Err(error) = append_core_properties(&core_properties, &mut output)
    {
        output.push_line(format!(
            "Metadata unavailable: {}",
            super::common::sanitized_error(&error)
        ));
    }
    output.push_line("");
    append_document(&document_xml, &mut output)?;
    Ok(output.finish())
}

fn preflight_archive(archive: &mut ZipArchive<Cursor<&[u8]>>) -> Result<()> {
    if archive.len() > MAX_ZIP_ENTRIES {
        bail!(
            "DOCX contains {} ZIP entries; the preview limit is {MAX_ZIP_ENTRIES}",
            archive.len()
        );
    }
    let mut total = 0u64;
    for index in 0..archive.len() {
        let file = archive
            .by_index(index)
            .context("cannot inspect ZIP entry")?;
        if file.encrypted() {
            bail!("encrypted DOCX entries are not supported");
        }
        if file.size() > MAX_ZIP_ENTRY_BYTES {
            bail!(
                "DOCX entry {} is {} bytes; the per-entry limit is {MAX_ZIP_ENTRY_BYTES}",
                file.name(),
                file.size()
            );
        }
        total = total
            .checked_add(file.size())
            .ok_or_else(|| anyhow::anyhow!("DOCX uncompressed size overflow"))?;
        if total > MAX_ZIP_TOTAL_BYTES {
            bail!("DOCX expands to more than {MAX_ZIP_TOTAL_BYTES} bytes across all ZIP entries");
        }
    }
    Ok(())
}

fn read_entry(archive: &mut ZipArchive<Cursor<&[u8]>>, name: &str) -> Result<Vec<u8>> {
    let file = archive
        .by_name(name)
        .with_context(|| format!("DOCX is missing required part {name}"))?;
    if file.encrypted() {
        bail!("DOCX part {name} is encrypted");
    }
    if file.size() > MAX_ZIP_ENTRY_BYTES {
        bail!("DOCX part {name} exceeds the per-entry size limit");
    }
    let capacity =
        usize::try_from(file.size()).context("DOCX part is too large for this platform")?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_ZIP_ENTRY_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read DOCX part {name}"))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_ZIP_ENTRY_BYTES {
        bail!("DOCX part {name} exceeded its declared size limit while reading");
    }
    Ok(bytes)
}

fn read_optional_entry(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    name: &str,
) -> Result<Option<Vec<u8>>> {
    match archive.by_name(name) {
        Ok(file) => {
            if file.encrypted() || file.size() > MAX_ZIP_ENTRY_BYTES {
                return Ok(None);
            }
            let mut bytes = Vec::with_capacity(usize::try_from(file.size()).unwrap_or(0));
            file.take(MAX_ZIP_ENTRY_BYTES.saturating_add(1))
                .read_to_end(&mut bytes)
                .with_context(|| format!("cannot read optional DOCX part {name}"))?;
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_ZIP_ENTRY_BYTES {
                Ok(None)
            } else {
                Ok(Some(bytes))
            }
        }
        Err(zip::result::ZipError::FileNotFound) => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("cannot inspect optional DOCX part {name}"))
        }
    }
}

fn content_type_detection(xml: &[u8]) -> Result<DocxDetection> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().check_end_names = true;
    let mut budget = ParseBudget::new(MAX_XML_EVENTS);
    let mut depth = 0usize;
    let mut detection = DocxDetection::NotDocx;
    loop {
        budget.check()?;
        match reader.read_event()? {
            Event::Start(start) => {
                depth = checked_depth(depth)?;
                if local_name(start.name().as_ref()) == b"Override"
                    && attribute(&start, b"PartName")?.as_deref() == Some("/word/document.xml")
                    && let Some(content_type) = attribute(&start, b"ContentType")?
                {
                    if content_type == DOCX_CONTENT_TYPE && detection != DocxDetection::MacroEnabled
                    {
                        detection = DocxDetection::Docx;
                    }
                    if content_type == MACRO_CONTENT_TYPE || content_type.contains("macroEnabled") {
                        detection = DocxDetection::MacroEnabled;
                    }
                }
            }
            Event::Empty(start) => {
                if local_name(start.name().as_ref()) == b"Override"
                    && attribute(&start, b"PartName")?.as_deref() == Some("/word/document.xml")
                    && let Some(content_type) = attribute(&start, b"ContentType")?
                {
                    if content_type == DOCX_CONTENT_TYPE && detection != DocxDetection::MacroEnabled
                    {
                        detection = DocxDetection::Docx;
                    }
                    if content_type == MACRO_CONTENT_TYPE || content_type.contains("macroEnabled") {
                        detection = DocxDetection::MacroEnabled;
                    }
                }
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            Event::DocType(_) => bail!("DOCTYPE is forbidden in DOCX XML"),
            Event::Eof if depth == 0 => return Ok(detection),
            Event::Eof => bail!("DOCX content types XML ended before all elements were closed"),
            _ => {}
        }
    }
}

fn append_core_properties(xml: &[u8], output: &mut BoundedPreview) -> Result<()> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().check_end_names = true;
    let mut budget = ParseBudget::new(MAX_XML_EVENTS);
    let mut depth = 0usize;
    let mut field: Option<&'static str> = None;
    loop {
        budget.check()?;
        match reader.read_event()? {
            Event::Start(start) => {
                depth = checked_depth(depth)?;
                field = match local_name(start.name().as_ref()) {
                    b"title" => Some("Title"),
                    b"creator" => Some("Creator"),
                    b"subject" => Some("Subject"),
                    b"created" => Some("Created"),
                    b"modified" => Some("Modified"),
                    _ => field,
                };
            }
            Event::Text(text) => {
                if let Some(label) = field {
                    let value = decode_text(&text)?;
                    if !value.trim().is_empty() {
                        output.push_line(format!("{label}: {}", value.trim()));
                    }
                }
            }
            Event::End(end) => {
                if matches!(
                    local_name(end.name().as_ref()),
                    b"title" | b"creator" | b"subject" | b"created" | b"modified"
                ) {
                    field = None;
                }
                depth = depth.saturating_sub(1);
            }
            Event::DocType(_) => bail!("DOCTYPE is forbidden in DOCX core properties"),
            Event::Eof if depth == 0 => return Ok(()),
            Event::Eof => bail!("DOCX core properties ended before all elements were closed"),
            _ => {}
        }
    }
}

fn append_document(xml: &[u8], output: &mut BoundedPreview) -> Result<()> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().check_end_names = true;
    let mut budget = ParseBudget::new(MAX_XML_EVENTS);
    let mut depth = 0usize;
    let mut in_text = false;
    let mut in_paragraph = false;
    let mut in_table = false;
    let mut in_cell = false;
    let mut paragraph = String::new();
    let mut paragraph_style: Option<String> = None;
    let mut paragraph_is_list = false;
    let mut cell = Vec::<String>::new();
    let mut row = Vec::<String>::new();

    loop {
        budget.check()?;
        match reader.read_event()? {
            Event::Start(start) => {
                depth = checked_depth(depth)?;
                match local_name(start.name().as_ref()) {
                    b"p" => {
                        in_paragraph = true;
                        paragraph.clear();
                        paragraph_style = None;
                        paragraph_is_list = false;
                    }
                    b"t" => in_text = true,
                    b"numPr" if in_paragraph => paragraph_is_list = true,
                    b"tbl" => in_table = true,
                    b"tr" if in_table => row.clear(),
                    b"tc" if in_table => {
                        in_cell = true;
                        cell.clear();
                    }
                    b"pStyle" if in_paragraph => {
                        paragraph_style = attribute(&start, b"val")?;
                    }
                    _ => {}
                }
            }
            Event::Empty(empty) => match local_name(empty.name().as_ref()) {
                b"br" if in_paragraph => paragraph.push(' '),
                b"tab" if in_paragraph => paragraph.push_str("    "),
                b"pStyle" if in_paragraph => paragraph_style = attribute(&empty, b"val")?,
                b"numPr" if in_paragraph => paragraph_is_list = true,
                _ => {}
            },
            Event::Text(text) if in_text && in_paragraph => {
                paragraph.push_str(&decode_text(&text)?);
            }
            Event::End(end) => {
                match local_name(end.name().as_ref()) {
                    b"t" => in_text = false,
                    b"p" => {
                        let line = format_paragraph(
                            paragraph.trim(),
                            paragraph_style.as_deref(),
                            paragraph_is_list,
                        );
                        if !line.is_empty() {
                            if in_cell {
                                cell.push(line);
                            } else if !output.push_line(line) {
                                return Ok(());
                            }
                        }
                        in_paragraph = false;
                    }
                    b"tc" if in_table => {
                        row.push(cell.join(" / "));
                        in_cell = false;
                    }
                    b"tr" if in_table => {
                        if !output.push_line(row.join(" | ")) {
                            return Ok(());
                        }
                    }
                    b"tbl" => in_table = false,
                    _ => {}
                }
                depth = depth.saturating_sub(1);
            }
            Event::DocType(_) => bail!("DOCTYPE is forbidden in DOCX document XML"),
            Event::Eof if depth == 0 => return Ok(()),
            Event::Eof => bail!("DOCX document XML ended before all elements were closed"),
            _ => {}
        }
        if output.is_truncated() {
            return Ok(());
        }
    }
}

fn format_paragraph(text: &str, style: Option<&str>, is_list: bool) -> String {
    if text.is_empty() {
        return String::new();
    }
    if let Some(level) = style.and_then(heading_level) {
        return format!("{} {text}", "#".repeat(level));
    }
    if is_list {
        return format!("- {text}");
    }
    text.to_owned()
}

fn heading_level(style: &str) -> Option<usize> {
    let normalized = style.to_ascii_lowercase().replace(' ', "");
    let suffix = normalized.strip_prefix("heading")?;
    let level = suffix.parse::<usize>().ok()?;
    (1..=6).contains(&level).then_some(level)
}

fn checked_depth(depth: usize) -> Result<usize> {
    let next = depth
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("DOCX XML depth overflow"))?;
    if next > MAX_XML_DEPTH {
        bail!("DOCX XML exceeds the {MAX_XML_DEPTH} element depth limit");
    }
    Ok(next)
}

fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|byte| *byte == b':').next().unwrap_or(name)
}

fn attribute(start: &quick_xml::events::BytesStart<'_>, name: &[u8]) -> Result<Option<String>> {
    for attribute in start.attributes().with_checks(true) {
        let attribute = attribute?;
        if local_name(attribute.key.as_ref()) == name {
            let raw = String::from_utf8_lossy(attribute.value.as_ref());
            return Ok(Some(quick_xml::escape::unescape(&raw)?.into_owned()));
        }
    }
    Ok(None)
}

fn decode_text(text: &quick_xml::events::BytesText<'_>) -> Result<String> {
    let decoded = text.xml10_content()?;
    Ok(quick_xml::escape::unescape(&decoded)?.into_owned())
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use anyhow::Result;
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    use super::*;

    #[test]
    fn extracts_docx_headings_lists_tables_and_metadata() -> Result<()> {
        let bytes = docx_fixture(false, false)?;
        let inspected = inspect(&bytes)?;
        assert_eq!(inspected.detection(), DocxDetection::Docx);
        let request = PreviewRequest::new(
            std::path::Path::new("fixture.docx"),
            std::path::Path::new("fixture.docx"),
        )
        .with_limits(32_768, 256);
        let content = preview(inspected, &request)?;
        assert!(content.lines.contains(&"Title: Safe preview".to_owned()));
        assert!(content.lines.contains(&"# Heading".to_owned()));
        assert!(content.lines.contains(&"- Item".to_owned()));
        assert!(content.lines.contains(&"A | B".to_owned()));
        Ok(())
    }

    #[test]
    fn rejects_macro_content_and_doctype() -> Result<()> {
        let macro_bytes = docx_fixture(true, false)?;
        assert_eq!(
            inspect(&macro_bytes)?.detection(),
            DocxDetection::MacroEnabled
        );
        let doctype = docx_fixture(false, true)?;
        let request = PreviewRequest::new(
            std::path::Path::new("unsafe.docx"),
            std::path::Path::new("unsafe.docx"),
        );
        assert!(preview(inspect(&doctype)?, &request).is_err());
        Ok(())
    }

    #[test]
    fn rejects_prepended_polyglot_data() -> Result<()> {
        let mut bytes = b"#!/bin/sh\n".to_vec();
        bytes.extend(docx_fixture(false, false)?);
        assert_eq!(inspect(&bytes)?.detection(), DocxDetection::NotDocx);
        let request = PreviewRequest::new(
            std::path::Path::new("polyglot.docx"),
            std::path::Path::new("polyglot.docx"),
        );
        assert!(preview(inspect(&bytes)?, &request).is_err());
        Ok(())
    }

    #[test]
    fn malformed_optional_metadata_does_not_hide_document_text() -> Result<()> {
        let bytes = docx_fixture_with_core(false, false, b"<broken></different>")?;
        let request = PreviewRequest::new(
            std::path::Path::new("metadata.docx"),
            std::path::Path::new("metadata.docx"),
        );
        let content = preview(inspect(&bytes)?, &request)?;
        assert!(
            content
                .lines
                .iter()
                .any(|line| line.starts_with("Metadata unavailable:"))
        );
        assert!(content.lines.contains(&"# Heading".to_owned()));
        Ok(())
    }

    #[test]
    fn content_type_doctype_and_xml_depth_are_rejected() {
        let content_types = br#"<!DOCTYPE Types [<!ENTITY x "boom">]><Types><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;
        assert!(content_type_detection(content_types).is_err());
        assert!(checked_depth(MAX_XML_DEPTH).is_err());
    }

    fn docx_fixture(macro_enabled: bool, doctype: bool) -> Result<Vec<u8>> {
        docx_fixture_with_core(
            macro_enabled,
            doctype,
            br#"<?xml version="1.0"?><cp:coreProperties xmlns:cp="x" xmlns:dc="d"><dc:title>Safe preview</dc:title><dc:creator>Latte Lens</dc:creator></cp:coreProperties>"#,
        )
    }

    fn docx_fixture_with_core(
        macro_enabled: bool,
        doctype: bool,
        core_properties: &[u8],
    ) -> Result<Vec<u8>> {
        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let content_type = if macro_enabled {
            MACRO_CONTENT_TYPE
        } else {
            DOCX_CONTENT_TYPE
        };
        writer.start_file(CONTENT_TYPES, options)?;
        write!(
            writer,
            r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Override PartName="/word/document.xml" ContentType="{content_type}"/></Types>"#
        )?;
        writer.start_file(CORE_PROPERTIES, options)?;
        writer.write_all(core_properties)?;
        writer.start_file(DOCUMENT_XML, options)?;
        if doctype {
            writer.write_all(br#"<?xml version="1.0"?><!DOCTYPE document [<!ENTITY x "boom">]><w:document xmlns:w="w"><w:body><w:p><w:r><w:t>&x;</w:t></w:r></w:p></w:body></w:document>"#)?;
        } else {
            writer.write_all(br#"<?xml version="1.0"?><w:document xmlns:w="w"><w:body><w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Heading</w:t></w:r></w:p><w:p><w:pPr><w:numPr/></w:pPr><w:r><w:t>Item</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:t>A</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>B</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#)?;
        }
        Ok(writer.finish()?.into_inner())
    }
}
