//! Extensible, read-only file previews.
//!
//! Providers are queried in reverse registration order. This lets an optional
//! format-specific provider (for example, PDF or Word) override the built-in
//! text sniffer without coupling Latte Lens to that format or its dependencies.

use std::{
    io::{Read, Seek, SeekFrom},
    ops::Range,
    path::Path,
    str::FromStr,
    sync::{Arc, OnceLock},
};

use anyhow::{Context, Result};
use syntect::{
    easy::ScopeRangeIterator,
    highlighting::ScopeSelectors,
    parsing::{ParseState, ScopeStack, SyntaxSet},
};

use crate::content_safety::{
    ContentPathKind, OpenRegular, SafeFile, inspect_content_path, open_regular,
};
use crate::folding::FoldSource;

pub const DEFAULT_MAX_BYTES: usize = 256 * 1024;
pub const DEFAULT_MAX_LINES: usize = 10_000;
const MAX_HIGHLIGHT_LINE_BYTES: usize = 16 * 1024;

/// Semantic class attached to a byte range in a source preview.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HighlightKind {
    Comment,
    String,
    Keyword,
    Function,
    Type,
    Number,
    Constant,
    Attribute,
    SearchMatch,
    Search,
}

/// A syntax-highlighted byte range within one logical preview line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HighlightSpan {
    pub range: Range<usize>,
    pub kind: HighlightKind,
}

/// Everything a provider needs to produce a bounded preview.
#[derive(Clone, Copy, Debug)]
pub struct PreviewRequest<'a> {
    /// Filesystem path used for reading. The application normally supplies an
    /// absolute path rooted in the repository being viewed.
    pub absolute_path: &'a Path,
    /// User-facing path used in diagnostics. This can stay relative to the
    /// repository even when `absolute_path` is canonicalized.
    pub display_path: &'a Path,
    /// Canonical content boundary used to reject link-bearing path components
    /// and paths outside the selected workspace.
    content_root: Option<&'a Path>,
    pub max_bytes: usize,
    pub max_lines: usize,
}

impl<'a> PreviewRequest<'a> {
    pub fn new(absolute_path: &'a Path, display_path: &'a Path) -> Self {
        Self {
            absolute_path,
            display_path,
            content_root: None,
            max_bytes: DEFAULT_MAX_BYTES,
            max_lines: DEFAULT_MAX_LINES,
        }
    }

    /// Constrain this request to a canonical workspace or repository root.
    pub fn within_root(mut self, content_root: &'a Path) -> Self {
        self.content_root = Some(content_root);
        self
    }

    pub fn with_limits(mut self, max_bytes: usize, max_lines: usize) -> Self {
        self.max_bytes = max_bytes;
        self.max_lines = max_lines;
        self
    }

    /// Open the selected object only when it is still a real regular file.
    ///
    /// Optional providers should use this method instead of reopening
    /// `absolute_path`; it applies the same no-link, non-blocking policy as the
    /// built-in text preview and Git's untracked-file renderer.
    pub fn open_regular(&self) -> Result<Option<PreviewFile>> {
        match open_regular(self.content_root, self.absolute_path).with_context(|| {
            format!(
                "cannot safely open preview file {}",
                self.display_path.display()
            )
        })? {
            OpenRegular::Opened(file) => Ok(Some(PreviewFile { file })),
            OpenRegular::Declined(_) => Ok(None),
        }
    }
}

/// Read/seek handle for a preview file opened through the content-safety gate.
pub struct PreviewFile {
    file: SafeFile,
}

impl PreviewFile {
    pub fn len(&self) -> u64 {
        self.file.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Read for PreviewFile {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buffer)
    }
}

impl Seek for PreviewFile {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(position)
    }
}

/// Provider-independent preview content.
///
/// Keeping this separate from [`Preview`] means the registry remains the
/// source of truth for the provider id.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewContent {
    pub lines: Vec<String>,
    pub highlights: Vec<Vec<HighlightSpan>>,
    pub truncated: bool,
    pub show_line_numbers: bool,
}

impl PreviewContent {
    pub fn new(lines: Vec<String>) -> Self {
        let highlights = vec![Vec::new(); lines.len()];
        Self {
            lines,
            highlights,
            truncated: false,
            show_line_numbers: false,
        }
    }

