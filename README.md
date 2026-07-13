# Latte Lens

> See what your agents are changing.

Latte Lens is a repository viewer designed for multi-agent terminals. It keeps
the working tree, source preview, and Git diff visible in one fast,
keyboard-driven TUI.

## Current prototype

- Collapsible repository tree that respects Git ignore rules
- Staged, unstaged, deleted, renamed, and untracked file status
- Colorized staged and working-tree diffs
- Numbered, syntax-highlighted previews for recognized source files, with plain-text fallback
- Extensible preview providers for formats such as PDF and Word
- Side-by-side tree and content panes, so context stays visible while reading
- A bounded file sidebar, single divider, and quiet focus/selection accents instead of boxed panels
- Left-pane tabs for the complete workspace tree or repository-grouped Git changes
- Keyboard and mouse navigation across scope tabs, tree, and content, with clickable controls and wheel scrolling
- Terminal-native background and default text colors
- Automatic refresh when entering Git Changes, plus keyboard and clickable manual refresh
- Graceful directory-tree mode outside Git repositories

## Run it

```bash
cargo run -- /path/to/repository
```

Install the command from this checkout, then run it from anywhere:

```bash
make install
latte-lens /path/to/repository
```

By default, Cargo installs `latte-lens` into `~/.cargo/bin`.

Inside the TUI:

| Key | Action |
| --- | --- |
| `↑` / `↓` | Move the focused tree or scroll focused content; `↑` at the first/empty tree row focuses scope tabs |
| `←` / `→` | Focus Tree or Content; while scope tabs are focused, select All Files or refresh/select Git Changes |
| `shift-←` / `shift-→` | Scroll Diff/Info horizontally; Preview wraps automatically |
| `j` / `k` | Move in the focused tree, or scroll the focused content pane |
| `1` / `2` | Show all files, or refresh and show only Git changes, while retaining focus |
| `tab` / `shift-tab` | Switch the left tree scope while retaining focus |
| `h` / `l` | Focus the tree or content pane |
| `enter` | Expand/collapse the selected repository/directory, or focus Content for a selected file/pointer diff |
| `/` / `ctrl-f` / `ctrl-shift-f` | Find files and directories, find in the current Preview, or search workspace text content |
| `p` / `d` | Show Preview or Diff in the right pane |
| `n` / `N` | Next or previous changed file in Diff |
| `ctrl-d` / `ctrl-u` | Page through content |
| `r` | Refresh repository state |
| `q` / `esc` | Press twice within 1.5 seconds to quit; `esc` closes an active search first |
| `ctrl-c` | Quit immediately when no content is selected; copy the current selection otherwise |

Mouse controls:

- Click `Files` or `Git changes` to switch the left tree dataset; entering Git Changes refreshes it first.
- Click `Refresh` in the header (or press `r`) to re-scan the repository without leaving the current view.
- Click `Find` or `Text` in the Files heading to open file or workspace text search. Search results preview on one click and reveal in Files on double click.
- In search, use `F2` for case sensitivity, `F3` for whole words, `F4` for regular expressions, and `F5` to include ignored content. `Enter` reveals a result and `Esc` restores the prior view.
- While searching, `Ctrl+P` switches to file search and `Ctrl+F` switches to text search.
- In a file Preview, `Ctrl+F` opens an in-preview find bar. `Enter`/`↓` and `Shift+Enter`/`↑` move between matches, `F2` toggles case sensitivity, and `Esc` closes it. The same controls are clickable. Use `Ctrl+Shift+F` for workspace text search.
- In Git Changes, click a repository or directory row to expand/collapse it; click a file or submodule-pointer row to open its owning-repository diff. All Files keeps its existing directory/file behavior.
- Click a pane to focus it, or use the wheel over either pane to navigate it.
- Drag the vertical divider to resize Tree and Preview/Diff. Tree keeps a 28-column minimum and the content pane keeps 24 columns.

