use std::{collections::HashMap, ops::Range, path::Path};

use pulldown_cmark::{Event, HeadingLevel, Options, Parser as MarkdownParser, Tag, TagEnd};
use tree_sitter::{Language, Node, Parser, Tree};

use crate::navigation::{SourcePosition, SourceRange};

pub(crate) const MAX_FOLD_NODES: usize = 100_000;
pub(crate) const MAX_MARKDOWN_EVENTS: usize = 50_000;
pub(crate) const MAX_FOLD_REGIONS: usize = 4_096;
pub(crate) const MAX_SYMBOL_NODES: usize = 100_000;
pub(crate) const MAX_MARKDOWN_SYMBOL_EVENTS: usize = 50_000;
pub(crate) const MAX_STRUCTURE_SYMBOLS: usize = 4_096;
pub(crate) const MAX_SYMBOL_DEPTH: usize = 64;
pub(crate) const MAX_TOKEN_NODES: usize = 100_000;
pub(crate) const MAX_RECOGNIZABLE_TOKENS: usize = 65_536;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum FoldKind {
    Heading,
    CodeBlock,
    Function,
    Method,
    Type,
    Module,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct FoldAnchor {
    kind: FoldKind,
    header_hash: u64,
    occurrence: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FoldRegion {
    pub start_line: usize,
    pub end_line: usize,
    pub kind: FoldKind,
    pub anchor: FoldAnchor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FoldSource {
    None,
    BuiltinText,
}

impl FoldSource {
    pub const fn allows_folding(self) -> bool {
        matches!(self, Self::BuiltinText)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StructureSource {
    None,
    Markdown,
    TreeSitter,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct SymbolId(pub(crate) u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SymbolKind {
    Heading,
    Function,
    Method,
    Type,
    Module,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StructureSymbol {
    pub(crate) id: SymbolId,
    pub(crate) name: String,
    pub(crate) kind: SymbolKind,
    pub(crate) range: SourceRange,
    pub(crate) selection_range: SourceRange,
    pub(crate) parent: Option<SymbolId>,
    pub(crate) detail: Option<String>,
    pub(crate) container: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecognizableTokenIndex {
    pub(crate) ranges: Vec<SourceRange>,
    pub(crate) complete: bool,
}

impl RecognizableTokenIndex {
    pub(crate) fn empty(complete: bool) -> Self {
        Self {
            ranges: Vec::new(),
            complete,
        }
    }

    /// Returns the smallest precomputed named leaf containing `point`.
    ///
    /// Ranges are sorted, non-overlapping, and end-exclusive, so foreground
    /// navigation can use a binary lookup without parsing or scanning the AST.
    #[allow(dead_code)] // Used by the following foreground navigation reducer node.
    pub(crate) fn containing(&self, point: SourcePosition) -> Option<SourceRange> {
        if !self.complete {
            return None;
        }
        let index = self
            .ranges
            .partition_point(|range| range.start <= point)
            .checked_sub(1)?;
        let range = self.ranges[index];
        (range.start <= point && point < range.end).then_some(range)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StructureSnapshot {
    pub(crate) source: StructureSource,
    pub(crate) folds: Vec<FoldRegion>,
    pub(crate) symbols: Vec<StructureSymbol>,
    pub(crate) symbols_complete: bool,
    pub(crate) recognizable_tokens: RecognizableTokenIndex,
}

impl StructureSnapshot {
    pub(crate) fn unavailable() -> Self {
        Self {
            source: StructureSource::None,
            folds: Vec::new(),
            symbols: Vec::new(),
            symbols_complete: false,
            recognizable_tokens: RecognizableTokenIndex::empty(false),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FoldLimits {
    pub max_nodes: usize,
    pub max_markdown_events: usize,
    pub max_regions: usize,
    /// Deterministic cancellation hook for tests. Production leaves this unset.
    pub cancel_after_work: Option<usize>,
}

impl Default for FoldLimits {
    fn default() -> Self {
        Self {
            max_nodes: MAX_FOLD_NODES,
            max_markdown_events: MAX_MARKDOWN_EVENTS,
            max_regions: MAX_FOLD_REGIONS,
            cancel_after_work: None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct StructureLimits {
    pub(crate) fold: FoldLimits,
    pub(crate) max_symbol_nodes: usize,
    pub(crate) max_markdown_symbol_events: usize,
    pub(crate) max_symbols: usize,
    pub(crate) max_symbol_depth: usize,
    pub(crate) cancel_symbols_after_work: Option<usize>,
    pub(crate) max_token_nodes: usize,
    pub(crate) max_tokens: usize,
    pub(crate) cancel_tokens_after_work: Option<usize>,
}

impl Default for StructureLimits {
    fn default() -> Self {
        Self {
            fold: FoldLimits::default(),
            max_symbol_nodes: MAX_SYMBOL_NODES,
            max_markdown_symbol_events: MAX_MARKDOWN_SYMBOL_EVENTS,
            max_symbols: MAX_STRUCTURE_SYMBOLS,
            max_symbol_depth: MAX_SYMBOL_DEPTH,
            cancel_symbols_after_work: None,
            max_token_nodes: MAX_TOKEN_NODES,
            max_tokens: MAX_RECOGNIZABLE_TOKENS,
            cancel_tokens_after_work: None,
        }
    }
}

#[derive(Default)]
struct WorkBudget {
    used: usize,
    cancel_after: Option<usize>,
}

impl WorkBudget {
    fn step(&mut self) -> bool {
        self.used = self.used.saturating_add(1);
        self.cancel_after.is_none_or(|limit| self.used <= limit)
    }
}

#[derive(Clone, Debug)]
struct RawRegion {
    start_line: usize,
    end_line: usize,
    kind: FoldKind,
}

#[cfg(test)]
pub(crate) fn fold_regions(path: &Path, lines: &[String]) -> Vec<FoldRegion> {
    fold_regions_with_limits(path, lines, FoldLimits::default())
}

#[cfg(test)]
pub(crate) fn fold_regions_with_limits(
    path: &Path,
    lines: &[String],
    limits: FoldLimits,
) -> Vec<FoldRegion> {
    if lines.len() < 2 {
        return Vec::new();
    }
    let source = lines.join("\n");
    let line_starts = line_starts(&source);
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut budget = WorkBudget {
        cancel_after: limits.cancel_after_work,
        ..WorkBudget::default()
    };
    let raw = match extension.as_str() {
        "md" | "markdown" => markdown_regions(
            &source,
            &line_starts,
            limits.max_markdown_events,
            &mut budget,
        ),
        #[cfg(feature = "tree-sitter-rust")]
        "rs" => code_regions(
            &source,
            &line_starts,
            tree_sitter_rust::LANGUAGE.into(),
            CodeLanguage::Rust,
            limits.max_nodes,
            &mut budget,
        ),
        #[cfg(feature = "tree-sitter-typescript")]
        "ts" | "mts" | "cts" => code_regions(
            &source,
            &line_starts,
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            CodeLanguage::TypeScript,
            limits.max_nodes,
            &mut budget,
        ),
        #[cfg(feature = "tree-sitter-typescript")]
        "tsx" => code_regions(
            &source,
            &line_starts,
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            CodeLanguage::TypeScript,
            limits.max_nodes,
            &mut budget,
        ),
        #[cfg(feature = "tree-sitter-javascript")]
        "js" | "mjs" | "cjs" | "jsx" => code_regions(
            &source,
            &line_starts,
            tree_sitter_javascript::LANGUAGE.into(),
            CodeLanguage::JavaScript,
            limits.max_nodes,
            &mut budget,
        ),
        #[cfg(feature = "tree-sitter-python")]
        "py" | "pyi" => code_regions(
            &source,
            &line_starts,
            tree_sitter_python::LANGUAGE.into(),
            CodeLanguage::Python,
            limits.max_nodes,
            &mut budget,
        ),
        #[cfg(feature = "tree-sitter-go")]
        "go" => code_regions(
            &source,
            &line_starts,
            tree_sitter_go::LANGUAGE.into(),
            CodeLanguage::Go,
            limits.max_nodes,
            &mut budget,
        ),
        _ => Some(Vec::new()),
    };
    let Some(raw) = raw else {
        return Vec::new();
    };
    normalize_regions(raw, lines, limits.max_regions)
}

/// Builds the immutable structure consumed by folding and navigation.
///
/// Code is parsed once, then projected with three independent budgets. Failure
/// in one projection never clears a successful sibling projection.
pub(crate) fn structure_snapshot(path: &Path, lines: &[String]) -> StructureSnapshot {
    structure_snapshot_with_limits(path, lines, StructureLimits::default())
}

pub(crate) fn structure_snapshot_with_limits(
    path: &Path,
    lines: &[String],
    limits: StructureLimits,
) -> StructureSnapshot {
    let source = lines.join("\n");
    let starts = line_starts(&source);
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    match extension.as_str() {
        "md" | "markdown" => markdown_structure_snapshot(&source, &starts, lines, limits),
        #[cfg(feature = "tree-sitter-rust")]
        "rs" => code_structure_snapshot(
            &source,
            &starts,
            lines,
            tree_sitter_rust::LANGUAGE.into(),
            CodeLanguage::Rust,
            limits,
        ),
        #[cfg(feature = "tree-sitter-typescript")]
        "ts" | "mts" | "cts" => code_structure_snapshot(
            &source,
            &starts,
            lines,
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            CodeLanguage::TypeScript,
            limits,
        ),
        #[cfg(feature = "tree-sitter-typescript")]
        "tsx" => code_structure_snapshot(
            &source,
            &starts,
            lines,
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            CodeLanguage::TypeScript,
            limits,
        ),
        #[cfg(feature = "tree-sitter-javascript")]
        "js" | "mjs" | "cjs" | "jsx" => code_structure_snapshot(
            &source,
            &starts,
            lines,
            tree_sitter_javascript::LANGUAGE.into(),
            CodeLanguage::JavaScript,
            limits,
        ),
        #[cfg(feature = "tree-sitter-python")]
        "py" | "pyi" => code_structure_snapshot(
            &source,
            &starts,
            lines,
            tree_sitter_python::LANGUAGE.into(),
            CodeLanguage::Python,
            limits,
        ),
        #[cfg(feature = "tree-sitter-go")]
        "go" => code_structure_snapshot(
            &source,
            &starts,
            lines,
            tree_sitter_go::LANGUAGE.into(),
            CodeLanguage::Go,
            limits,
        ),
        _ => StructureSnapshot::unavailable(),
    }
}

fn markdown_structure_snapshot(
    source: &str,
    starts: &[usize],
    lines: &[String],
    limits: StructureLimits,
) -> StructureSnapshot {
    let mut fold_budget = WorkBudget {
        cancel_after: limits.fold.cancel_after_work,
        ..WorkBudget::default()
    };
    let folds = markdown_regions(
        source,
        starts,
        limits.fold.max_markdown_events,
        &mut fold_budget,
    )
    .map_or_else(Vec::new, |raw| {
        normalize_regions(raw, lines, limits.fold.max_regions)
    });

    let mut symbol_budget = WorkBudget {
        cancel_after: limits.cancel_symbols_after_work,
        ..WorkBudget::default()
    };
    let symbols = markdown_symbols(source, starts, limits, &mut symbol_budget);
    let (symbols, symbols_complete) =
        symbols.map_or_else(|| (Vec::new(), false), |symbols| (symbols, true));

    StructureSnapshot {
        source: StructureSource::Markdown,
        folds,
        symbols,
        symbols_complete,
        // Markdown deliberately has no semantic token allowlist. This is a
        // successful empty projection, not an incomplete parse.
        recognizable_tokens: RecognizableTokenIndex::empty(true),
    }
}

fn code_structure_snapshot(
    source: &str,
    starts: &[usize],
    lines: &[String],
    language: Language,
    flavor: CodeLanguage,
    limits: StructureLimits,
) -> StructureSnapshot {
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return failed_code_structure_snapshot();
    }
    let Some(tree) = parser.parse(source, None) else {
        return failed_code_structure_snapshot();
    };

    let mut fold_budget = WorkBudget {
        cancel_after: limits.fold.cancel_after_work,
        ..WorkBudget::default()
    };
    let folds = collect_code_regions(
        &tree,
        source.len(),
        starts,
        flavor,
        limits.fold.max_nodes,
        &mut fold_budget,
    )
    .map_or_else(Vec::new, |raw| {
        normalize_regions(raw, lines, limits.fold.max_regions)
    });

    let mut symbol_budget = WorkBudget {
        cancel_after: limits.cancel_symbols_after_work,
        ..WorkBudget::default()
    };
    let symbols = collect_code_symbols(&tree, source, starts, flavor, limits, &mut symbol_budget);
    let (symbols, symbols_complete) =
        symbols.map_or_else(|| (Vec::new(), false), |symbols| (symbols, true));

    let mut token_budget = WorkBudget {
        cancel_after: limits.cancel_tokens_after_work,
        ..WorkBudget::default()
    };
    let recognizable_tokens =
        collect_recognizable_tokens(&tree, source, starts, flavor, limits, &mut token_budget)
            .map_or_else(
                || RecognizableTokenIndex::empty(false),
                |ranges| RecognizableTokenIndex {
                    ranges,
                    complete: true,
                },
            );

    StructureSnapshot {
        source: StructureSource::TreeSitter,
        folds,
        symbols,
        symbols_complete,
        recognizable_tokens,
    }
}

fn failed_code_structure_snapshot() -> StructureSnapshot {
    StructureSnapshot {
        source: StructureSource::TreeSitter,
        folds: Vec::new(),
        symbols: Vec::new(),
        symbols_complete: false,
        recognizable_tokens: RecognizableTokenIndex::empty(false),
    }
}

fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(
        source
            .bytes()
            .enumerate()
            .filter_map(|(index, byte)| (byte == b'\n').then_some(index + 1)),
    );
    starts
}

fn source_position(source: &str, starts: &[usize], byte: usize) -> Option<SourcePosition> {
    if byte > source.len() || !source.is_char_boundary(byte) {
        return None;
    }
    let line = starts
        .partition_point(|offset| *offset <= byte)
        .saturating_sub(1);
    Some(SourcePosition {
        line,
        byte: byte.checked_sub(*starts.get(line)?)?,
    })
}

fn source_range(source: &str, starts: &[usize], range: Range<usize>) -> Option<SourceRange> {
    if range.start >= range.end {
        return None;
    }
    Some(SourceRange {
        start: source_position(source, starts, range.start)?,
        end: source_position(source, starts, range.end)?,
    })
}

fn byte_range_to_lines(
    range: Range<usize>,
    source_len: usize,
    starts: &[usize],
) -> Option<(usize, usize)> {
    if range.start >= range.end || range.end > source_len || range.end == 0 {
        return None;
    }
    let start = starts
        .partition_point(|offset| *offset <= range.start)
        .saturating_sub(1);
    // Parser ranges are end-exclusive. Looking at end - 1 prevents a node ending
    // at column zero on the following line from swallowing that line.
    let inclusive_end_byte = range.end - 1;
    let end = starts
        .partition_point(|offset| *offset <= inclusive_end_byte)
        .saturating_sub(1);
    (start < end).then_some((start, end))
}

fn markdown_regions(
    source: &str,
    starts: &[usize],
    max_events: usize,
    budget: &mut WorkBudget,
) -> Option<Vec<RawRegion>> {
    let mut headings: Vec<(usize, HeadingLevel)> = Vec::new();
    let mut regions = Vec::new();
    let mut events = 0usize;
    for (event, range) in MarkdownParser::new_ext(source, Options::all()).into_offset_iter() {
        events = events.saturating_add(1);
        if events > max_events || !budget.step() {
            return None;
        }
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let line = starts
                    .partition_point(|offset| *offset <= range.start)
                    .saturating_sub(1);
                headings.push((line, level));
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((start_line, end_line)) =
                    byte_range_to_lines(range, source.len(), starts)
                {
                    regions.push(RawRegion {
                        start_line,
                        end_line,
                        kind: FoldKind::CodeBlock,
                    });
                }
            }
            _ => {}
        }
    }
    for (index, (start, level)) in headings.iter().copied().enumerate() {
        let next = headings[index + 1..]
            .iter()
            .find(|(_, candidate)| heading_rank(*candidate) <= heading_rank(level))
            .map_or(starts.len(), |(line, _)| *line);
        let end = next.saturating_sub(1).min(starts.len().saturating_sub(1));
        if start < end {
            regions.push(RawRegion {
                start_line: start,
                end_line: end,
                kind: FoldKind::Heading,
            });
        }
    }
    Some(regions)
}

const fn heading_rank(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[derive(Clone, Copy)]
#[allow(dead_code)] // Variants are only constructed when their grammar feature is enabled.
enum CodeLanguage {
    Rust,
    TypeScript,
    JavaScript,
    Python,
    Go,
}

#[cfg(test)]
fn code_regions(
    source: &str,
    starts: &[usize],
    language: Language,
    flavor: CodeLanguage,
    max_nodes: usize,
    budget: &mut WorkBudget,
) -> Option<Vec<RawRegion>> {
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(source, None)?;
    collect_code_regions(&tree, source.len(), starts, flavor, max_nodes, budget)
}

fn collect_code_regions(
    tree: &Tree,
    source_len: usize,
    starts: &[usize],
    flavor: CodeLanguage,
    max_nodes: usize,
    budget: &mut WorkBudget,
) -> Option<Vec<RawRegion>> {
    let mut regions = Vec::new();
    let mut stack: Vec<Node<'_>> = vec![tree.root_node()];
    let mut visited = 0usize;
    while let Some(node) = stack.pop() {
        visited = visited.saturating_add(1);
        if visited > max_nodes || !budget.step() {
            return None;
        }
        if !node.is_error()
            && !node.is_missing()
            && let Some(kind) = classify_node(flavor, node, budget)?
            && let Some((start_line, end_line)) =
                byte_range_to_lines(node.byte_range(), source_len, starts)
        {
            regions.push(RawRegion {
                start_line,
                end_line,
                kind,
            });
        }
        let child_count = node.child_count();
        for index in (0..child_count).rev() {
            if let Some(child) = node.child(index) {
                stack.push(child);
            }
        }
    }
    Some(regions)
}

fn classify_node(
    language: CodeLanguage,
    node: Node<'_>,
    budget: &mut WorkBudget,
) -> Option<Option<FoldKind>> {
    let kind = node.kind();
    let classified = match language {
        CodeLanguage::Rust => match kind {
            "function_item" => Some(
                if node
                    .parent()
                    .is_some_and(|parent| parent.kind() == "declaration_list")
                    && node
                        .parent()
                        .and_then(|parent| parent.parent())
                        .is_some_and(|parent| matches!(parent.kind(), "impl_item" | "trait_item"))
                {
                    FoldKind::Method
                } else {
                    FoldKind::Function
                },
            ),
            "struct_item" | "enum_item" | "union_item" | "trait_item" | "type_item"
            | "impl_item" => Some(FoldKind::Type),
            "mod_item" => Some(FoldKind::Module),
            _ => None,
        },
        CodeLanguage::Python => match kind {
            "function_definition" => Some(python_function_kind(node, budget)?),
            "class_definition" => Some(FoldKind::Type),
            _ => None,
        },
        CodeLanguage::Go => match kind {
            "function_declaration" => Some(FoldKind::Function),
            "method_declaration" => Some(FoldKind::Method),
            "type_declaration" => Some(FoldKind::Type),
            _ => None,
        },
        CodeLanguage::TypeScript | CodeLanguage::JavaScript => match kind {
            "function_declaration"
            | "function_expression"
            | "arrow_function"
            | "generator_function"
            | "generator_function_declaration" => Some(FoldKind::Function),
            "method_definition" | "abstract_method_signature" | "method_signature" => {
                Some(FoldKind::Method)
            }
            "class_declaration"
            | "class"
            | "abstract_class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "type_alias_declaration" => Some(FoldKind::Type),
            "internal_module" | "ambient_declaration" => Some(FoldKind::Module),
            _ => None,
        },
    };
    Some(classified)
}

fn python_function_kind(node: Node<'_>, budget: &mut WorkBudget) -> Option<FoldKind> {
    let mut ancestor = node.parent();
    while let Some(current) = ancestor {
        if !budget.step() {
            return None;
        }
        match current.kind() {
            // The nearest semantic owner decides classification. Decorators,
            // blocks, and control-flow nodes are deliberately transparent.
            "function_definition" | "lambda" => return Some(FoldKind::Function),
            "class_definition" => return Some(FoldKind::Method),
            _ => ancestor = current.parent(),
        }
    }
    Some(FoldKind::Function)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SymbolOwner {
    RustModule,
    RustTrait,
    RustOtherType,
    RustFunction,
    TypeScriptModule,
    TypeScriptType,
    TypeScriptFunction,
    TypeScriptMethod,
    JavaScriptClass,
    JavaScriptFunction,
    JavaScriptMethod,
    PythonClass,
    PythonFunction,
    GoRoot,
}

#[derive(Clone, Copy)]
struct SymbolDescriptor {
    kind: SymbolKind,
    owner: SymbolOwner,
}

#[derive(Clone, Copy)]
struct ActiveSymbol {
    id: SymbolId,
    owner: SymbolOwner,
}

enum SymbolVisit<'tree> {
    Enter(Node<'tree>),
    Exit(bool),
}

fn collect_code_symbols(
    tree: &Tree,
    source: &str,
    starts: &[usize],
    flavor: CodeLanguage,
    limits: StructureLimits,
    budget: &mut WorkBudget,
) -> Option<Vec<StructureSymbol>> {
    let mut symbols = Vec::new();
    let mut symbol_depths = Vec::new();
    let mut active = Vec::new();
    let mut stack = vec![SymbolVisit::Enter(tree.root_node())];
    let mut visited = 0usize;

    while let Some(event) = stack.pop() {
        match event {
            SymbolVisit::Exit(pushed) => {
                if pushed {
                    active.pop()?;
                }
            }
            SymbolVisit::Enter(node) => {
                visited = visited.saturating_add(1);
                if visited > limits.max_symbol_nodes || !budget.step() {
                    return None;
                }

                let descriptor = symbol_descriptor(flavor, node, &active);
                let mut pushed = false;
                if let Some(descriptor) = descriptor {
                    if node.is_error() || node.is_missing() {
                        return None;
                    }
                    // Anonymous declaration forms deliberately produce no
                    // symbol. A present but malformed name is a projection
                    // failure because returning a valid prefix would lie
                    // about completeness.
                    if let Some(name_node) = node.child_by_field_name("name") {
                        if name_node.is_error() || name_node.is_missing() {
                            return None;
                        }
                        let name = name_node.utf8_text(source.as_bytes()).ok()?.to_owned();
                        if name.is_empty() {
                            return None;
                        }
                        if symbols.len() >= limits.max_symbols {
                            return None;
                        }
                        let parent = symbol_parent(flavor, descriptor, &active);
                        let depth = parent.map_or(1usize, |parent| {
                            symbol_depths
                                .get(parent.0 as usize)
                                .copied()
                                .unwrap_or(usize::MAX)
                                .saturating_add(1)
                        });
                        if depth > limits.max_symbol_depth {
                            return None;
                        }
                        let range = source_range(source, starts, node.byte_range())?;
                        let selection_range = source_range(source, starts, name_node.byte_range())?;
                        if selection_range.start < range.start || selection_range.end > range.end {
                            return None;
                        }
                        let id = SymbolId(u32::try_from(symbols.len()).ok()?);
                        symbols.push(StructureSymbol {
                            id,
                            name,
                            kind: descriptor.kind,
                            range,
                            selection_range,
                            parent,
                            detail: None,
                            container: None,
                        });
                        symbol_depths.push(depth);
                        active.push(ActiveSymbol {
                            id,
                            owner: descriptor.owner,
                        });
                        pushed = true;
                    }
                }

                stack.push(SymbolVisit::Exit(pushed));
                for index in (0..node.child_count()).rev() {
                    if let Some(child) = node.child(index) {
                        stack.push(SymbolVisit::Enter(child));
                    }
                }
            }
        }
    }
    Some(symbols)
}

fn symbol_descriptor(
    flavor: CodeLanguage,
    node: Node<'_>,
    active: &[ActiveSymbol],
) -> Option<SymbolDescriptor> {
    let kind = node.kind();
    match flavor {
        CodeLanguage::Rust => match kind {
            "function_item" => Some(SymbolDescriptor {
                kind: if node
                    .parent()
                    .is_some_and(|parent| parent.kind() == "declaration_list")
                    && node
                        .parent()
                        .and_then(|parent| parent.parent())
                        .is_some_and(|parent| matches!(parent.kind(), "impl_item" | "trait_item"))
                {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                },
                owner: SymbolOwner::RustFunction,
            }),
            "trait_item" => Some(SymbolDescriptor {
                kind: SymbolKind::Type,
                owner: SymbolOwner::RustTrait,
            }),
            "struct_item" | "enum_item" | "union_item" | "type_item" => Some(SymbolDescriptor {
                kind: SymbolKind::Type,
                owner: SymbolOwner::RustOtherType,
            }),
            "mod_item" => Some(SymbolDescriptor {
                kind: SymbolKind::Module,
                owner: SymbolOwner::RustModule,
            }),
            _ => None,
        },
        CodeLanguage::TypeScript => match kind {
            "function_declaration" | "generator_function_declaration" => Some(SymbolDescriptor {
                kind: SymbolKind::Function,
                owner: SymbolOwner::TypeScriptFunction,
            }),
            "method_definition" | "abstract_method_signature" | "method_signature" => {
                Some(SymbolDescriptor {
                    kind: SymbolKind::Method,
                    owner: SymbolOwner::TypeScriptMethod,
                })
            }
            "class_declaration"
            | "abstract_class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "type_alias_declaration" => Some(SymbolDescriptor {
                kind: SymbolKind::Type,
                owner: SymbolOwner::TypeScriptType,
            }),
            "internal_module" => Some(SymbolDescriptor {
                kind: SymbolKind::Module,
                owner: SymbolOwner::TypeScriptModule,
            }),
            _ => None,
        },
        CodeLanguage::JavaScript => match kind {
            "function_declaration" | "generator_function_declaration" => Some(SymbolDescriptor {
                kind: SymbolKind::Function,
                owner: SymbolOwner::JavaScriptFunction,
            }),
            "method_definition" => Some(SymbolDescriptor {
                kind: SymbolKind::Method,
                owner: SymbolOwner::JavaScriptMethod,
            }),
            "class_declaration" => Some(SymbolDescriptor {
                kind: SymbolKind::Type,
                owner: SymbolOwner::JavaScriptClass,
            }),
            _ => None,
        },
        CodeLanguage::Python => match kind {
            "function_definition" => {
                let nearest_semantic_owner = active.iter().rev().find(|candidate| {
                    matches!(
                        candidate.owner,
                        SymbolOwner::PythonClass | SymbolOwner::PythonFunction
                    )
                });
                Some(SymbolDescriptor {
                    kind: if nearest_semantic_owner
                        .is_some_and(|candidate| candidate.owner == SymbolOwner::PythonClass)
                    {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Function
                    },
                    owner: SymbolOwner::PythonFunction,
                })
            }
            "class_definition" => Some(SymbolDescriptor {
                kind: SymbolKind::Type,
                owner: SymbolOwner::PythonClass,
            }),
            _ => None,
        },
        CodeLanguage::Go => match kind {
            "function_declaration" => Some(SymbolDescriptor {
                kind: SymbolKind::Function,
                owner: SymbolOwner::GoRoot,
            }),
            "method_declaration" => Some(SymbolDescriptor {
                kind: SymbolKind::Method,
                owner: SymbolOwner::GoRoot,
            }),
            "type_spec" => Some(SymbolDescriptor {
                kind: SymbolKind::Type,
                owner: SymbolOwner::GoRoot,
            }),
            _ => None,
        },
    }
}

fn symbol_parent(
    flavor: CodeLanguage,
    descriptor: SymbolDescriptor,
    active: &[ActiveSymbol],
) -> Option<SymbolId> {
    active
        .iter()
        .rev()
        .find(|candidate| match flavor {
            CodeLanguage::Rust => match descriptor.owner {
                SymbolOwner::RustFunction => matches!(
                    candidate.owner,
                    SymbolOwner::RustModule | SymbolOwner::RustTrait
                ),
                SymbolOwner::RustModule | SymbolOwner::RustTrait | SymbolOwner::RustOtherType => {
                    candidate.owner == SymbolOwner::RustModule
                }
                _ => false,
            },
            CodeLanguage::TypeScript => match descriptor.owner {
                SymbolOwner::TypeScriptModule => candidate.owner == SymbolOwner::TypeScriptModule,
                SymbolOwner::TypeScriptType
                | SymbolOwner::TypeScriptFunction
                | SymbolOwner::TypeScriptMethod => matches!(
                    candidate.owner,
                    SymbolOwner::TypeScriptModule | SymbolOwner::TypeScriptType
                ),
                _ => false,
            },
            CodeLanguage::JavaScript => match descriptor.owner {
                SymbolOwner::JavaScriptMethod => candidate.owner == SymbolOwner::JavaScriptClass,
                SymbolOwner::JavaScriptClass | SymbolOwner::JavaScriptFunction => matches!(
                    candidate.owner,
                    SymbolOwner::JavaScriptClass | SymbolOwner::JavaScriptFunction
                ),
                _ => false,
            },
            CodeLanguage::Python => matches!(
                candidate.owner,
                SymbolOwner::PythonClass | SymbolOwner::PythonFunction
            ),
            CodeLanguage::Go => false,
        })
        .map(|candidate| candidate.id)
}

#[derive(Default)]
struct MarkdownHeadingBuilder {
    level: Option<HeadingLevel>,
    line: usize,
    name_parts: Vec<String>,
    selection_start: Option<usize>,
    selection_end: Option<usize>,
}

struct MarkdownHeading {
    level: HeadingLevel,
    line: usize,
    name: String,
    selection: Option<Range<usize>>,
}

fn markdown_symbols(
    source: &str,
    starts: &[usize],
    limits: StructureLimits,
    budget: &mut WorkBudget,
) -> Option<Vec<StructureSymbol>> {
    let mut headings = Vec::new();
    let mut current: Option<MarkdownHeadingBuilder> = None;
    let mut events = 0usize;

    for (event, range) in MarkdownParser::new_ext(source, Options::all()).into_offset_iter() {
        events = events.saturating_add(1);
        if events > limits.max_markdown_symbol_events || !budget.step() {
            return None;
        }
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                if current.is_some() {
                    return None;
                }
                current = Some(MarkdownHeadingBuilder {
                    level: Some(level),
                    line: starts
                        .partition_point(|offset| *offset <= range.start)
                        .saturating_sub(1),
                    ..MarkdownHeadingBuilder::default()
                });
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some(heading) = current.as_mut() {
                    heading.name_parts.push(text.into_string());
                    heading.selection_start.get_or_insert(range.start);
                    heading.selection_end = Some(range.end);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(heading) = current.as_mut() {
                    heading.name_parts.push(" ".to_owned());
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                let heading = current.take()?;
                let name = heading
                    .name_parts
                    .join("")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                let selection = match (heading.selection_start, heading.selection_end) {
                    (Some(start), Some(end)) if start < end => Some(start..end),
                    (None, None) => None,
                    _ => return None,
                };
                headings.push(MarkdownHeading {
                    level: heading.level?,
                    line: heading.line,
                    name,
                    selection,
                });
            }
            _ => {}
        }
    }
    if current.is_some() {
        return None;
    }

    let mut symbols = Vec::new();
    let mut hierarchy: Vec<(u8, SymbolId)> = Vec::new();
    for (index, heading) in headings.iter().enumerate() {
        let rank = heading_rank(heading.level);
        while hierarchy
            .last()
            .is_some_and(|(candidate_rank, _)| *candidate_rank >= rank)
        {
            hierarchy.pop();
        }

        if heading.name.is_empty() {
            continue;
        }
        if symbols.len() >= limits.max_symbols || hierarchy.len() >= limits.max_symbol_depth {
            return None;
        }
        let selection = heading.selection.clone()?;
        let section_end = headings[index + 1..]
            .iter()
            .find(|candidate| heading_rank(candidate.level) <= rank)
            .map_or(source.len(), |candidate| starts[candidate.line]);
        let section_start = *starts.get(heading.line)?;
        let range = source_range(source, starts, section_start..section_end)?;
        let selection_range = source_range(source, starts, selection)?;
        if selection_range.start < range.start || selection_range.end > range.end {
            return None;
        }
        let id = SymbolId(u32::try_from(symbols.len()).ok()?);
        symbols.push(StructureSymbol {
            id,
            name: heading.name.clone(),
            kind: SymbolKind::Heading,
            range,
            selection_range,
            parent: hierarchy.last().map(|(_, id)| *id),
            detail: None,
            container: None,
        });
        hierarchy.push((rank, id));
    }
    Some(symbols)
}

fn collect_recognizable_tokens(
    tree: &Tree,
    source: &str,
    starts: &[usize],
    flavor: CodeLanguage,
    limits: StructureLimits,
    budget: &mut WorkBudget,
) -> Option<Vec<SourceRange>> {
    let mut byte_ranges = Vec::new();
    let mut stack = vec![tree.root_node()];
    let mut visited = 0usize;
    while let Some(node) = stack.pop() {
        visited = visited.saturating_add(1);
        if visited > limits.max_token_nodes || !budget.step() {
            return None;
        }

        if token_kind_is_allowlisted(flavor, node.kind())
            && node.is_named()
            && node.named_child_count() == 0
        {
            if node.is_error() || node.is_missing() || byte_ranges.len() >= limits.max_tokens {
                return None;
            }
            let range = node.byte_range();
            if range.start >= range.end
                || range.end > source.len()
                || !source.is_char_boundary(range.start)
                || !source.is_char_boundary(range.end)
            {
                return None;
            }
            byte_ranges.push((range.start, range.end));
        }

        for index in (0..node.child_count()).rev() {
            if let Some(child) = node.child(index) {
                stack.push(child);
            }
        }
    }

    byte_ranges.sort_unstable();
    byte_ranges.dedup();
    if byte_ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return None;
    }
    byte_ranges
        .into_iter()
        .map(|(start, end)| source_range(source, starts, start..end))
        .collect()
}

fn token_kind_is_allowlisted(flavor: CodeLanguage, kind: &str) -> bool {
    match flavor {
        CodeLanguage::Rust => matches!(kind, "identifier" | "type_identifier" | "field_identifier"),
        CodeLanguage::TypeScript | CodeLanguage::JavaScript => matches!(
            kind,
            "identifier"
                | "property_identifier"
                | "private_property_identifier"
                | "type_identifier"
                | "shorthand_property_identifier"
                | "shorthand_property_identifier_pattern"
        ),
        CodeLanguage::Python => kind == "identifier",
        CodeLanguage::Go => matches!(
            kind,
            "identifier" | "field_identifier" | "type_identifier" | "package_identifier"
        ),
    }
}

fn normalize_regions(
    mut raw: Vec<RawRegion>,
    lines: &[String],
    max_regions: usize,
) -> Vec<FoldRegion> {
    if raw.len() > max_regions {
        return Vec::new();
    }
    raw.retain(|region| region.start_line < region.end_line && region.end_line < lines.len());
    raw.sort_by_key(|region| (region.start_line, std::cmp::Reverse(region.end_line)));
    let mut normalized: Vec<RawRegion> = Vec::with_capacity(raw.len());
    let mut nesting: Vec<usize> = Vec::new();
    for region in raw {
        if normalized
            .last()
            .is_some_and(|previous| previous.start_line == region.start_line)
        {
            continue;
        }
        while nesting
            .last()
            .is_some_and(|index| normalized[*index].end_line < region.start_line)
        {
            nesting.pop();
        }
        if nesting
            .last()
            .is_some_and(|index| region.end_line > normalized[*index].end_line)
        {
            // Crossing ranges cannot be represented by a fold tree.
            continue;
        }
        let index = normalized.len();
        normalized.push(region);
        nesting.push(index);
    }

    let mut occurrences: HashMap<(FoldKind, u64), u32> = HashMap::new();
    normalized
        .into_iter()
        .map(|region| {
            let header_hash = stable_hash(lines[region.start_line].trim().as_bytes());
            let occurrence = occurrences.entry((region.kind, header_hash)).or_default();
            let anchor = FoldAnchor {
                kind: region.kind,
                header_hash,
                occurrence: *occurrence,
            };
            *occurrence = occurrence.saturating_add(1);
            FoldRegion {
                start_line: region.start_line,
                end_line: region.end_line,
                kind: region.kind,
                anchor,
            }
        })
        .collect()
}

fn stable_hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        FoldKind, FoldLimits, RawRegion, StructureLimits, SymbolKind, fold_regions_with_limits,
        normalize_regions, structure_snapshot, structure_snapshot_with_limits,
    };

    fn lines(source: &str) -> Vec<String> {
        source.lines().map(ToOwned::to_owned).collect()
    }

    fn token_text(source_lines: &[String], range: super::SourceRange) -> &str {
        assert_eq!(range.start.line, range.end.line);
        &source_lines[range.start.line][range.start.byte..range.end.byte]
    }

    #[test]
    fn markdown_headings_and_fences_are_nested() {
        let source = "# Top\nintro\n## Child\n```rs\nfn main() {}\n```\ntail";
        let regions = fold_regions_with_limits(
            Path::new("README.md"),
            &lines(source),
            FoldLimits::default(),
        );
        assert!(
            regions
                .iter()
                .any(|region| (region.start_line, region.end_line, region.kind)
                    == (0, 6, FoldKind::Heading))
        );
        assert!(
            regions
                .iter()
                .any(|region| (region.start_line, region.end_line, region.kind)
                    == (2, 6, FoldKind::Heading))
        );
        assert!(
            regions
                .iter()
                .any(|region| (region.start_line, region.end_line, region.kind)
                    == (3, 5, FoldKind::CodeBlock))
        );
    }

    #[test]
    fn nested_rust_and_python_functions_keep_method_classification_direct_only() {
        let rust = lines("impl Thing {\n fn method() {\n  fn nested() {\n  }\n }\n}");
        let regions = fold_regions_with_limits(Path::new("lib.rs"), &rust, FoldLimits::default());
        assert!(
            regions
                .iter()
                .any(|region| region.start_line == 1 && region.kind == FoldKind::Method)
        );
        assert!(
            regions
                .iter()
                .any(|region| region.start_line == 2 && region.kind == FoldKind::Function)
        );

        let python =
            lines("class Thing:\n def method(self):\n  def nested():\n   pass\n  return nested\n");
        let regions =
            fold_regions_with_limits(Path::new("thing.py"), &python, FoldLimits::default());
        assert!(
            regions
                .iter()
                .any(|region| region.start_line == 1 && region.kind == FoldKind::Method)
        );
        assert!(
            regions
                .iter()
                .any(|region| region.start_line == 2 && region.kind == FoldKind::Function)
        );
    }

    #[test]
    fn python_methods_follow_the_nearest_semantic_ancestor() {
        for (label, source, start_line, expected) in [
            (
                "decorated class method",
                "class Thing:\n @decorator\n def method(self):\n  pass",
                2,
                FoldKind::Method,
            ),
            (
                "conditional class method",
                "class Thing:\n if enabled:\n  def method(self):\n   pass",
                2,
                FoldKind::Method,
            ),
            (
                "nested local function",
                "def outer():\n def local():\n  pass\n return local",
                1,
                FoldKind::Function,
            ),
            (
                "nested class method",
                "def outer():\n class Inner:\n  def method(self):\n   pass\n return Inner",
                2,
                FoldKind::Method,
            ),
        ] {
            let regions = fold_regions_with_limits(
                Path::new("fixture.py"),
                &lines(source),
                FoldLimits::default(),
            );
            assert!(
                regions
                    .iter()
                    .any(|region| region.start_line == start_line && region.kind == expected),
                "{label}: {regions:?}"
            );
        }
    }

    #[test]
    fn supported_language_declarations_produce_semantic_regions() {
        for (path, source, expected) in [
            (
                "main.ts",
                "class Thing {\n method() {\n  return 1;\n }\n}",
                FoldKind::Type,
            ),
            (
                "main.js",
                "function run() {\n return true;\n}",
                FoldKind::Function,
            ),
            (
                "main.go",
                "package main\nfunc run() {\n println(1)\n}",
                FoldKind::Function,
            ),
        ] {
            let regions =
                fold_regions_with_limits(Path::new(path), &lines(source), FoldLimits::default());
            assert!(
                regions.iter().any(|region| region.kind == expected),
                "{path}: {regions:?}"
            );
        }
    }

    #[test]
    fn cancellation_and_caps_fail_closed() {
        let source = lines("fn main() {\n let x = 1;\n}");
        let mut limits = FoldLimits {
            cancel_after_work: Some(0),
            ..FoldLimits::default()
        };
        assert!(fold_regions_with_limits(Path::new("main.rs"), &source, limits).is_empty());
        limits = FoldLimits {
            max_nodes: 1,
            ..FoldLimits::default()
        };
        assert!(fold_regions_with_limits(Path::new("main.rs"), &source, limits).is_empty());
        limits = FoldLimits {
            max_regions: 0,
            ..FoldLimits::default()
        };
        assert!(fold_regions_with_limits(Path::new("main.rs"), &source, limits).is_empty());
    }

    #[test]
    fn exclusive_end_at_column_zero_and_error_descendants_are_safe() {
        let starts = super::line_starts("one\ntwo\nthree");
        assert_eq!(
            super::byte_range_to_lines(0..4, 13, &starts),
            None,
            "an exclusive end at the next line's column zero must not include it"
        );

        let broken = lines("fn still_folded() {\n let value = ;\n return value;\n}");
        let regions =
            fold_regions_with_limits(Path::new("broken.rs"), &broken, FoldLimits::default());
        assert!(regions.iter().any(|region| {
            region.start_line == 0 && region.end_line == 3 && region.kind == FoldKind::Function
        }));
    }

    #[test]
    fn normalization_keeps_widest_same_start_and_discards_crossing_before_anchors() {
        let source = lines("a\nb\nc\nd\ne");
        let regions = normalize_regions(
            vec![
                RawRegion {
                    start_line: 0,
                    end_line: 2,
                    kind: FoldKind::Function,
                },
                RawRegion {
                    start_line: 0,
                    end_line: 4,
                    kind: FoldKind::Type,
                },
                RawRegion {
                    start_line: 2,
                    end_line: 4,
                    kind: FoldKind::Function,
                },
                RawRegion {
                    start_line: 1,
                    end_line: 3,
                    kind: FoldKind::Method,
                },
            ],
            &source,
            10,
        );
        assert_eq!(regions.len(), 2);
        assert_eq!((regions[0].start_line, regions[0].end_line), (0, 4));
        assert_eq!((regions[1].start_line, regions[1].end_line), (1, 3));
    }

    #[test]
    fn code_symbols_follow_exact_language_mappings_and_hierarchy() {
        let rust = lines(
            "mod outer {\n trait Service {\n  fn required() {}\n }\n impl Thing {\n  fn method() {}\n }\n type Alias = usize;\n}\n",
        );
        let snapshot = structure_snapshot(Path::new("lib.rs"), &rust);
        assert!(snapshot.symbols_complete);
        let outer = snapshot
            .symbols
            .iter()
            .find(|symbol| symbol.name == "outer")
            .unwrap();
        let service = snapshot
            .symbols
            .iter()
            .find(|symbol| symbol.name == "Service")
            .unwrap();
        let required = snapshot
            .symbols
            .iter()
            .find(|symbol| symbol.name == "required")
            .unwrap();
        let method = snapshot
            .symbols
            .iter()
            .find(|symbol| symbol.name == "method")
            .unwrap();
        assert_eq!(outer.kind, SymbolKind::Module);
        assert_eq!(service.parent, Some(outer.id));
        assert_eq!(required.kind, SymbolKind::Method);
        assert_eq!(required.parent, Some(service.id));
        assert_eq!(method.kind, SymbolKind::Method);
        assert_eq!(method.parent, Some(outer.id));
        assert!(method.range.start < method.selection_range.start);
        assert!(method.selection_range.end <= method.range.end);

        let typescript = lines(
            "namespace Outer {\n interface Service { run(): void; }\n class Impl { method() {} }\n function execute() {}\n type Alias = string;\n}",
        );
        let snapshot = structure_snapshot(Path::new("main.ts"), &typescript);
        assert!(snapshot.symbols_complete);
        let namespace = snapshot
            .symbols
            .iter()
            .find(|symbol| symbol.name == "Outer")
            .unwrap();
        for (name, kind) in [
            ("Service", SymbolKind::Type),
            ("run", SymbolKind::Method),
            ("Impl", SymbolKind::Type),
            ("method", SymbolKind::Method),
            ("execute", SymbolKind::Function),
            ("Alias", SymbolKind::Type),
        ] {
            let symbol = snapshot
                .symbols
                .iter()
                .find(|symbol| symbol.name == name)
                .unwrap_or_else(|| panic!("missing {name}: {:?}", snapshot.symbols));
            assert_eq!(symbol.kind, kind);
            assert!(symbol.parent.is_some());
        }
        assert_eq!(namespace.kind, SymbolKind::Module);

        let python = lines(
            "class Outer:\n @decorator\n def method(self):\n  def local():\n   pass\n class Inner:\n  pass",
        );
        let snapshot = structure_snapshot(Path::new("main.py"), &python);
        let outer = &snapshot.symbols[0];
        let method = &snapshot.symbols[1];
        let local = &snapshot.symbols[2];
        let inner = &snapshot.symbols[3];
        assert_eq!(method.kind, SymbolKind::Method);
        assert_eq!(method.parent, Some(outer.id));
        assert_eq!(local.kind, SymbolKind::Function);
        assert_eq!(local.parent, Some(method.id));
        assert_eq!(inner.parent, Some(outer.id));

        let go =
            lines("package main\ntype Thing struct {}\nfunc (Thing) Run() {}\nfunc Start() {}");
        let snapshot = structure_snapshot(Path::new("main.go"), &go);
        assert!(snapshot.symbols_complete);
        assert_eq!(
            snapshot
                .symbols
                .iter()
                .map(|symbol| (symbol.name.as_str(), symbol.kind, symbol.parent))
                .collect::<Vec<_>>(),
            vec![
                ("Thing", SymbolKind::Type, None),
                ("Run", SymbolKind::Method, None),
                ("Start", SymbolKind::Function, None),
            ]
        );
    }

    #[test]
    fn anonymous_declarations_are_excluded_without_making_symbols_incomplete() {
        let source = lines(
            "const arrow = () => 1;\nconst expression = function() {};\nconst anonymous = class {};",
        );
        let snapshot = structure_snapshot(Path::new("main.js"), &source);
        assert!(snapshot.symbols_complete);
        assert!(snapshot.symbols.is_empty());
        assert!(snapshot.recognizable_tokens.complete);
        assert!(!snapshot.recognizable_tokens.ranges.is_empty());
    }

    #[test]
    fn markdown_symbols_collapse_names_and_preserve_rank_hierarchy() {
        let source =
            lines("# Top *bold* `code`\nintro\n## Child\nbody\n### Deep\ntext\n## Sibling\nend");
        let snapshot = structure_snapshot(Path::new("README.md"), &source);
        assert!(snapshot.symbols_complete);
        assert!(snapshot.recognizable_tokens.complete);
        assert!(snapshot.recognizable_tokens.ranges.is_empty());
        assert_eq!(
            snapshot
                .symbols
                .iter()
                .map(|symbol| (symbol.name.as_str(), symbol.kind, symbol.parent))
                .collect::<Vec<_>>(),
            vec![
                ("Top bold code", SymbolKind::Heading, None),
                ("Child", SymbolKind::Heading, Some(snapshot.symbols[0].id)),
                ("Deep", SymbolKind::Heading, Some(snapshot.symbols[1].id)),
                ("Sibling", SymbolKind::Heading, Some(snapshot.symbols[0].id)),
            ]
        );
        assert_eq!(snapshot.symbols[1].range.start.line, 2);
        assert_eq!(snapshot.symbols[1].range.end.line, 6);
        assert_eq!(snapshot.symbols[0].range.end.line, 7);
        assert_eq!(snapshot.symbols[0].range.end.byte, 3);
        assert_eq!(snapshot.symbols[0].selection_range.start.line, 0);
    }

    #[test]
    fn fold_symbol_and_token_projection_failures_are_isolated() {
        let source = lines("fn main() {\n let value = Thing::new();\n println!(\"{value:?}\");\n}");
        let baseline = structure_snapshot(Path::new("main.rs"), &source);
        assert!(!baseline.folds.is_empty());
        assert!(baseline.symbols_complete);
        assert!(baseline.recognizable_tokens.complete);

        let symbols_failed = structure_snapshot_with_limits(
            Path::new("main.rs"),
            &source,
            StructureLimits {
                max_symbol_nodes: 0,
                ..StructureLimits::default()
            },
        );
        assert!(!symbols_failed.symbols_complete);
        assert!(symbols_failed.symbols.is_empty());
        assert_eq!(symbols_failed.folds, baseline.folds);
        assert_eq!(
            symbols_failed.recognizable_tokens,
            baseline.recognizable_tokens
        );

        let tokens_failed = structure_snapshot_with_limits(
            Path::new("main.rs"),
            &source,
            StructureLimits {
                max_token_nodes: 0,
                ..StructureLimits::default()
            },
        );
        assert!(!tokens_failed.recognizable_tokens.complete);
        assert!(tokens_failed.recognizable_tokens.ranges.is_empty());
        assert_eq!(tokens_failed.folds, baseline.folds);
        assert_eq!(tokens_failed.symbols, baseline.symbols);
        assert!(tokens_failed.symbols_complete);

        let folds_failed = structure_snapshot_with_limits(
            Path::new("main.rs"),
            &source,
            StructureLimits {
                fold: FoldLimits {
                    max_nodes: 0,
                    ..FoldLimits::default()
                },
                ..StructureLimits::default()
            },
        );
        assert!(folds_failed.folds.is_empty());
        assert_eq!(folds_failed.symbols, baseline.symbols);
        assert_eq!(
            folds_failed.recognizable_tokens,
            baseline.recognizable_tokens
        );
    }

    #[test]
    fn recognizable_token_allowlists_include_typescript_shorthand_forms() {
        let source = lines(
            "const value = 1;\nconst object = { value };\nconst { object: alias, value: renamed = 2, shorthand } = input;",
        );
        let snapshot = structure_snapshot(Path::new("main.ts"), &source);
        assert!(snapshot.recognizable_tokens.complete);
        let token_texts = snapshot
            .recognizable_tokens
            .ranges
            .iter()
            .map(|range| token_text(&source, *range))
            .collect::<Vec<_>>();
        assert!(token_texts.contains(&"value"), "{token_texts:?}");
        assert!(token_texts.contains(&"shorthand"), "{token_texts:?}");
        assert!(token_texts.contains(&"alias"), "{token_texts:?}");

        let shorthand = snapshot
            .recognizable_tokens
            .ranges
            .iter()
            .copied()
            .find(|range| token_text(&source, *range) == "shorthand")
            .unwrap();
        assert_eq!(
            snapshot.recognizable_tokens.containing(shorthand.start),
            Some(shorthand)
        );
        assert_eq!(snapshot.recognizable_tokens.containing(shorthand.end), None);

        let exact_count = snapshot.recognizable_tokens.ranges.len();
        let exact = structure_snapshot_with_limits(
            Path::new("main.ts"),
            &source,
            StructureLimits {
                max_tokens: exact_count,
                ..StructureLimits::default()
            },
        );
        assert!(exact.recognizable_tokens.complete);
        let overflow = structure_snapshot_with_limits(
            Path::new("main.ts"),
            &source,
            StructureLimits {
                max_tokens: exact_count - 1,
                ..StructureLimits::default()
            },
        );
        assert!(!overflow.recognizable_tokens.complete);
        assert!(overflow.recognizable_tokens.ranges.is_empty());
    }

    #[test]
    fn recognizable_token_allowlists_cover_every_supported_family_and_zero_tokens() {
        for (path, source, expected) in [
            (
                "lib.rs",
                "fn run(input: Thing) { input.field; }",
                &["run", "input", "Thing", "field"][..],
            ),
            (
                "main.ts",
                "function run(input: Thing) { return input.field; }",
                &["run", "input", "Thing", "field"][..],
            ),
            (
                "main.py",
                "def run(value):\n return value.field",
                &["run", "value", "field"][..],
            ),
            (
                "main.go",
                "package sample\ntype Thing struct { Field int }\nfunc (value Thing) Run() { _ = value.Field }",
                &["sample", "Thing", "Field", "value", "Run"][..],
            ),
        ] {
            let source = lines(source);
            let snapshot = structure_snapshot(Path::new(path), &source);
            assert!(snapshot.recognizable_tokens.complete, "{path}");
            let token_texts = snapshot
                .recognizable_tokens
                .ranges
                .iter()
                .map(|range| token_text(&source, *range))
                .collect::<Vec<_>>();
            for expected in expected {
                assert!(token_texts.contains(expected), "{path}: {token_texts:?}");
            }
            assert!(
                snapshot
                    .recognizable_tokens
                    .ranges
                    .windows(2)
                    .all(|pair| pair[0].end <= pair[1].start),
                "{path}: {:?}",
                snapshot.recognizable_tokens.ranges
            );
        }

        let comment_only = lines("// no named source token");
        let snapshot = structure_snapshot(Path::new("empty.rs"), &comment_only);
        assert!(snapshot.recognizable_tokens.complete);
        assert!(snapshot.recognizable_tokens.ranges.is_empty());
    }

    #[test]
    fn markdown_symbol_budget_failure_keeps_folds_and_code_symbol_cap_fails_whole() {
        let markdown = lines("# Top\nbody\n## Child\ntext");
        let baseline = structure_snapshot(Path::new("README.md"), &markdown);
        let failed = structure_snapshot_with_limits(
            Path::new("README.md"),
            &markdown,
            StructureLimits {
                max_markdown_symbol_events: 0,
                ..StructureLimits::default()
            },
        );
        assert!(!failed.symbols_complete);
        assert!(failed.symbols.is_empty());
        assert_eq!(failed.folds, baseline.folds);
        assert!(failed.recognizable_tokens.complete);

        let code = lines("fn first() {}\nfn second() {}");
        let snapshot = structure_snapshot_with_limits(
            Path::new("lib.rs"),
            &code,
            StructureLimits {
                max_symbols: 1,
                ..StructureLimits::default()
            },
        );
        assert!(!snapshot.symbols_complete);
        assert!(snapshot.symbols.is_empty());
        assert!(snapshot.recognizable_tokens.complete);
    }
}
