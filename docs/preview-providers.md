# Preview provider extensions

Latte Lens separates file selection and rendering from format extraction.
`PreviewRegistry` asks registered `PreviewProvider` implementations for content,
then the existing content pane handles scrolling, titles, and fallback messages.

## Contract

A provider:

1. Returns `Ok(None)` when it does not support a file.
2. Returns `PreviewContent` when it can render the file as terminal text.
3. Respects `request.max_bytes` and `request.max_lines`.
4. Sets `truncated` when output was capped.
5. Optionally attaches semantic `HighlightSpan` byte ranges with
   `PreviewContent::with_highlights`; the outer vector must match `lines`.
6. Never modifies the selected file or its repository.
7. Uses `request.open_regular()` for file bytes instead of reopening
   `request.absolute_path` directly.

Providers are queried in reverse registration order. Register a specialized
provider after the built-ins so it gets first chance to handle its format.
The registry inspects every component below the selected workspace with
non-following metadata and never dispatches a symlink, FIFO, socket, device,
directory, or Windows reparse point to a provider.

`PreviewRequest::open_regular` repeats that inspection, opens the final
component with no-follow semantics, verifies the opened handle is the same
regular file, and checks its canonical location before returning a readable,
seekable `PreviewFile`. On Unix it also uses non-blocking open so a racing FIFO
cannot stall the worker. On Windows it opens the reparse point itself rather
than its target.

There is one deliberate compatibility boundary: an optional third-party
provider may ignore `open_regular()` and pass `absolute_path` to a library that
reopens the pathname. Static links and special files are still rejected before
dispatch, but Latte Lens cannot close that provider's later dispatch-to-open
race or cancel arbitrary blocking code. Providers with strict safety
requirements must consume `PreviewFile` or give their subprocess/library an
equivalent no-follow, bounded-I/O contract. On targets other than Unix and
Windows, the standard-library fallback performs the non-following metadata and
canonical-boundary checks but cannot make the final open atomically
no-following or verify a portable file identity; Latte Lens' release CI and
packages cover Linux, macOS, and Windows.

## Minimal provider

```rust
use anyhow::Result;
use latte_lens::preview::{
    PreviewContent, PreviewProvider, PreviewRegistry, PreviewRequest,
};

struct PdfPreviewProvider;

impl PreviewProvider for PdfPreviewProvider {
    fn id(&self) -> &'static str {
        "pdf"
    }

    fn preview(
        &self,
        request: &PreviewRequest<'_>,
    ) -> Result<Option<PreviewContent>> {
        let is_pdf = request
            .absolute_path
            .extension()
            .and_then(|extension| extension.to_str())
            == Some("pdf");
        if !is_pdf {
            return Ok(None);
        }

        // Replace this placeholder with a PDF library or a bounded external
        // command adapter. Cap its output using max_bytes and max_lines.
        let lines = vec![format!("PDF preview: {}", request.display_path.display())];
        Ok(Some(PreviewContent::new(lines)))
    }
}

let mut registry = PreviewRegistry::with_builtins();
registry.register(PdfPreviewProvider);
```

Use the registry when creating the app:

```rust,ignore
let app = App::with_preview_registry(repository_path, registry)?;
```

For a provider added after application construction, call
`app.register_preview_provider(provider)`. The current selection is refreshed
through the background worker; the request is immediate and its result is
applied asynchronously.

## Suitable extension strategies

- PDF: wrap a library or a bounded `pdftotext` subprocess.
- Word: extract paragraphs through an OOXML library or a bounded converter.
- Images: produce metadata, OCR text, or a future terminal image payload.
- Archives: list entries without extracting into the repository.

The provider API keeps text and semantic highlight ranges terminal-neutral;
Ratatui styles are applied only in the UI. A future preview payload enum can
add images or structured pages while preserving the registry and application
integration point.
