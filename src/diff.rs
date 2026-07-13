#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiffLineKind {
    Metadata,
    Hunk,
    Context,
    Addition,
    Deletion,
    NoNewline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DiffLineAnnotation {
    pub kind: DiffLineKind,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
}

pub(crate) fn annotate_diff(lines: &[String]) -> Vec<DiffLineAnnotation> {
    let mut old_line = None;
    let mut new_line = None;
    lines
        .iter()
        .map(|line| {
            if let Some((old_start, new_start)) = parse_hunk_starts(line) {
                old_line = Some(old_start);
                new_line = Some(new_start);
                return annotation(DiffLineKind::Hunk, None, None);
            }

            let Some(old) = old_line else {
                return annotation(DiffLineKind::Metadata, None, None);
            };
            let Some(new) = new_line else {
                return annotation(DiffLineKind::Metadata, None, None);
            };

            match line.as_bytes().first() {
                Some(b' ') => {
                    old_line = Some(old.saturating_add(1));
                    new_line = Some(new.saturating_add(1));
                    annotation(DiffLineKind::Context, Some(old), Some(new))
                }
                Some(b'+') => {
                    new_line = Some(new.saturating_add(1));
                    annotation(DiffLineKind::Addition, None, Some(new))
                }
                Some(b'-') => {
                    old_line = Some(old.saturating_add(1));
                    annotation(DiffLineKind::Deletion, Some(old), None)
                }
                Some(b'\\') => annotation(DiffLineKind::NoNewline, None, None),
                _ => {
                    old_line = None;
                    new_line = None;
                    annotation(DiffLineKind::Metadata, None, None)
                }
            }
        })
        .collect()
}

pub(crate) fn line_number_width(annotations: &[DiffLineAnnotation]) -> usize {
    annotations
        .iter()
        .flat_map(|annotation| [annotation.old_line, annotation.new_line])
        .flatten()
        .max()
        .unwrap_or(1)
        .to_string()
        .len()
}

const fn annotation(
    kind: DiffLineKind,
    old_line: Option<usize>,
    new_line: Option<usize>,
) -> DiffLineAnnotation {
    DiffLineAnnotation {
        kind,
        old_line,
        new_line,
    }
}

fn parse_hunk_starts(line: &str) -> Option<(usize, usize)> {
    let mut fields = line.split_ascii_whitespace();
    if fields.next()? != "@@" {
        return None;
    }
    let old_start = parse_range_start(fields.next()?, '-')?;
    let new_start = parse_range_start(fields.next()?, '+')?;
    (fields.next()? == "@@").then_some((old_start, new_start))
}

fn parse_range_start(field: &str, prefix: char) -> Option<usize> {
    field.strip_prefix(prefix)?.split(',').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{DiffLineAnnotation, DiffLineKind, annotate_diff, line_number_width};

    #[test]
    fn annotates_unified_diff_with_old_and_new_line_numbers() {
        let lines = [
            "diff --git a/demo.txt b/demo.txt",
            "--- a/demo.txt",
            "+++ b/demo.txt",
            "@@ -8,3 +8,4 @@ heading",
            " unchanged",
            "-old",
            "+new",
            "+another",
            "\\ No newline at end of file",
        ]
        .map(str::to_owned);

        let annotations = annotate_diff(&lines);

        assert_eq!(
            annotations[4],
            DiffLineAnnotation {
                kind: DiffLineKind::Context,
                old_line: Some(8),
                new_line: Some(8),
            }
        );
        assert_eq!(annotations[5].old_line, Some(9));
        assert_eq!(annotations[5].new_line, None);
        assert_eq!(annotations[6].old_line, None);
        assert_eq!(annotations[6].new_line, Some(9));
        assert_eq!(annotations[7].new_line, Some(10));
        assert_eq!(annotations[8].kind, DiffLineKind::NoNewline);
        assert_eq!(line_number_width(&annotations), 2);
    }

    #[test]
    fn file_headers_are_not_mistaken_for_changes() {
        let lines = ["--- a/demo.txt", "+++ b/demo.txt"].map(str::to_owned);

        assert!(
            annotate_diff(&lines)
                .iter()
                .all(|annotation| annotation.kind == DiffLineKind::Metadata)
        );
    }
}
