use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
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
    Indexing {
        generation: u64,
    },
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
    Reindex,
    Shutdown,
}

#[derive(Default)]
struct CandidateSet {
    paths: Vec<PathBuf>,
    truncated: bool,
}

#[derive(Default)]
struct CandidateInventory {
    default: CandidateSet,
    include_ignored: CandidateSet,
}

impl CandidateInventory {
    fn candidates(&self, include_ignored: bool) -> &CandidateSet {
        if include_ignored {
            &self.include_ignored
        } else {
            &self.default
        }
    }
}

pub(crate) struct SearchRuntime {
    commands: Sender<SearchCommand>,
    event_tx: Sender<SearchEvent>,
    events: Receiver<SearchEvent>,
    latest_generation: Arc<AtomicU64>,
    inventory_epoch: Arc<AtomicU64>,
    inventory_ready: Arc<AtomicBool>,
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
        let inventory_epoch = Arc::new(AtomicU64::new(0));
        let inventory_ready = Arc::new(AtomicBool::new(false));
        let worker_generation = Arc::clone(&latest_generation);
        let worker_epoch = Arc::clone(&inventory_epoch);
        let worker_ready = Arc::clone(&inventory_ready);
        let worker_events = event_tx.clone();
        let worker = thread::Builder::new()
            .name("latte-lens-search".to_owned())
            .spawn(move || {
                search_loop(
                    root,
                    command_rx,
                    worker_events,
                    worker_generation,
                    worker_epoch,
                    worker_ready,
                )
            })?;
        Ok(Self {
            commands: command_tx,
            event_tx,
            events: event_rx,
            latest_generation,
            inventory_epoch,
            inventory_ready,
            worker: Some(worker),
        })
    }

    pub(crate) fn search(&self, request: SearchRequest) {
        self.latest_generation
            .store(request.generation, Ordering::Release);
        if !self.inventory_ready.load(Ordering::Acquire) {
            // The query is queued behind the current rebuild. This makes the
            // UI state explicit instead of searching an obsolete inventory.
            let _ = self.event_tx.send(SearchEvent::Indexing {
                generation: request.generation,
            });
        }
        let _ = self.commands.send(SearchCommand::Search(request));
    }

    pub(crate) fn refresh_inventory(&self) {
        self.inventory_ready.store(false, Ordering::Release);
        self.inventory_epoch.fetch_add(1, Ordering::AcqRel);
        let _ = self.commands.send(SearchCommand::Reindex);
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
    inventory_epoch: Arc<AtomicU64>,
    inventory_ready: Arc<AtomicBool>,
) {
    // Startup stays shallow: the first full candidate inventory is built only
    // after the user submits a text query.
    let mut inventory = CandidateInventory::default();
    // Keep the newest request until it has run against a ready inventory. A
    // refresh can arrive while an older refresh is rebuilding; dropping the
    // request in that gap leaves the UI waiting for an event that never comes.
    let mut pending_request = None;
    while let Ok(command) = commands.recv() {
        match command {
            SearchCommand::Search(request) => pending_request = Some(request),
            SearchCommand::Reindex => {
                inventory_ready.store(false, Ordering::Release);
            }
            SearchCommand::Shutdown => break,
        }
        while let Ok(command) = commands.try_recv() {
            match command {
                SearchCommand::Search(request) => pending_request = Some(request),
                SearchCommand::Reindex => {
                    inventory_ready.store(false, Ordering::Release);
                }
                SearchCommand::Shutdown => return,
            }
        }
        let Some(request) = pending_request.take() else {
            continue;
        };
        if latest_generation.load(Ordering::Acquire) != request.generation {
            continue;
        }
        let epoch = inventory_epoch.load(Ordering::Acquire);
        if !inventory_ready.load(Ordering::Acquire) {
            let _ = events.send(SearchEvent::Indexing {
                generation: request.generation,
            });
            inventory = build_candidate_inventory(&root);
            if inventory_epoch.load(Ordering::Acquire) != epoch {
                pending_request = Some(request);
                continue;
            }
            inventory_ready.store(true, Ordering::Release);
        }
        execute_search(
            &root,
            inventory.candidates(request.options.include_ignored),
            &request,
            &events,
            &latest_generation,
            &inventory_epoch,
            epoch,
        );
    }
}

fn build_candidate_inventory(root: &Path) -> CandidateInventory {
    CandidateInventory {
        default: build_candidate_set(root, false),
        include_ignored: build_candidate_set(root, true),
    }
}

