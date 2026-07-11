# Latte Lens contributor notes

## Product boundary

Latte Lens is a read-only terminal repository viewer. It may inspect files and
invoke read-only Git commands, but it must not stage, reset, discard, or rewrite
the user's worktree.

## Stack and commands

- Rust 2024 edition; minimum supported Rust version is 1.88.
- Run locally with `cargo run -- /path/to/repository`; the installed binary is `latte-lens`.
- Before handing off changes, run `make ci`. Run `make coverage` when testable production logic changes.

## Module map

- `src/main.rs`: CLI parsing and terminal lifecycle only.
- `src/app.rs`: application state and input handling.
- `src/tree.rs`: filesystem traversal; keep Git ignore behavior intact.
- `src/git.rs`: the only module allowed to invoke Git.
- `src/preview.rs`: read-only preview provider contract, registry, and built-ins.
- `src/ui.rs`: rendering and Latte visual tokens.

## Design constraints

- Prefer the system Git executable over a native Git library unless a measured
  performance problem justifies changing the compatibility boundary.
- Keep UI rendering free of filesystem and subprocess work.
- Preview providers must respect request byte/line limits, decline unsupported
  files with `Ok(None)`, and never modify the selected file or repository.
- Syntax highlighting is best-effort decoration. Keep highlight byte ranges on
  UTF-8 boundaries, preserve plain-text fallback, and never change copied text.
- Keep PDF, Word, image, and other heavyweight format dependencies in optional
  providers rather than the core crate.
- Do not set a global or panel background color. Normal text and canvas cells
  must retain terminal `Color::Reset`; selected rows/tabs use modifiers.
- Mouse hit boxes are written by `ui::draw` and consumed by `App::handle_mouse`.
  Keep their layout calculations synchronized and cover changes with tests.
- New Git porcelain parsers require tests for spaces, renames, and non-UTF-8
  behavior where applicable.
- Do not require Nerd Fonts; enhanced glyphs must have a portable fallback.