All Files remains bounded by the selected workspace, includes dotfiles and
ignored paths, excludes only Git's internal `.git` metadata, and begins with
directories collapsed. Its bounded scan visits shallow directories first, so a
large generated subtree cannot hide later workspace roots when deeper results
become partial. Git Changes discovers repositories below that boundary,
groups each visible repository under a selectable header, and shows only its
changed files and required directories. Repository and Git-change directories default
expanded; clean irrelevant leaves are hidden, while relationship, submodule,
placeholder, partial-discovery, and isolated repository-error states remain
visible. Expansion and repo+row selection identities persist across successful
refreshes. Dirty repository headers use a quiet warm dot and label, while clean
repositories stay muted for fast scanning. In both scopes `p` and `d`
explicitly switch the right-side content.
The focused panel uses a
lavender dot and title, the selected tree row uses a slim accent rail, and the
footer begins with `Tabs`, `Tree`, or `Content`. These cues use terminal text
styles without painting a background or enclosing each pane in a box.
Latte Lens does not paint its own canvas background, so it follows the host
terminal theme—including embedded terminals such as herdr.

Filesystem traversal is capped at 50,000 entries to keep refreshes bounded.
When that cap omits additional paths, both tree scopes show an entry count with
`+` and `PARTIAL`; empty partial results are described as partial rather than
as a complete or clean repository view.

## Stack

- **Rust** for a small, fast, single-binary terminal tool
- **Ratatui + Crossterm** for rendering and terminal input
- **System Git CLI** as the compatibility boundary for worktrees, user config,
  diff drivers, and future Git features
- **ignore** for fast bounded filesystem walking with filters disabled in All Files
- **regex** for bounded, cancellable workspace text search

Repository discovery, Git status, tree scans, diffs, and previews run on a
dedicated background worker. Text searches use a separate cancellable worker
so a large query cannot block refresh or preview. The event loop only applies
the latest requested generation, so stale refreshes, selections, or searches
cannot replace newer state. Diff and Git-change preview requests carry their
owning repository identity, including rename/copy source paths.
The terminal UI renders immediately at startup while the first file-tree and
repository snapshot loads in the background; its loading state remains
interactive, and the completed snapshot replaces it without restarting the UI.
File watching is not implemented; entering Git Changes or pressing `r`
requests a fresh graph-aware snapshot.

## Platform support

| Validation surface | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Locked compile and unit/integration tests | CI | Not currently covered | CI |
| Native release build, package, and SHA-256 checksum | CI (`.tar.gz`) | CI (`.tar.gz`) | CI (`.zip` containing `latte-lens.exe`) |
| Interactive PTY E2E | CI (POSIX PTY) | CI (POSIX PTY) | Not currently covered |

Windows CI covers the supported build, unit/integration test, and packaging
surface. The current interactive E2E harness uses POSIX PTY APIs, so it is not
used as Windows evidence. ConPTY-based interactive E2E remains out of scope and
should be considered experimental until it is implemented and continuously
validated.

## Architecture

```text
src/main.rs   CLI entry point and terminal lifecycle
src/app.rs    application state, focus, and keyboard interaction
src/runtime.rs bounded background I/O worker and generation state
src/tree.rs   ignore-aware working-tree scan
src/git.rs    Git status and diff boundary
src/repo_graph.rs bounded repository discovery, relationships, and owning-repo routing
src/preview.rs extensible preview registry and built-in text provider
src/clipboard.rs native clipboard commands with an OSC 52 terminal fallback
src/ui.rs     Latte-styled Ratatui rendering
```

## Preview providers

Clean text and code files open in preview mode automatically. Changed files
open in diff mode; press `p` to inspect their current source or `d` to return to
the diff. Preview reads are capped by both bytes and lines. Content previews
never follow symbolic links and decline FIFOs, sockets, devices, directories,
and Windows reparse points before provider dispatch.

