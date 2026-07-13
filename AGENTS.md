# Latte Lens contributor guide

## 1. Project Overview

Latte Lens is a read-only terminal repository viewer. It may inspect files and
invoke read-only Git commands, but it must not stage, reset, discard, or rewrite
the user's worktree.

- Package: `latte-lens` `0.1.0-beta.2` (`Cargo.toml` is the version source of truth).
- Language: Rust 2024 edition on stable Rust, with MSRV 1.88.
- Terminal stack: Ratatui 0.30 over Crossterm, with Clap 4.5 for the CLI.
- Content stack: system Git for repository state, `ignore` and `regex` for search,
  and Syntect/Two-Face for syntax highlighting.
- Supported release targets: Linux x86_64/ARM64, macOS x86_64/Apple Silicon,
  and Windows x86_64. Windows ARM64 is not packaged.

Large workspaces are a normal operating condition, not a drive-root special
case. Startup enumerates at most the first two path components, deeper
directories load one level at a time when expanded, and each traversal is
bounded by 50,000 entries. Text-search inventory is built lazily after the first
text query. Preserve these bounded, non-blocking semantics for every workspace.

## 2. Project Structure Map

| Path | Responsibility |
| --- | --- |
| `src/main.rs` | CLI parsing, terminal startup, and terminal cleanup. |
| `src/app.rs` | Application state, input handling, selection, search state, and merging asynchronous results. |
| `src/runtime.rs` | Background filesystem/Git/preview work, request generations, queues, and stale-result rejection. |
| `src/tree.rs` | Bounded shallow scans and on-demand directory expansion for All Files. |
| `src/search.rs` | Lazy, cancellable search inventory and streaming file/text search. |
| `src/git.rs` | The system-Git process boundary and byte-preserving porcelain parsing. |
| `src/repo_graph.rs` | Nested-repository discovery, ownership, and repository relationships. |
| `src/content_safety.rs` | Non-following path inspection and safe regular-file opening. |
| `src/preview.rs` | Bounded preview-provider contract, registry, text preview, and syntax highlighting. |
| `src/ui.rs` | Pure Ratatui layout/rendering and mouse hit-region calculation. |
| `src/clipboard.rs` | Native clipboard adapters and OSC 52 fallback. |
| `tests/` | CLI, Git, tree, repository-graph, and full TUI integration tests. |
| `benches/tree_scan.rs` | Criterion benchmark for tree scanning. |
| `scripts/` | PTY E2E, installer tests, release builds, package verification, and release-note generation. |
| `.github/workflows/` | Push/PR quality gates and tag-driven cross-platform releases. |
| `docs/` | Preview-extension and search-performance contracts. |

Generated or local-only paths include `target/`, `dist/`, `log/`,
`.oh-my-code/`, `__pycache__/`, and `*.profraw`; do not treat them as source.

## 3. Build & Development Commands

Run commands from the repository root.

| Command | Use |
| --- | --- |
| `make setup` | Install rustfmt, Clippy, LLVM tools, and `cargo-llvm-cov`. |
| `cargo run -- /path/to/workspace` | Start Latte Lens against a directory; omit the path to use `.`. |
| `make build` | Build the debug binary with the lockfile. |
| `make ci` | Run the complete local handoff gate: formatting, check, lint, Rust tests, script tests, and PTY E2E. |
| `make coverage` | Enforce the default 80% line-coverage floor. |
| `make bench` | Run Criterion benchmarks. |
| `make release` | Build the optimized binary. |
| `make package-smoke` | Build and verify the current platform's release archive and checksum. |

Use the narrow targets while iterating: `make fmt`, `make fmt-check`,
`make check`, `make lint`, `make test`, `make script-test`, or `make e2e`.
Use `make clean` only when removing `target/` and `dist/` is intentional.

## 4. Testing Instructions

Before handing off code changes, run `make ci`. When testable production logic
changes, also run `make coverage`. Install its one-time dependency with
`make setup` if `cargo-llvm-cov` is unavailable.

Choose the smallest relevant test first, then widen:

```sh
# One integration test function
cargo test --locked --test app_tui_integration \
  every_workspace_starts_with_two_levels_and_loads_deeper_directories_on_expand \
  -- --nocapture

# One library unit test
cargo test --locked --lib parses_porcelain_v2_statuses_spaces_renames_and_submodules \
  -- --nocapture

# One installer/release test module
python3 -m unittest scripts/test_verify_release_package.py
```

Test behavior at the boundary being changed:

- Tree/startup/search changes: cover large arbitrary workspaces, scan caps,
  truncation, lazy expansion, cancellation, stale generations, and responsive
  rendering. All Files must continue to show ignored paths and dotfiles while
  excluding `.git`; text search respects ignore rules by default and can include
  ignored files explicitly. Do not limit regressions to drive-root fixtures.
- Git parsing changes: cover paths with spaces, renames/copies, submodules,
  staged/worktree combinations, and non-UTF-8 paths where the platform permits.
- UI/input changes: use Ratatui `TestBackend` integration tests and keep visual
  rows, mouse hitboxes, clipping, focus, selection, and copied text aligned.
- Content/preview changes: cover symlinks, special files, path races, byte/line
  limits, binary fallback, and unsupported providers.
- Installer/release changes: run `make script-test`; run `make package-smoke`
  when archive layout, checksums, or packaging changes.
- Real terminal behavior: run `make e2e`. The PTY harness is POSIX-only; CI runs
  it on Linux and macOS, while Windows runs locked check, test, and package
  verification without PTY coverage.

