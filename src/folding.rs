use std::{collections::HashMap, ops::Range, path::Path};

use pulldown_cmark::{Event, HeadingLevel, Options, Parser as MarkdownParser, Tag, TagEnd};
use tree_sitter::{Language, Node, Parser, Tree};

pub(crate) const MAX_FOLD_NODES: usize = 100_000;
pub(crate) const MAX_MARKDOWN_EVENTS: usize = 50_000;
pub(crate) const MAX_FOLD_REGIONS: usize = 4_096;

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

pub(crate) fn fold_regions(path: &Path, lines: &[String]) -> Vec<FoldRegion> {
    fold_regions_with_limits(path, lines, FoldLimits::default())
}

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
        "rs" => code_regions(
            &source,
            &line_starts,
            tree_sitter_rust::LANGUAGE.into(),
            CodeLanguage::Rust,
            limits.max_nodes,
            &mut budget,
        ),
        "ts" | "mts" | "cts" => code_regions(
            &source,
            &line_starts,
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            CodeLanguage::TypeScript,
            limits.max_nodes,
            &mut budget,
        ),
        "tsx" => code_regions(
            &source,
            &line_starts,
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            CodeLanguage::TypeScript,
            limits.max_nodes,
            &mut budget,
        ),
        "js" | "mjs" | "cjs" | "jsx" => code_regions(
            &source,
            &line_starts,
            tree_sitter_javascript::LANGUAGE.into(),
            CodeLanguage::JavaScript,
            limits.max_nodes,
            &mut budget,
        ),
        "py" | "pyi" => code_regions(
            &source,
            &line_starts,
            tree_sitter_python::LANGUAGE.into(),
            CodeLanguage::Python,
            limits.max_nodes,
            &mut budget,
        ),
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
enum CodeLanguage {
    Rust,
    TypeScript,
    JavaScript,
    Python,
    Go,
}

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

    use super::{FoldKind, FoldLimits, RawRegion, fold_regions_with_limits, normalize_regions};

    fn lines(source: &str) -> Vec<String> {
        source.lines().map(ToOwned::to_owned).collect()
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
}