    pub fn with_truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }

    pub fn with_line_numbers(mut self, show_line_numbers: bool) -> Self {
        self.show_line_numbers = show_line_numbers;
        self
    }

    pub fn with_highlights(mut self, highlights: Vec<Vec<HighlightSpan>>) -> Self {
        if highlights.len() == self.lines.len() {
            self.highlights = highlights;
        }
        self
    }
}

/// A resolved preview ready for the application or UI layer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Preview {
    pub provider_id: String,
    pub lines: Vec<String>,
    pub highlights: Vec<Vec<HighlightSpan>>,
    pub truncated: bool,
    pub show_line_numbers: bool,
}

/// Extension point for optional preview implementations.
///
/// Returning `Ok(None)` declines the request and lets the registry try the
/// next provider. Providers must only inspect the requested file; previewing
/// must never mutate it or the surrounding repository.
pub trait PreviewProvider: Send + Sync {
    fn id(&self) -> &'static str;

    fn preview(&self, request: &PreviewRequest<'_>) -> Result<Option<PreviewContent>>;
}

/// Ordered collection of preview providers.
#[derive(Clone)]
pub struct PreviewRegistry {
    providers: Vec<ProviderEntry>,
}

#[derive(Clone)]
struct ProviderEntry {
    provider: Arc<dyn PreviewProvider>,
    fold_source: FoldSource,
}

pub(crate) enum PreviewResolution {
    Preview {
        preview: Preview,
        fold_source: FoldSource,
    },
    Unsupported,
    Unsafe {
        kind: ContentPathKind,
        offending_path: std::path::PathBuf,
    },
}

impl PreviewRegistry {
    /// Construct an empty registry for callers that want full control over
    /// supported formats.
    pub fn empty() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    /// Construct a registry containing Latte Lens' built-in providers.
    pub fn with_builtins() -> Self {
        let mut registry = Self::empty();
        registry.providers.push(ProviderEntry {
            provider: Arc::new(TextPreviewProvider),
            fold_source: FoldSource::BuiltinText,
        });
        registry
    }

    /// Register a provider at the highest priority.
    ///
    /// Providers are queried from last registered to first registered. A PDF
    /// provider registered after the built-ins therefore gets the first chance
    /// to handle `.pdf`, even when a particular PDF happens to look textual.
    pub fn register<P>(&mut self, provider: P) -> &mut Self
    where
        P: PreviewProvider + 'static,
    {
        self.providers.push(ProviderEntry {
            provider: Arc::new(provider),
            fold_source: FoldSource::None,
        });
        self
    }

    /// Resolve a preview, returning `None` when every provider declines it.
    pub fn preview(&self, request: &PreviewRequest<'_>) -> Result<Option<Preview>> {
        match self.resolve(request)? {
            PreviewResolution::Preview { preview, .. } => Ok(Some(preview)),
            PreviewResolution::Unsupported | PreviewResolution::Unsafe { .. } => Ok(None),
        }
    }

    pub(crate) fn resolve(&self, request: &PreviewRequest<'_>) -> Result<PreviewResolution> {
        let inspected = inspect_content_path(request.content_root, request.absolute_path)
            .with_context(|| {
                format!(
                    "cannot inspect preview file {}",
                    request.display_path.display()
                )
            })?;

        if inspected.kind != ContentPathKind::Regular || inspected.path != request.absolute_path {
            return Ok(PreviewResolution::Unsafe {
                kind: inspected.kind,
                offending_path: inspected.path,
            });
        }

        for entry in self.providers.iter().rev() {
            if let Some(content) = entry.provider.preview(request)? {
                return Ok(PreviewResolution::Preview {
                    preview: Preview {
                        provider_id: entry.provider.id().to_owned(),
                        lines: content.lines,
                        highlights: content.highlights,
                        truncated: content.truncated,
                        show_line_numbers: content.show_line_numbers,
                    },
                    fold_source: entry.fold_source,
                });
            }
        }

        Ok(PreviewResolution::Unsupported)
    }
}

impl Default for PreviewRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

/// UTF-8 text and source-code preview provider.
///
/// Detection is content based rather than extension based, so extensionless
/// files and uncommon source-code extensions work automatically. NUL bytes and
/// malformed UTF-8 are treated as binary and declined.
#[derive(Clone, Copy, Debug, Default)]
pub struct TextPreviewProvider;

impl PreviewProvider for TextPreviewProvider {
    fn id(&self) -> &'static str {
        "text"
    }