Do not weaken or delete timing/safety tests to make a change pass. Timing tests
must assert the intended invariant with enough headroom for CI variance.

## 5. Git Workflow

- The default branch is `main`; CI runs on pushes and pull requests.
- Use Conventional Commit subjects recognized by
  `scripts/generate-release-notes.sh`: `feat:`, `fix:`, `perf:`, `docs:`,
  `build:`, `ci:`, and `test:`. Scopes and `!` are supported.
- Keep commits scoped. Preserve unrelated user changes and inspect the complete
  worktree before staging or committing.
- Run `make ci` before handing off a commit. Include `make coverage` when
  production logic changed and can be covered.
- Do not invent a branch naming, merge, squash, reviewer, or approval policy;
  none is defined in this repository.
- `(CRITICAL)` Distinguish contributor Git operations from the product runtime:
  agents may commit only when explicitly asked, while Latte Lens itself may run
  only read-only Git commands.
- `(CRITICAL)` Never use destructive Git commands to discard worktree changes.

For releases, update both `Cargo.toml` and `Cargo.lock`, merge that version
change, then create a tag exactly equal to `v` plus the Cargo package version.
A version containing `-` becomes a GitHub prerelease. Do not push a release tag
or publish assets without explicit authorization.

## 6. Code Style Guidelines

Follow rustfmt and Clippy; `make fmt-check` and `make lint` deny drift and
warnings. Prefer small typed state transitions, explicit bounds, `Result` with
actionable `anyhow::Context`, saturating terminal arithmetic, and deterministic
ordering. Keep filesystem/process work out of rendering.

Open preview content through the safety gate:

```rust
// ✅ Preserves no-follow and regular-file checks.
let Some(file) = request.open_regular()? else {
    return Ok(None);
};

// ❌ Reopens an unchecked pathname and bypasses the safety contract.
let file = std::fs::File::open(request.absolute_path)?;
```

Decline unsupported preview formats and respect both limits:

```rust
// ✅ Lets the next provider try and reports bounded output honestly.
if bytes.contains(&0) {
    return Ok(None);
}
let content = PreviewContent::new(lines).with_truncated(was_capped);

// ❌ Reads an arbitrary file to exhaustion or claims an uncapped preview.
let bytes = std::fs::read(request.absolute_path)?;
```

Use the terminal's inherited background:

```rust
// ✅ Foreground-only styling works across terminal themes.
Style::default().fg(MINT)

// ❌ Global or panel backgrounds violate the UI contract.
Style::default().fg(MINT).bg(Color::Black)
```

Use ordinary Unicode/ASCII labels with graceful width handling; do not require
Nerd Fonts. When adding public APIs, include focused rustdoc for safety,
ordering, ownership, truncation, or fallback semantics that callers must obey.

## 7. Boundaries & Guardrails

### ✅ Always do

- Preserve the read-only product boundary and route product Git operations
  through the system Git CLI in `src/git.rs` with optional locks disabled.
- Keep slow filesystem, Git, search, and preview work in the background runtime;
  reject stale results by generation/epoch before mutating application state.
- Bound traversal and content work. Surface truncation/partial results instead
  of silently turning a large workspace into an unbounded scan.
- Keep repository discovery bounded (50,000 entries, 1,024 repositories, and
  depth 128 by default) and text search bounded (50,000 candidates and 1,000
  results). Entering Git Changes may request a full but still bounded graph.
- Use `PreviewRequest::open_regular()` and keep provider output within
  `max_bytes` and `max_lines`; return `Ok(None)` for unsupported content.
- Keep `PreviewContent.highlights.len()` aligned with `lines.len()`.
- Update tests and docs when changing user-visible behavior or a public contract.
- Preserve existing user changes and limit edits to the requested scope.

### ⚠️ Ask first

- Add or upgrade dependencies, raise the MSRV, change supported platforms, or
  alter the CLI/public preview API.
- Change the two-level startup policy, 50,000-entry caps, preview/search
  limits, cancellation semantics, or repository-discovery boundaries.
- Introduce a provider that bypasses `open_regular()` or delegates to an
  unbounded/blocking library or subprocess.
- Change package contents, installer trust/checksum behavior, release workflow,
  or version/tag state.
- Delete local/generated artifacts, make broad architecture changes, commit,
  push, or publish unless the user's request already authorizes that action.

### 🚫 Never do

- Stage, reset, checkout, clean, discard, rewrite, or otherwise modify the
  viewed user's repository from Latte Lens runtime code.
- Follow symlinks/reparse points or open FIFOs, sockets, devices, directories,
  or paths outside the selected content root for preview/search reads.
- Perform filesystem I/O or spawn subprocesses from `src/ui.rs` rendering.
- Add unbounded recursive traversal or eagerly build the text-search inventory
  during startup merely because the selected path is not a drive root.
- Set a global/panel background color or make Nerd Fonts a requirement.
- Hand-edit or commit generated artifacts from `target/`, `dist/`, `log/`,
  `.oh-my-code/`, `__pycache__/`, or `*.profraw`.
- Weaken safety, truncation, packaging-integrity, or read-only tests to obtain a
  passing build.

## 8. Related Documentation

- `README.md` — product behavior, controls, platform support, architecture,
  engineering commands, installation, and release flow.
- `docs/preview-providers.md` — preview-provider extension and content-safety
  contract.
- `docs/search-performance.md` — lazy inventory, Refresh snapshot semantics,
  and the ignored-file timing regression test.
- `LICENSE` — Apache License 2.0 terms.
