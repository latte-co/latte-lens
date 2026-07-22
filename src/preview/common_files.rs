use std::{
    collections::hash_map::RandomState,
    hash::BuildHasher,
    io::{Read, Seek, SeekFrom},
    sync::Mutex,
};

use anyhow::{Context, Result};

use super::{
    PreviewContent, PreviewProvider, PreviewRequest,
    common::{
        BoundedPreview, CacheKey, CommonFormat, MAX_BINARY_INPUT_BYTES, PROBE_BYTES, PreviewCache,
        ProbeFormat, prepend_notice, probe_format, sanitized_error,
    },
    docx_preview::{self, DocxDetection},
    image_preview, pdf_preview,
};

pub(super) struct CommonFilePreviewProvider {
    cache: Mutex<PreviewCache>,
    hash_state: RandomState,
}

impl Default for CommonFilePreviewProvider {
    fn default() -> Self {
        Self {
            cache: Mutex::new(PreviewCache::default()),
            hash_state: RandomState::new(),
        }
    }
}

impl PreviewProvider for CommonFilePreviewProvider {
    fn id(&self) -> &'static str {
        "common-file"
    }

    fn preview(&self, request: &PreviewRequest<'_>) -> Result<Option<PreviewContent>> {
        let claimed = CommonFormat::from_extension(request.display_path);
        let Some(mut file) = request.open_regular()? else {
            return Ok(None);
        };

        let mut probe = vec![0; PROBE_BYTES.min(usize::try_from(file.len()).unwrap_or(usize::MAX))];
        let probe_len = file
            .read(&mut probe)
            .with_context(|| format!("cannot probe {}", request.display_path.display()))?;
        probe.truncate(probe_len);
        let initial_probe = probe_format(&probe);
        if initial_probe == ProbeFormat::Unknown && claimed.is_none() {
            return Ok(None);
        }

        if file.len() > MAX_BINARY_INPUT_BYTES {
            return Ok(Some(blocked_preview(
                request,
                format!(
                    "Preview blocked: {} is {} bytes; binary previews are limited to {} bytes.",
                    request.display_path.display(),
                    file.len(),
                    MAX_BINARY_INPUT_BYTES
                ),
                true,
            )));
        }

        if request.terminal_image_size().is_none()
            && let ProbeFormat::Supported(format) = initial_probe
            && matches!(
                format,
                CommonFormat::Png | CommonFormat::Jpeg | CommonFormat::Gif | CommonFormat::WebP
            )
        {
            let key = self.cache_key(
                request,
                format,
                usize::try_from(file.len()).unwrap_or(usize::MAX),
                &probe,
            );
            if let Some(content) = self.cache.lock().expect("preview cache poisoned").get(&key) {
                return Ok(Some(with_mismatch_notice(
                    content, request, claimed, format,
                )));
            }
            file.seek(SeekFrom::Start(0))
                .with_context(|| format!("cannot rewind {}", request.display_path.display()))?;
            let content = match image_preview::metadata_preview(file, &probe, format, request) {
                Ok(content) => content,
                Err(error) => blocked_preview(
                    request,
                    format!(
                        "Unable to preview {} safely: {}",
                        format.label(),
                        sanitized_error(&error)
                    ),
                    false,
                ),
            };
            self.cache
                .lock()
                .expect("preview cache poisoned")
                .insert(key, content.clone());
            return Ok(Some(with_mismatch_notice(
                content, request, claimed, format,
            )));
        }

        file.seek(SeekFrom::Start(0))
            .with_context(|| format!("cannot rewind {}", request.display_path.display()))?;
        let initial_len = file.len();
        let mut bytes = Vec::with_capacity(
            usize::try_from(initial_len.min(MAX_BINARY_INPUT_BYTES)).unwrap_or(0),
        );
        file.take(MAX_BINARY_INPUT_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .with_context(|| format!("cannot read {}", request.display_path.display()))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_BINARY_INPUT_BYTES {
            return Ok(Some(blocked_preview(
                request,
                format!(
                    "Preview blocked: {} grew beyond the {} byte binary preview limit while it was being read.",
                    request.display_path.display(),
                    MAX_BINARY_INPUT_BYTES
                ),
                true,
            )));
        }

        let mut inspected_docx = None;
        let detected = match initial_probe {
            ProbeFormat::Supported(format) => Some(format),
            ProbeFormat::Zip => match docx_preview::inspect(&bytes) {
                Ok(inspected) => match inspected.detection() {
                    DocxDetection::Docx => {
                        inspected_docx = Some(inspected);
                        Some(CommonFormat::Docx)
                    }
                    DocxDetection::MacroEnabled => {
                        return Ok(Some(blocked_preview(
                            request,
                            "Preview blocked: macro-enabled Word documents are not supported.",
                            false,
                        )));
                    }
                    DocxDetection::NotDocx => None,
                },
                Err(error) if claimed.is_some() => {
                    return Ok(Some(blocked_preview(
                        request,
                        format!(
                            "Unable to inspect the claimed file safely: {}",
                            sanitized_error(&error)
                        ),
                        false,
                    )));
                }
                Err(_) => None,
            },
            ProbeFormat::Unknown => None,
        };

        let Some(detected) = detected else {
            return Ok(claimed.map(|claimed| {
                blocked_preview(
                    request,
                    format!(
                        "Format mismatch: extension claims {}, but the file content is not a supported {} file.",
                        claimed.label(),
                        claimed.label()
                    ),
                    false,
                )
            }));
        };

        if is_docm(request) {
            return Ok(Some(blocked_preview(
                request,
                "Preview blocked: .docm files are macro-capable and are not supported.",
                false,
            )));
        }

        let key = self.cache_key(request, detected, bytes.len(), &bytes);
        if let Some(content) = self.cache.lock().expect("preview cache poisoned").get(&key) {
            return Ok(Some(with_mismatch_notice(
                content, request, claimed, detected,
            )));
        }

        let parsed = match detected {
            CommonFormat::Png | CommonFormat::Jpeg | CommonFormat::Gif | CommonFormat::WebP => {
                image_preview::preview(&bytes, detected, request)
            }
            CommonFormat::Pdf => pdf_preview::preview(&bytes, request),
            CommonFormat::Docx => docx_preview::preview(
                inspected_docx.expect("DOCX detection retains its inspected archive"),
                request,
            ),
        };
        let content = match parsed {
            Ok(content) => content,
            Err(error) => blocked_preview(
                request,
                format!(
                    "Unable to preview {} safely: {}",
                    detected.label(),
                    sanitized_error(&error)
                ),
                false,
            ),
        };
        self.cache
            .lock()
            .expect("preview cache poisoned")
            .insert(key, content.clone());
        Ok(Some(with_mismatch_notice(
            content, request, claimed, detected,
        )))
    }
}