    fn preview(&self, request: &PreviewRequest<'_>) -> Result<Option<PreviewContent>> {
        let Some(file) = request.open_regular()? else {
            return Ok(None);
        };
        let read_limit = u64::try_from(request.max_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let mut bytes = Vec::new();
        file.take(read_limit)
            .read_to_end(&mut bytes)
            .with_context(|| {
                format!(
                    "cannot read preview file {}",
                    request.display_path.display()
                )
            })?;

        // NUL is a strong, inexpensive binary signal. Inspect the look-ahead
        // byte too so a binary file is not accepted merely because the display
        // boundary happens immediately before its first NUL.
        if bytes.contains(&0) {
            return Ok(None);
        }

        let truncated_by_bytes = bytes.len() > request.max_bytes;
        bytes.truncate(request.max_bytes);
        let Some(text) = utf8_prefix(&bytes, truncated_by_bytes) else {
            return Ok(None);
        };
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);

        let mut source_lines = text.lines();
        let lines: Vec<String> = source_lines
            .by_ref()
            .take(request.max_lines)
            .map(ToOwned::to_owned)
            .collect();
        let truncated_by_lines = source_lines.next().is_some();

        let highlights = syntax_highlights(request.display_path, &lines).unwrap_or_default();

        Ok(Some(
            PreviewContent::new(lines)
                .with_highlights(highlights)
                .with_truncated(truncated_by_bytes || truncated_by_lines)
                .with_line_numbers(true),
        ))
    }
}

fn syntax_highlights(path: &Path, lines: &[String]) -> Option<Vec<Vec<HighlightSpan>>> {
    if lines
        .iter()
        .any(|line| line.len() > MAX_HIGHLIGHT_LINE_BYTES)
    {
        return None;
    }

    let syntax_set = syntax_set();
    let syntax = path
        .extension()
        .and_then(|extension| extension.to_str())
        .and_then(|extension| syntax_set.find_syntax_by_extension(extension))
        .or_else(|| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| syntax_set.find_syntax_by_token(name))
        })
        .or_else(|| {
            lines
                .first()
                .and_then(|line| syntax_set.find_syntax_by_first_line(line))
        })?;
    if syntax.name == "Plain Text" {
        return None;
    }

    let mut parse_state = ParseState::new(syntax);
    let mut scope_stack = ScopeStack::new();
    let mut highlighted_lines = Vec::with_capacity(lines.len());
    for line in lines {
        let operations = parse_state.parse_line(line, syntax_set).ok()?;
        let mut offset = 0;
        let mut highlights: Vec<HighlightSpan> = Vec::new();
        for (token, operation) in ScopeRangeIterator::new(&operations, line) {
            scope_stack.apply(operation).ok()?;
            let start = offset;
            offset += token.len();
            let Some(kind) = classify_scope(&scope_stack) else {
                continue;
            };
            if start == offset {
                continue;
            }
            if let Some(previous) = highlights.last_mut()
                && previous.kind == kind
                && previous.range.end == start
            {
                previous.range.end = offset;
            } else {
                highlights.push(HighlightSpan {
                    range: start..offset,
                    kind,
                });
            }
        }
        highlighted_lines.push(highlights);
    }
    Some(highlighted_lines)
}

fn syntax_set() -> &'static SyntaxSet {
    static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAX_SET.get_or_init(two_face::syntax::extra_no_newlines)
}

struct ScopeClassifiers {
    comment: ScopeSelectors,
    string: ScopeSelectors,
    keyword: ScopeSelectors,
    function: ScopeSelectors,
    ty: ScopeSelectors,
    number: ScopeSelectors,
    constant: ScopeSelectors,
    attribute: ScopeSelectors,
}