Preview text wraps to the current pane width. A logical source line keeps one
line number; wrapped continuation rows leave the number gutter blank. Scrolling
and mouse selection follow the visual rows, while copied text preserves the
original logical line without inserting display-only newlines. Diff remains
unwrapped so horizontal structure and patch prefixes stay exact.

Recognized source files highlight comments, strings, keywords, functions,
types, numbers, constants, and attributes. The bundled grammar set includes
TypeScript and TSX alongside common systems, scripting, web, configuration,
and documentation formats. Language detection uses the file name or extension
and falls back to a shebang when available. Highlighting is best-effort
decoration: unknown languages, parser failures, and unusually long lines remain
readable as plain text. Styles change foregrounds and modifiers only, so the
terminal continues to own the canvas background.

Drag across text in the right-hand content pane to create a visible selection
and copy it when the mouse is released. With a selection, `Ctrl+C` (or `Cmd+C`
when the terminal forwards it) copies the selection again; `Ctrl+Shift+C` is
also accepted. Without a selection, `Ctrl+C` exits immediately as a conventional
terminal interrupt. `q` and `Esc` require a second matching press within 1.5
seconds, so a stray navigation key cannot close the application.
Preview, Diff, and informational content all
support selection. Line-number gutters are excluded from copied previews,
multi-line selections preserve newlines, and Unicode grapheme clusters remain
intact. By default Latte Lens writes both the native platform clipboard and an
OSC 52 terminal clipboard sequence, which keeps copying reliable inside nested
terminals and isolated sessions such as Herdr. A native clipboard success is
reported as copied; when only OSC 52 is available, Latte Lens instead reports
that the text was sent to the terminal clipboard because terminals do not
acknowledge whether they accepted the sequence.

Optional formats stay outside the core binary. Implement the public
`PreviewProvider` trait and register it with `App::register_preview_provider` or
inject a `PreviewRegistry` through `App::with_preview_registry`. Providers
registered later have higher priority, so a PDF or Word provider can override
the built-in text detector without changing the application or UI.

See [docs/preview-providers.md](docs/preview-providers.md) for the provider
contract and an integration example.

## Engineering commands

Run `make help` to see the complete command list. The common quality loop is:

```bash
make ci
```

It runs formatting, type checks, strict Clippy, unit and integration tests, then
launches the real TUI in a pseudo-terminal for an end-to-end acceptance check.

Additional commands:

```bash
make coverage       # enforce at least 80% line coverage
make coverage-html  # generate an inspectable HTML report
make bench          # run performance benchmarks
make package        # create a release archive and SHA-256 checksum
make package-smoke  # build and verify the archive payload and checksum
```

`make setup` installs the optional local coverage command. CI runs the quality
gate on Linux, the POSIX PTY E2E test on Linux and macOS, checks Rust 1.88
compatibility, enforces the 80% line-coverage floor, and validates release
packages on Linux, macOS, and Windows.

## Publishing a release

Pushing a version tag creates a GitHub Release automatically. The release
workflow runs the Linux quality gate, verifies the native Linux, macOS, and
Windows packages, uploads every package and its SHA-256 sidecar, then generates
text release notes from the commits since the previous published release and
lists the contributors to those commits. It also adds a `SHA256SUMS.txt`
manifest covering every downloadable archive.

The tag must exactly match the version in `Cargo.toml`, with a `v` prefix:

```bash
# First update Cargo.toml to version = "0.1.1" and merge that change.
git tag v0.1.1
git push origin v0.1.1
```

Tags with a pre-release suffix, such as `v0.2.0-beta.1`, are published as
GitHub pre-releases rather than as the latest stable release.

## Next milestones

1. Background filesystem watching and diff cache
2. Syntax-aware diffs and word-level change highlights
3. Agent attribution: show which agent changed each file or hunk
4. Worktree and session switching for multi-agent workflows

## License

Apache-2.0