impl CommonFilePreviewProvider {
    fn digest(&self, bytes: &[u8]) -> u64 {
        self.hash_state.hash_one(bytes)
    }

    fn cache_key(
        &self,
        request: &PreviewRequest<'_>,
        format: CommonFormat,
        input_len: usize,
        digest_bytes: &[u8],
    ) -> CacheKey {
        CacheKey {
            path: request.absolute_path.to_path_buf(),
            format,
            input_len,
            digest: self.digest(digest_bytes),
            max_bytes: request.max_bytes,
            max_lines: request.max_lines,
            terminal_image_size: request.terminal_image_size(),
        }
    }
}

fn with_mismatch_notice(
    content: PreviewContent,
    request: &PreviewRequest<'_>,
    claimed: Option<CommonFormat>,
    detected: CommonFormat,
) -> PreviewContent {
    let Some(claimed) = claimed.filter(|claimed| *claimed != detected) else {
        return content;
    };
    prepend_notice(
        content,
        format!(
            "Extension mismatch: extension claims {}, detected {} content.",
            claimed.label(),
            detected.label()
        ),
        request.max_bytes,
        request.max_lines,
    )
}

fn blocked_preview(
    request: &PreviewRequest<'_>,
    message: impl AsRef<str>,
    truncated: bool,
) -> PreviewContent {
    let mut preview = BoundedPreview::new(request.max_bytes, request.max_lines);
    preview.push_line(message);
    if truncated {
        preview.mark_truncated();
    }
    preview.finish()
}

fn is_docm(request: &PreviewRequest<'_>) -> bool {
    request
        .display_path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("docm"))
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Cursor, path::Path};

    use anyhow::Result;
    use image::{DynamicImage, ImageFormat};
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn disguised_script_with_pdf_extension_is_not_rendered_as_text() -> Result<()> {
        let directory = tempdir()?;
        let path = directory.path().join("payload.pdf");
        fs::write(&path, "#!/bin/sh\necho exploited\n")?;
        let content = CommonFilePreviewProvider::default()
            .preview(&PreviewRequest::new(&path, Path::new("payload.pdf")))?
            .expect("claimed common formats return an explanatory preview");
        assert!(content.lines[0].contains("Format mismatch"));
        assert!(content.lines.iter().all(|line| !line.contains("exploited")));
        Ok(())
    }

    #[test]
    fn oversized_input_is_rejected_from_metadata_without_being_read() -> Result<()> {
        let directory = tempdir()?;
        let path = directory.path().join("oversized.pdf");
        fs::File::create(&path)?.set_len(MAX_BINARY_INPUT_BYTES + 1)?;
        let content = CommonFilePreviewProvider::default()
            .preview(&PreviewRequest::new(&path, Path::new("oversized.pdf")))?
            .expect("claimed common formats return an explanatory preview");
        assert!(content.lines[0].contains("Preview blocked"));
        assert!(content.truncated);
        Ok(())
    }

    #[test]
    fn metadata_only_image_previews_populate_the_shared_cache() -> Result<()> {
        let directory = tempdir()?;
        let path = directory.path().join("cached.png");
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::new_rgba8(2, 3).write_to(&mut bytes, ImageFormat::Png)?;
        fs::write(&path, bytes.into_inner())?;
        let provider = CommonFilePreviewProvider::default();
        let request = PreviewRequest::new(&path, Path::new("cached.png"));

        provider.preview(&request)?.expect("image preview");
        assert_eq!(provider.cache.lock().unwrap().len(), 1);
        provider.preview(&request)?.expect("cached image preview");
        assert_eq!(provider.cache.lock().unwrap().len(), 1);
        Ok(())
    }
}