fn build_candidate_set(root: &Path, include_ignored: bool) -> CandidateSet {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .parents(!include_ignored)
        .ignore(!include_ignored)
        .git_ignore(!include_ignored)
        .git_global(!include_ignored)
        .git_exclude(!include_ignored)
        .sort_by_file_path(|left, right| left.cmp(right))
        .filter_entry(|entry| entry.file_name() != ".git");

    let mut paths = Vec::new();
    let mut truncated = false;
    for walked in builder.build() {
        let Ok(entry) = walked else {
            continue;
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if paths.len() == MAX_SEARCH_FILES {
            truncated = true;
            break;
        }
        paths.push(entry.path().to_path_buf());
    }
    CandidateSet { paths, truncated }
}

fn execute_search(
    root: &Path,
    candidates: &CandidateSet,
    request: &SearchRequest,
    events: &Sender<SearchEvent>,
    latest_generation: &AtomicU64,
    inventory_epoch: &AtomicU64,
    expected_epoch: u64,
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

    let mut batch = Vec::with_capacity(BATCH_SIZE);
    let mut scanned_files = 0;
    let mut result_count = 0;
    let mut truncated = candidates.truncated;
    let worker_count = thread::available_parallelism()
        .map_or(1, usize::from)
        .min(8)
        .min(candidates.paths.len().max(1));
    let next_candidate = AtomicUsize::new(0);
    let (found_tx, found_rx) = mpsc::channel();
    thread::scope(|scope| {
        for _ in 0..worker_count {
            let found_tx = found_tx.clone();
            let matcher = &matcher;
            let paths = &candidates.paths;
            let next_candidate = &next_candidate;
            scope.spawn(move || {
                loop {
                    if latest_generation.load(Ordering::Acquire) != request.generation
                        || inventory_epoch.load(Ordering::Acquire) != expected_epoch
                    {
                        return;
                    }
                    let index = next_candidate.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = paths.get(index) else {
                        return;
                    };
                    let found =
                        search_file(root, path, matcher, MAX_SEARCH_RESULTS).unwrap_or_default();
                    if found_tx.send((index, found)).is_err() {
                        return;
                    }
                }
            });
        }
        drop(found_tx);
        let mut pending = BTreeMap::new();
        let mut next_index = 0;
        while let Ok((index, found)) = found_rx.recv() {
            if latest_generation.load(Ordering::Acquire) != request.generation
                || inventory_epoch.load(Ordering::Acquire) != expected_epoch
            {
                return;
            }
            pending.insert(index, found);
            while let Some(found) = pending.remove(&next_index) {
                scanned_files += 1;
                next_index += 1;
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
                    return;
                }
            }
        }
    });

    if latest_generation.load(Ordering::Acquire) != request.generation
        || inventory_epoch.load(Ordering::Acquire) != expected_epoch
    {
        return;
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
    fn startup_and_refresh_leave_full_inventory_work_until_a_query() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("fixture.rs"), "needle").unwrap();
        let runtime = SearchRuntime::start(root.path().to_path_buf()).unwrap();

        std::thread::sleep(Duration::from_millis(50));
        assert!(!runtime.inventory_ready.load(Ordering::Acquire));

        runtime.search(SearchRequest {
            generation: 1,
            query: "needle".to_owned(),
            options: SearchOptions::default(),
        });
        let _ = wait_for_finished(&runtime, 1);
        assert!(runtime.inventory_ready.load(Ordering::Acquire));

        runtime.refresh_inventory();
        std::thread::sleep(Duration::from_millis(50));
        assert!(!runtime.inventory_ready.load(Ordering::Acquire));
    }

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

    #[test]
    fn inventory_preserves_ignore_parity_and_lexical_paths() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(".gitignore"), "ignored/\n").unwrap();
        fs::create_dir_all(root.path().join("ignored")).unwrap();
        fs::create_dir_all(root.path().join(".git/objects")).unwrap();
        fs::write(root.path().join("z.rs"), "z").unwrap();
        fs::write(root.path().join("a.rs"), "a").unwrap();
        fs::write(root.path().join("ignored/hidden.rs"), "hidden").unwrap();
        fs::write(root.path().join(".git/objects/never.rs"), "never").unwrap();

        let inventory = build_candidate_inventory(root.path());
        let relative = |set: &CandidateSet| {
            set.paths
                .iter()
                .map(|path| path.strip_prefix(root.path()).unwrap().to_path_buf())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            relative(&inventory.default),
            vec![
                PathBuf::from(".gitignore"),
                PathBuf::from("a.rs"),
                PathBuf::from("z.rs")
            ]
        );
        assert_eq!(
            relative(&inventory.include_ignored),
            vec![
                PathBuf::from(".gitignore"),
                PathBuf::from("a.rs"),
                PathBuf::from("ignored/hidden.rs"),
                PathBuf::from("z.rs"),
            ]
        );
    }

    #[test]
    fn parallel_search_keeps_lexical_line_order_and_result_cap() {
        let root = tempfile::tempdir().unwrap();
        let first = (1..=MAX_SEARCH_RESULTS)
            .map(|line| format!("needle {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(root.path().join("a.rs"), first).unwrap();
        fs::write(root.path().join("z.rs"), "needle after cap").unwrap();

        let runtime = SearchRuntime::start(root.path().to_path_buf()).unwrap();
        runtime.search(SearchRequest {
            generation: 1,
            query: "needle".to_owned(),
            options: SearchOptions::default(),
        });
        let events = wait_for_finished(&runtime, 1);
        let matches = event_matches(&events);
        assert_eq!(matches.len(), MAX_SEARCH_RESULTS);
        assert!(matches.iter().all(|found| found.path == Path::new("a.rs")));
        assert_eq!(matches.first().unwrap().line_number, 1);
        assert_eq!(matches.last().unwrap().line_number, MAX_SEARCH_RESULTS);
        assert!(matches!(
            events.last(),
            Some(SearchEvent::Finished {
                truncated: true,
                scanned_files: 1,
                ..
            })
        ));
    }

    #[test]
    fn newer_generation_suppresses_queued_stale_search() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("fixture.rs"), "old new").unwrap();
        let runtime = SearchRuntime::start(root.path().to_path_buf()).unwrap();
        runtime.search(SearchRequest {
            generation: 1,
            query: "old".to_owned(),
            options: SearchOptions::default(),
        });
        runtime.search(SearchRequest {
            generation: 2,
            query: "new".to_owned(),
            options: SearchOptions::default(),
        });
        let events = wait_for_finished(&runtime, 2);
        assert!(!events.iter().any(|event| matches!(
            event,
            SearchEvent::Batch { generation: 1, .. } | SearchEvent::Finished { generation: 1, .. }
        )));
        assert_eq!(event_matches(&events)[0].line, "old new");
    }

    #[test]
    fn refresh_rebuilds_inventory_before_the_next_query() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("before.rs"), "before").unwrap();
        let runtime = SearchRuntime::start(root.path().to_path_buf()).unwrap();
        runtime.refresh_inventory();
        fs::write(root.path().join("after.rs"), "after").unwrap();
        runtime.search(SearchRequest {
            generation: 1,
            query: "after".to_owned(),
            options: SearchOptions::default(),
        });
        let events = wait_for_finished(&runtime, 1);
        let matches = event_matches(&events);
        assert_eq!(matches[0].path, Path::new("after.rs"));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, SearchEvent::Indexing { generation: 1 }))
        );
    }

    #[test]
    fn queued_query_survives_multiple_reindexes_and_uses_the_newest_inventory() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("before.rs"), "before").unwrap();
        let runtime = SearchRuntime::start(root.path().to_path_buf()).unwrap();

        runtime.refresh_inventory();
        fs::write(root.path().join("after.rs"), "needle").unwrap();
        runtime.search(SearchRequest {
            generation: 1,
            query: "needle".to_owned(),
            options: SearchOptions::default(),
        });
        runtime.refresh_inventory();

        let events = wait_for_finished(&runtime, 1);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, SearchEvent::Indexing { generation: 1 }))
        );
        assert_eq!(event_matches(&events)[0].path, Path::new("after.rs"));
    }

    #[test]
    #[ignore = "manual timing evidence; run with cargo test search_inventory_timing -- --ignored --nocapture"]
    fn search_inventory_timing() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..2_000 {
            fs::write(
                root.path().join(format!("fixture-{index:04}.rs")),
                "fn needle() {}\n",
            )
            .unwrap();
        }
        let inventory = build_candidate_inventory(root.path());
        let matcher = build_matcher("needle", SearchOptions::default()).unwrap();
        let started = std::time::Instant::now();
        for _ in 0..10 {
            // This is the pre-inventory equivalent: discover candidates with
            // the same walker, then match them for each query.
            let candidates = build_candidate_set(root.path(), false);
            for path in &candidates.paths {
                let _ = search_file(root.path(), path, &matcher, MAX_SEARCH_RESULTS);
            }
        }
        let traversal_and_match = started.elapsed();
        let started = std::time::Instant::now();
        for _ in 0..10 {
            for path in &inventory.default.paths {
                let _ = search_file(root.path(), path, &matcher, MAX_SEARCH_RESULTS);
            }
        }
        eprintln!(
            "ten traversal-equivalent discovery + match passes: {traversal_and_match:?}; \
             ten warm inventory-only match passes: {:?}",
            started.elapsed(),
        );
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
                SearchEvent::Indexing { .. } | SearchEvent::Finished { .. } => None,
            })
            .flatten()
            .collect()
    }
}
