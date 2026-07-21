use std::{
    collections::VecDeque,
    hash::Hash,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Result, bail};

use super::PreviewContent;

pub(super) const MAX_BINARY_INPUT_BYTES: u64 = 32 * 1024 * 1024;
pub(super) const PROBE_BYTES: usize = 1024;
pub(super) const PARSE_DEADLINE: Duration = Duration::from_secs(5);
pub(super) const MAX_ERROR_BYTES: usize = 512;

const CACHE_MAX_ENTRIES: usize = 8;
const CACHE_MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum CommonFormat {
    Png,
    Jpeg,
    Gif,
    WebP,
    Pdf,
    Docx,
}

impl CommonFormat {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Gif => "GIF",
            Self::WebP => "WebP",
            Self::Pdf => "PDF",
            Self::Docx => "DOCX",
        }
    }

    pub(super) fn from_extension(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        match extension.as_str() {
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "gif" => Some(Self::Gif),
            "webp" => Some(Self::WebP),
            "pdf" => Some(Self::Pdf),
            "docx" | "docm" => Some(Self::Docx),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ProbeFormat {
    Supported(CommonFormat),
    Zip,
    Unknown,
}

pub(super) fn probe_format(bytes: &[u8]) -> ProbeFormat {
    if bytes.starts_with(b"%PDF-") {
        return ProbeFormat::Supported(CommonFormat::Pdf);
    }
    if matches!(
        bytes.get(..4),
        Some(b"PK\x03\x04" | b"PK\x05\x06" | b"PK\x07\x08")
    ) {
        return ProbeFormat::Zip;
    }
    match image::guess_format(bytes) {
        Ok(image::ImageFormat::Png) => ProbeFormat::Supported(CommonFormat::Png),
        Ok(image::ImageFormat::Jpeg) => ProbeFormat::Supported(CommonFormat::Jpeg),
        Ok(image::ImageFormat::Gif) => ProbeFormat::Supported(CommonFormat::Gif),
        Ok(image::ImageFormat::WebP) => ProbeFormat::Supported(CommonFormat::WebP),
        _ => ProbeFormat::Unknown,
    }
}

pub(super) struct BoundedPreview {
    lines: Vec<String>,
    used_bytes: usize,
    max_bytes: usize,
    max_lines: usize,
    truncated: bool,
}

impl BoundedPreview {
    pub(super) fn new(max_bytes: usize, max_lines: usize) -> Self {
        Self {
            lines: Vec::new(),
            used_bytes: 0,
            max_bytes,
            max_lines,
            truncated: false,
        }
    }

    pub(super) fn push_line(&mut self, line: impl AsRef<str>) -> bool {
        if self.lines.len() >= self.max_lines || self.used_bytes >= self.max_bytes {
            self.truncated = true;
            return false;
        }
        let line = sanitize_terminal_text(line.as_ref());
        let remaining = self.max_bytes.saturating_sub(self.used_bytes);
        let (line, was_truncated) = truncate_utf8(line, remaining);
        self.used_bytes = self.used_bytes.saturating_add(line.len());
        self.lines.push(line);
        self.truncated |= was_truncated;
        !was_truncated
    }

    pub(super) fn push_text(&mut self, text: &str) -> bool {
        let mut complete = true;
        for line in text.lines() {
            if !self.push_line(line) {
                complete = false;
                break;
            }
        }
        complete
    }

    pub(super) fn mark_truncated(&mut self) {
        self.truncated = true;
    }

    pub(super) fn force_notice(&mut self, notice: impl AsRef<str>) {
        let notice = sanitize_terminal_text(notice.as_ref());
        let (notice, _) = truncate_utf8(notice, self.max_bytes);
        while (!self.lines.is_empty())
            && (self.lines.len() >= self.max_lines
                || self.used_bytes.saturating_add(notice.len()) > self.max_bytes)
        {
            if let Some(removed) = self.lines.pop() {
                self.used_bytes = self.used_bytes.saturating_sub(removed.len());
            }
        }
        if self.lines.len() < self.max_lines
            && self.used_bytes.saturating_add(notice.len()) <= self.max_bytes
        {
            self.used_bytes = self.used_bytes.saturating_add(notice.len());
            self.lines.push(notice);
        }
        self.truncated = true;
    }

    pub(super) fn is_truncated(&self) -> bool {
        self.truncated
    }

    pub(super) fn finish(self) -> PreviewContent {
        PreviewContent::new(self.lines).with_truncated(self.truncated)
    }
}

pub(super) fn sanitize_terminal_text(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for character in input.chars() {
        if character == '\t' {
            output.push_str("    ");
        } else if is_terminal_control(character) {
            use std::fmt::Write as _;
            let _ = write!(output, "<U+{:04X}>", u32::from(character));
        } else {
            output.push(character);
        }
    }
    output
}

pub(super) fn sanitized_error(error: &dyn std::fmt::Display) -> String {
    let single_line = error.to_string().replace(['\r', '\n'], " ");
    let sanitized = sanitize_terminal_text(&single_line);
    truncate_utf8(sanitized, MAX_ERROR_BYTES).0
}

pub(super) fn prepend_notice(
    content: PreviewContent,
    notice: impl AsRef<str>,
    max_bytes: usize,
    max_lines: usize,
) -> PreviewContent {
    let kind = content.kind;
    let show_line_numbers = content.show_line_numbers;
    let original_highlights = content.highlights;
    let mut preview = BoundedPreview::new(max_bytes, max_lines);
    preview.push_line(notice);
    for line in content.lines {
        if !preview.push_line(line) {
            break;
        }
    }
    if content.truncated {
        preview.mark_truncated();
    }
    let mut result = preview
        .finish()
        .with_kind(kind)
        .with_line_numbers(show_line_numbers);
    if !result.lines.is_empty() {
        let kept_content_lines = result.lines.len().saturating_sub(1);
        let mut highlights = Vec::with_capacity(result.lines.len());
        highlights.push(Vec::new());
        highlights.extend(original_highlights.into_iter().take(kept_content_lines));
        result = result.with_highlights(highlights);
    }
    result
}

fn is_terminal_control(character: char) -> bool {
    matches!(
        character,
        '\u{0000}'..='\u{001f}'
            | '\u{007f}'..='\u{009f}'
            | '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value, false);
    }
    let mut end = max_bytes.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    (value, true)
}

pub(super) struct ParseBudget {
    deadline: Instant,
    events: usize,
    max_events: usize,
}

impl ParseBudget {
    pub(super) fn new(max_events: usize) -> Self {
        Self {
            deadline: Instant::now() + PARSE_DEADLINE,
            events: 0,
            max_events,
        }
    }

    pub(super) fn check(&mut self) -> Result<()> {
        self.events = self.events.saturating_add(1);
        if self.events > self.max_events {
            bail!("parser event budget exceeded");
        }
        if Instant::now() > self.deadline {
            bail!("preview parsing exceeded the 5 second cooperative deadline");
        }
        Ok(())
    }

    pub(super) fn check_stage(&self) -> Result<()> {
        if Instant::now() > self.deadline {
            bail!("preview parsing exceeded the 5 second cooperative deadline");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct CacheKey {
    pub(super) path: PathBuf,
    pub(super) format: CommonFormat,
    pub(super) input_len: usize,
    pub(super) digest: u64,
    pub(super) max_bytes: usize,
    pub(super) max_lines: usize,
    pub(super) terminal_image_size: Option<super::TerminalImageSize>,
}

#[derive(Default)]
pub(super) struct PreviewCache {
    entries: VecDeque<CacheEntry>,
    weight: usize,
}

struct CacheEntry {
    key: CacheKey,
    content: PreviewContent,
    weight: usize,
}

impl PreviewCache {
    pub(super) fn get(&mut self, key: &CacheKey) -> Option<PreviewContent> {
        let index = self.entries.iter().position(|entry| &entry.key == key)?;
        let entry = self.entries.remove(index)?;
        let content = entry.content.clone();
        self.entries.push_front(entry);
        Some(content)
    }

    pub(super) fn insert(&mut self, key: CacheKey, content: PreviewContent) {
        let weight = content
            .lines
            .iter()
            .map(String::len)
            .sum::<usize>()
            .saturating_add(
                content
                    .highlights
                    .iter()
                    .map(|line| line.len().saturating_mul(std::mem::size_of::<usize>() * 3))
                    .sum::<usize>(),
            );
        if weight > CACHE_MAX_BYTES {
            return;
        }
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key)
            && let Some(old) = self.entries.remove(index)
        {
            self.weight = self.weight.saturating_sub(old.weight);
        }
        self.entries.push_front(CacheEntry {
            key,
            content,
            weight,
        });
        self.weight = self.weight.saturating_add(weight);
        while self.entries.len() > CACHE_MAX_ENTRIES || self.weight > CACHE_MAX_BYTES {
            if let Some(removed) = self.entries.pop_back() {
                self.weight = self.weight.saturating_sub(removed.weight);
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_terminal_and_bidi_controls() {
        let sanitized = sanitize_terminal_text("safe\x1b]52;secret\x07\u{202e}tail");
        assert!(!sanitized.contains('\x1b'));
        assert!(!sanitized.contains('\x07'));
        assert!(!sanitized.contains('\u{202e}'));
        assert!(sanitized.contains("<U+001B>"));
        assert!(sanitized.contains("<U+202E>"));
    }

    #[test]
    fn bounded_preview_reserves_a_forced_truncation_notice() {
        let mut preview = BoundedPreview::new(18, 2);
        assert!(preview.push_line("first"));
        assert!(preview.push_line("second"));
        preview.force_notice("Parsed 1 / 9");
        let content = preview.finish();
        assert_eq!(content.lines, ["first", "Parsed 1 / 9"]);
        assert!(content.truncated);
    }

    #[test]
    fn cache_is_lru_and_separates_limits() {
        let mut cache = PreviewCache::default();
        let key = CacheKey {
            path: PathBuf::from("file.pdf"),
            format: CommonFormat::Pdf,
            input_len: 3,
            digest: 7,
            max_bytes: 10,
            max_lines: 2,
            terminal_image_size: None,
        };
        cache.insert(key.clone(), PreviewContent::new(vec!["cached".to_owned()]));
        assert_eq!(cache.get(&key).unwrap().lines, ["cached"]);
        let mut other = key.clone();
        other.max_lines = 3;
        assert!(cache.get(&other).is_none());
        let mut terminal = key.clone();
        terminal.terminal_image_size = Some(crate::preview::TerminalImageSize {
            columns: 80,
            rows: 24,
        });
        assert!(cache.get(&terminal).is_none());
    }
}
