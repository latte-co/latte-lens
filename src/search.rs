use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use regex::{Regex, RegexBuilder};

use crate::content_safety::{OpenRegular, open_regular};

pub(crate) const MAX_SEARCH_RESULTS: usize = 1_000;
const MAX_SEARCH_FILES: usize = 50_000;
const MAX_SEARCH_FILE_BYTES: u64 = crate::preview::DEFAULT_MAX_BYTES as u64;
const MAX_RESULT_LINE_BYTES: usize = 500;
const BATCH_SIZE: usize = 50;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SearchOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
    pub include_ignored: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SearchMatch {
    pub path: PathBuf,
    pub line_number: usize,
    pub byte_range: std::ops::Range<usize>,
    pub summary_range: std::ops::Range<usize>,
    pub line: String,
}

#[derive(Clone, Debug)]
pub(crate) struct SearchRequest {
    pub generation: u64,
    pub query: String,
    pub options: SearchOptions,
}

#[derive(Debug)]
pub(crate) enum SearchEvent {
    Batch {
        generation: u64,
        matches: Vec<SearchMatch>,
        scanned_files: usize,
    },
    Finished {
        generation: u64,
        scanned_files: usize,
        truncated: bool,
        error: Option<String>,
    },
}

enum SearchCommand {
    Search(SearchRequest),
    Shutdown,
}

pub(crate) struct SearchRuntime {
    commands: Sender<SearchCommand>,
    events: Receiver<SearchEvent>,
    latest_generation: Arc<AtomicU64>,
    worker: Option<JoinHandle<()>>,
}

impl SearchRuntime {
    pub(crate) fn start(root: PathBuf) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("cannot open search root {}", root.display()))?;
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let latest_generation = Arc::new(AtomicU64::new(0));
        let worker_generation = Arc::clone(&latest_generation);
        let worker = thread::Builder::new()
            .name("latte-lens-search".to_owned())
            .spawn(move || search_loop(root, command_rx, event_tx, worker_generation))?;
        Ok(Self {
            commands: command_tx,
            events: event_rx,
            latest_generation,
            worker: Some(worker),
        })
    }

    pub(crate) fn search(&self, request: SearchRequest) {
        self.latest_generation
            .store(request.generation, Ordering::Release);
        let _ = self.commands.send(SearchCommand::Search(request));
    }

    pub(crate) fn cancel(&self, next_generation: u64) {
        self.latest_generation
            .store(next_generation, Ordering::Release);
    }

    pub(crate) fn take_events(&self) -> Vec<SearchEvent> {
        self.events.try_iter().collect()
    }
}