fn scope_classifiers() -> &'static ScopeClassifiers {
    static CLASSIFIERS: OnceLock<ScopeClassifiers> = OnceLock::new();
    CLASSIFIERS.get_or_init(|| ScopeClassifiers {
        comment: selectors("comment"),
        string: selectors("string"),
        keyword: selectors(
            "keyword, storage.modifier, storage.control, storage.type.function, \
             storage.type.class, storage.type.struct, storage.type.enum, \
             storage.type.interface, storage.type.trait",
        ),
        function: selectors("entity.name.function, support.function, variable.function"),
        ty: selectors(
            "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, \
             entity.name.interface, entity.name.trait, support.type, storage.type",
        ),
        number: selectors("constant.numeric"),
        constant: selectors("constant, variable.language, support.constant"),
        attribute: selectors("entity.other.attribute-name, meta.attribute"),
    })
}

fn selectors(value: &str) -> ScopeSelectors {
    ScopeSelectors::from_str(value).expect("built-in syntax scope selectors must be valid")
}

fn classify_scope(stack: &ScopeStack) -> Option<HighlightKind> {
    let classifiers = scope_classifiers();
    let scopes = stack.as_slice();
    [
        (&classifiers.comment, HighlightKind::Comment),
        (&classifiers.string, HighlightKind::String),
        (&classifiers.function, HighlightKind::Function),
        (&classifiers.keyword, HighlightKind::Keyword),
        (&classifiers.ty, HighlightKind::Type),
        (&classifiers.number, HighlightKind::Number),
        (&classifiers.attribute, HighlightKind::Attribute),
        (&classifiers.constant, HighlightKind::Constant),
    ]
    .into_iter()
    .find_map(|(selector, kind)| selector.does_match(scopes).map(|_| kind))
}