impl Drop for SearchRuntime {
    fn drop(&mut self) {
        self.latest_generation.store(u64::MAX, Ordering::Release);
        let _ = self.commands.send(SearchCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn search_loop(
    root: PathBuf,
    commands: Receiver<SearchCommand>,
    events: Sender<SearchEvent>,
    latest_generation: Arc<AtomicU64>,
) {
    while let Ok(command) = commands.recv() {
        let SearchCommand::Search(mut request) = command else {
            break;
        };
        while let Ok(command) = commands.try_recv() {
            match command {
                SearchCommand::Search(newer) => request = newer,
                SearchCommand::Shutdown => return,
            }
        }
        if latest_generation.load(Ordering::Acquire) != request.generation {
            continue;
        }
        execute_search(&root, &request, &events, &latest_generation);
    }
}

fn execute_search(
    root: &Path,
    request: &SearchRequest,
    events: &Sender<SearchEvent>,
    latest_generation: &AtomicU64,
) {
    let matcher = match build_matcher(&request.query, request.options) {
        Ok(matcher) => matcher,
        Err(error) => {
            let _ = events.send(SearchEvent::Finished {
                generation: request.generation,
                scanned_files: 0,
                truncated: false,
                error: Some(error.to_string()),
            });
            return;
        }
    };

    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .parents(!request.options.include_ignored)
        .ignore(!request.options.include_ignored)
        .git_ignore(!request.options.include_ignored)
        .git_global(!request.options.include_ignored)
        .git_exclude(!request.options.include_ignored)
        .sort_by_file_path(|left, right| left.cmp(right))
        .filter_entry(|entry| entry.file_name() != ".git");

    let mut batch = Vec::with_capacity(BATCH_SIZE);
    let mut scanned_files = 0;
    let mut result_count = 0;
    let mut truncated = false;

    for walked in builder.build() {
        if latest_generation.load(Ordering::Acquire) != request.generation {
            return;
        }
        let Ok(entry) = walked else {
            continue;
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if scanned_files == MAX_SEARCH_FILES {
            truncated = true;
            break;
        }
        scanned_files += 1;

        let Ok(found) = search_file(
            root,
            entry.path(),
            &matcher,
            MAX_SEARCH_RESULTS.saturating_sub(result_count),
        ) else {
            continue;
        };
        for found in found {
            batch.push(found);
            result_count += 1;
            if batch.len() == BATCH_SIZE {
                let matches = std::mem::replace(&mut batch, Vec::with_capacity(BATCH_SIZE));
                if events
                    .send(SearchEvent::Batch {
                        generation: request.generation,
                        matches,
                        scanned_files,
                    })
                    .is_err()
                {
                    return;
                }
            }
            if result_count == MAX_SEARCH_RESULTS {
                truncated = true;
                break;
            }
        }
        if result_count == MAX_SEARCH_RESULTS {
            break;
        }
    }

    if !batch.is_empty() {
        let _ = events.send(SearchEvent::Batch {
            generation: request.generation,
            matches: batch,
            scanned_files,
        });
    }
    let _ = events.send(SearchEvent::Finished {
        generation: request.generation,
        scanned_files,
        truncated,
        error: None,
    });
}

fn build_matcher(query: &str, options: SearchOptions) -> Result<Regex> {
    let mut pattern = if options.regex {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    if options.whole_word {
        pattern = format!(r"\b(?:{pattern})\b");
    }
    RegexBuilder::new(&pattern)
        .case_insensitive(!options.case_sensitive)
        .build()
        .context("invalid search expression")
}

fn search_file(
    root: &Path,
    path: &Path,
    matcher: &Regex,
    max_results: usize,
) -> Result<Vec<SearchMatch>> {
    let relative = path
        .strip_prefix(root)
        .context("search result escaped the workspace")?
        .to_path_buf();
    let mut file = match open_regular(Some(root), path)? {
        OpenRegular::Opened(file) if file.len() <= MAX_SEARCH_FILE_BYTES => file,
        OpenRegular::Opened(_) | OpenRegular::Declined(_) => return Ok(Vec::new()),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("cannot search {}", path.display()))?;
    if bytes.contains(&0) {
        return Ok(Vec::new());
    }
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Ok(Vec::new());
    };

    let mut matches = Vec::new();
    for (line_index, raw_line) in text
        .lines()
        .take(crate::preview::DEFAULT_MAX_LINES)
        .enumerate()
    {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let Some(found) = matcher.find(line) else {
            continue;
        };
        let source_range = found.range();
        let (summary, summary_range) = bounded_result_line(line, source_range.clone());
        matches.push(SearchMatch {
            path: relative.clone(),
            line_number: line_index + 1,
            byte_range: source_range,
            summary_range,
            line: summary,
        });
        if matches.len() == max_results {
            break;
        }
    }
    Ok(matches)
}

fn bounded_result_line(
    line: &str,
    range: std::ops::Range<usize>,
) -> (String, std::ops::Range<usize>) {
    if line.len() <= MAX_RESULT_LINE_BYTES {
        return (line.to_owned(), range);
    }
    let context = MAX_RESULT_LINE_BYTES / 2;
    let mut start = range.start.saturating_sub(context);
    while !line.is_char_boundary(start) {
        start += 1;
    }
    let mut end = start.saturating_add(MAX_RESULT_LINE_BYTES).min(line.len());
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    let mut summary = line[start..end].to_owned();
    let prefix = if start > 0 { '…'.len_utf8() } else { 0 };
    if start > 0 {
        summary.insert(0, '…');
    }
    if end < line.len() {
        summary.push('…');
    }
    (
        summary,
        range.start.saturating_sub(start).saturating_add(prefix)
            ..range.end.saturating_sub(start).saturating_add(prefix),
    )
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use super::*;

    #[test]
    fn search_respects_options_ignores_and_binary_safety() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(".gitignore"), "ignored/\n").unwrap();
        fs::create_dir(root.path().join("ignored")).unwrap();
        fs::write(root.path().join("visible.rs"), "Needle needle haystack\n").unwrap();
        fs::write(root.path().join("ignored/hidden.rs"), "needle\n").unwrap();
        fs::write(root.path().join("binary.bin"), b"needle\0data").unwrap();

        let runtime = SearchRuntime::start(root.path().to_path_buf()).unwrap();
        runtime.search(SearchRequest {
            generation: 1,
            query: "Needle".to_owned(),
            options: SearchOptions {
                case_sensitive: true,
                whole_word: true,
                ..SearchOptions::default()
            },
        });
        let events = wait_for_finished(&runtime, 1);
        let matches = event_matches(&events);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, Path::new("visible.rs"));

        runtime.search(SearchRequest {
            generation: 2,
            query: "needle".to_owned(),
            options: SearchOptions {
                include_ignored: true,
                ..SearchOptions::default()
            },
        });
        let events = wait_for_finished(&runtime, 2);
        let matches = event_matches(&events);
        assert_eq!(matches.len(), 2);
        assert!(
            matches
                .iter()
                .any(|found| found.path == Path::new("ignored/hidden.rs"))
        );
        assert!(
            !matches
                .iter()
                .any(|found| found.path == Path::new("binary.bin"))
        );
    }

    #[test]
    fn bounded_result_line_keeps_unicode_match_ranges_on_char_boundaries() {
        let line = format!("{}needle{}", "要".repeat(100), "尾".repeat(100));
        let start = "要".len() * 100;

        let (summary, range) = bounded_result_line(&line, start..start + "needle".len());

        assert_eq!(summary.get(range), Some("needle"));
    }

    #[test]
    fn invalid_regex_is_reported_without_scanning() {
        let root = tempfile::tempdir().unwrap();
        let runtime = SearchRuntime::start(root.path().to_path_buf()).unwrap();
        runtime.search(SearchRequest {
            generation: 1,
            query: "(".to_owned(),
            options: SearchOptions {
                regex: true,
                ..SearchOptions::default()
            },
        });
        let events = wait_for_finished(&runtime, 1);
        assert!(matches!(
            events.last(),
            Some(SearchEvent::Finished {
                scanned_files: 0,
                error: Some(_),
                ..
            })
        ));
    }

    fn wait_for_finished(runtime: &SearchRuntime, generation: u64) -> Vec<SearchEvent> {
        let mut events = Vec::new();
        for _ in 0..100 {
            events.extend(runtime.take_events());
            if events.iter().any(|event| {
                matches!(event, SearchEvent::Finished { generation: current, .. } if *current == generation)
            }) {
                return events;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("search did not finish");
    }

    fn event_matches(events: &[SearchEvent]) -> Vec<SearchMatch> {
        events
            .iter()
            .filter_map(|event| match event {
                SearchEvent::Batch { matches, .. } => Some(matches.clone()),
                SearchEvent::Finished { .. } => None,
            })
            .flatten()
            .collect()
    }
}