fn utf8_prefix(bytes: &[u8], truncated_by_bytes: bool) -> Option<&str> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(error) if truncated_by_bytes && error.error_len().is_none() => {
            // A byte limit may split a valid multi-byte codepoint. Keep only
            // the complete UTF-8 prefix instead of rendering a replacement
            // character or misclassifying the whole file as binary.
            std::str::from_utf8(&bytes[..error.valid_up_to()]).ok()
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Read, Seek, SeekFrom},
        path::Path,
    };

    use anyhow::Result;
    use tempfile::tempdir;

    use super::{
        HighlightKind, PreviewContent, PreviewProvider, PreviewRegistry, PreviewRequest,
        PreviewResolution, TextPreviewProvider,
    };
    use crate::folding::FoldSource;

    #[test]
    fn previews_code_and_extensionless_utf8_text() -> Result<()> {
        let directory = tempdir()?;
        let code = directory.path().join("main.rs");
        let extensionless = directory.path().join("README");
        fs::write(&code, "fn main() {\n    println!(\"latte\");\n}\n")?;
        fs::write(&extensionless, "Latte Lens\n看清每一次修改\n")?;
        let registry = PreviewRegistry::default();

        let code_preview = registry
            .preview(&PreviewRequest::new(&code, Path::new("src/main.rs")))?
            .expect("Rust source should be previewable");
        assert_eq!(code_preview.provider_id, "text");
        assert_eq!(code_preview.lines[0], "fn main() {");
        assert!(code_preview.show_line_numbers);
        assert!(
            highlight_contains(&code_preview, 0, "fn", HighlightKind::Keyword),
            "{:?}",
            code_preview.highlights
        );
        assert!(highlight_contains(
            &code_preview,
            0,
            "main",
            HighlightKind::Function
        ));
        assert!(highlight_contains(
            &code_preview,
            1,
            "\"latte\"",
            HighlightKind::String
        ));

        let text_preview = registry
            .preview(&PreviewRequest::new(&extensionless, Path::new("README")))?
            .expect("extensionless UTF-8 should be previewable");
        assert_eq!(text_preview.lines, ["Latte Lens", "看清每一次修改"]);
        assert!(text_preview.highlights.iter().all(Vec::is_empty));
        assert!(!text_preview.truncated);
        Ok(())
    }

    #[test]
    fn safely_opened_preview_files_report_size_and_support_reading_and_seeking() -> Result<()> {
        let directory = tempdir()?;
        let root = directory.path().canonicalize()?;
        let non_empty = root.join("notes.txt");
        let empty = root.join("empty.txt");
        fs::write(&non_empty, "latte")?;
        fs::write(&empty, "")?;

        let mut file = PreviewRequest::new(&non_empty, Path::new("notes.txt"))
            .within_root(&root)
            .open_regular()?
            .expect("regular file should pass the content-safety gate");
        assert_eq!(file.len(), 5);
        assert!(!file.is_empty());
        assert_eq!(file.seek(SeekFrom::Start(2))?, 2);
        let mut suffix = String::new();
        file.read_to_string(&mut suffix)?;
        assert_eq!(suffix, "tte");

        let empty = PreviewRequest::new(&empty, Path::new("empty.txt"))
            .within_root(&root)
            .open_regular()?
            .expect("empty regular file should still be openable");
        assert!(empty.is_empty());
        Ok(())
    }

    fn highlight_contains(
        preview: &super::Preview,
        line_index: usize,
        expected: &str,
        kind: HighlightKind,
    ) -> bool {
        let Some(line) = preview.lines.get(line_index) else {
            return false;
        };
        preview
            .highlights
            .get(line_index)
            .into_iter()
            .flatten()
            .any(|highlight| {
                highlight.kind == kind && line.get(highlight.range.clone()) == Some(expected)
            })
    }

    #[test]
    fn declines_binary_content_with_a_nul_byte() -> Result<()> {
        let directory = tempdir()?;
        let binary = directory.path().join("image.bin");
        fs::write(&binary, b"looks textual\0but is binary")?;

        let preview = PreviewRegistry::with_builtins()
            .preview(&PreviewRequest::new(&binary, Path::new("image.bin")))?;

        assert!(preview.is_none());
        Ok(())
    }

    #[test]
    fn unusually_long_source_lines_fall_back_to_plain_text() {
        let lines = vec!["x".repeat(super::MAX_HIGHLIGHT_LINE_BYTES + 1)];

        assert!(super::syntax_highlights(Path::new("main.rs"), &lines).is_none());
    }

    #[test]
    fn highlights_typescript_and_tsx_sources() -> Result<()> {
        let directory = tempdir()?;
        let typescript = directory.path().join("greet.ts");
        let tsx = directory.path().join("app.tsx");
        fs::write(
            &typescript,
            "export function greet(name: string): string {\n    return `Hello ${name}`;\n}\n",
        )?;
        fs::write(
            &tsx,
            "export function App(): JSX.Element {\n    return <main>Hello</main>;\n}\n",
        )?;
        let registry = PreviewRegistry::with_builtins();

        let typescript_preview = registry
            .preview(&PreviewRequest::new(&typescript, Path::new("src/greet.ts")))?
            .expect("TypeScript source should be previewable");
        assert!(highlight_contains(
            &typescript_preview,
            0,
            "function",
            HighlightKind::Keyword
        ));
        assert!(highlight_contains(
            &typescript_preview,
            0,
            "greet",
            HighlightKind::Function
        ));
        assert!(
            typescript_preview
                .highlights
                .iter()
                .any(|line| !line.is_empty())
        );

        let tsx_preview = registry
            .preview(&PreviewRequest::new(&tsx, Path::new("src/app.tsx")))?
            .expect("TSX source should be previewable");
        assert!(tsx_preview.highlights.iter().any(|line| !line.is_empty()));
        Ok(())
    }

    #[test]
    fn marks_byte_limited_previews_as_truncated() -> Result<()> {
        let directory = tempdir()?;
        let file = directory.path().join("notes.txt");
        fs::write(&file, "abcdef\nsecond line\n")?;

        let preview = PreviewRegistry::with_builtins()
            .preview(&PreviewRequest::new(&file, Path::new("notes.txt")).with_limits(3, 50))?
            .expect("text should be previewable");

        assert_eq!(preview.lines, ["abc"]);
        assert!(preview.truncated);
        Ok(())
    }

    #[test]
    fn marks_line_limited_previews_as_truncated() -> Result<()> {
        let directory = tempdir()?;
        let file = directory.path().join("notes.txt");
        fs::write(&file, "one\ntwo\nthree\n")?;

        let preview = PreviewRegistry::with_builtins()
            .preview(&PreviewRequest::new(&file, Path::new("notes.txt")).with_limits(1_024, 2))?
            .expect("text should be previewable");

        assert_eq!(preview.lines, ["one", "two"]);
        assert!(preview.truncated);
        Ok(())
    }

    #[test]
    fn trims_a_codepoint_split_by_the_byte_limit() -> Result<()> {
        let directory = tempdir()?;
        let file = directory.path().join("utf8.txt");
        fs::write(&file, "a拿b")?;

        let preview = PreviewRegistry::with_builtins()
            .preview(&PreviewRequest::new(&file, Path::new("utf8.txt")).with_limits(2, 10))?
            .expect("a valid UTF-8 prefix should be previewable");

        assert_eq!(preview.lines, ["a"]);
        assert!(preview.truncated);
        Ok(())
    }

    #[test]
    fn last_registered_provider_overrides_the_text_builtin() -> Result<()> {
        struct FakePdfProvider;

        impl PreviewProvider for FakePdfProvider {
            fn id(&self) -> &'static str {
                "fake-pdf"
            }

            fn preview(&self, request: &PreviewRequest<'_>) -> Result<Option<PreviewContent>> {
                if request
                    .absolute_path
                    .extension()
                    .and_then(|value| value.to_str())
                    == Some("pdf")
                {
                    return Ok(Some(PreviewContent::new(vec![
                        "rendered PDF page".to_owned(),
                    ])));
                }
                Ok(None)
            }
        }

        let directory = tempdir()?;
        let pdf = directory.path().join("spec.pdf");
        // This deliberately looks like valid text so the test proves provider
        // ordering, rather than succeeding because the text provider declines.
        fs::write(&pdf, "%PDF-1.7 textual test fixture")?;
        let mut registry = PreviewRegistry::with_builtins();
        registry.register(FakePdfProvider);

        let preview = registry
            .preview(&PreviewRequest::new(&pdf, Path::new("docs/spec.pdf")))?
            .expect("fake PDF provider should handle the file");

        assert_eq!(preview.provider_id, "fake-pdf");
        assert_eq!(preview.lines, ["rendered PDF page"]);
        assert!(!preview.show_line_numbers);
        Ok(())
    }

    #[test]
    fn custom_provider_named_text_does_not_gain_builtin_folding_capability() -> Result<()> {
        struct CustomText;

        impl PreviewProvider for CustomText {
            fn id(&self) -> &'static str {
                "text"
            }

            fn preview(&self, _: &PreviewRequest<'_>) -> Result<Option<PreviewContent>> {
                Ok(Some(PreviewContent::new(vec![
                    "fn deceptive() {".to_owned(),
                    "}".to_owned(),
                ])))
            }
        }

        let directory = tempdir()?;
        let source = directory.path().join("fake.rs");
        fs::write(&source, "ignored")?;
        let mut registry = PreviewRegistry::with_builtins();
        registry.register(CustomText);
        let resolution = registry.resolve(&PreviewRequest::new(&source, Path::new("fake.rs")))?;
        assert!(matches!(
            resolution,
            PreviewResolution::Preview {
                fold_source: FoldSource::None,
                ..
            }
        ));
        Ok(())
    }

    #[test]
    fn reports_a_missing_file_as_an_error() {
        let directory = tempdir().expect("temporary directory");
        let missing = directory.path().join("missing.txt");

        let error = PreviewRegistry::with_builtins()
            .preview(&PreviewRequest::new(
                &missing,
                Path::new("docs/missing.txt"),
            ))
            .expect_err("a missing file must not look merely unsupported");

        assert!(
            error
                .to_string()
                .contains("cannot inspect preview file docs/missing.txt")
        );
    }

    #[test]
    fn text_provider_declines_malformed_utf8() -> Result<()> {
        let directory = tempdir()?;
        let file = directory.path().join("invalid.txt");
        fs::write(&file, [0xff, 0xfe, 0xfd])?;
        let request = PreviewRequest::new(&file, Path::new("invalid.txt"));

        assert!(TextPreviewProvider.preview(&request)?.is_none());
        Ok(())
    }
}
