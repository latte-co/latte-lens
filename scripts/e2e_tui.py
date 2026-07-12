#!/usr/bin/env python3
"""Exercise the real Latte Lens terminal UI inside an isolated Git repository."""

from __future__ import annotations

import fcntl
import os
import pty
import select
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unicodedata
from pathlib import Path


TIMEOUT_SECONDS = 10
OLD_CANVAS_ESCAPE = b"48;2;12;10;9"


class TerminalScreen:
    """Small, streaming terminal screen model for the ANSI emitted by Ratatui.

    Ratatui's backend updates only cells that changed.  A transcript with SGR
    sequences stripped therefore is not a screen: a redraw can write the new
    ``▾`` glyph for a directory without rewriting its unchanged name. This
    model follows cursor movement and keeps the current bounded terminal
    contents, which is what a person (and the PTY) actually sees.

    The captured Crossterm stream uses CSI cursor-position, mode, and SGR
    commands. Styles and mouse modes do not change visible cells; cursor
    positions do. Keeping that distinction here avoids a terminal-emulator
    dependency while still observing the real terminal state.
    """

    def __init__(self, columns: int, rows: int) -> None:
        self.columns = columns
        self.rows = rows
        self.cursor_column = 0
        self.cursor_row = 0
        self.cells = self._blank_cells()
        self._text = bytearray()
        self._escape_kind: str | None = None
        self._sequence = bytearray()

    def feed(self, data: bytes) -> None:
        for byte in data:
            if self._escape_kind is not None:
                self._consume_escape(byte)
            elif byte == 0x1B:
                self._flush_text(final=True)
                self._escape_kind = "intro"
            elif byte < 0x20 or byte == 0x7F:
                self._flush_text(final=True)
                self._consume_control(byte)
            else:
                self._text.append(byte)
                self._flush_text()

    def text(self) -> str:
        return "\n".join("".join(row).rstrip() for row in self.cells)

    def _blank_cells(self) -> list[list[str]]:
        return [[" "] * self.columns for _ in range(self.rows)]

    def _flush_text(self, final: bool = False) -> None:
        if not self._text:
            return

        raw = bytes(self._text)
        try:
            decoded = raw.decode("utf-8")
        except UnicodeDecodeError as error:
            if not final and error.reason == "unexpected end of data":
                if error.start == 0:
                    return
                decoded = raw[: error.start].decode("utf-8")
                del self._text[: error.start]
            else:
                decoded = raw.decode("utf-8", errors="replace")
                self._text.clear()
        else:
            self._text.clear()

        for char in decoded:
            self._write(char)

    def _consume_control(self, byte: int) -> None:
        if byte == 0x08:  # Backspace
            self.cursor_column = max(0, self.cursor_column - 1)
        elif byte == 0x09:  # Horizontal tab
            self.cursor_column = min(self.columns - 1, (self.cursor_column // 8 + 1) * 8)
        elif byte in (0x0A, 0x0B, 0x0C):  # LF, VT, FF
            self._line_feed()
        elif byte == 0x0D:  # Carriage return
            self.cursor_column = 0

    def _consume_escape(self, byte: int) -> None:
        if self._escape_kind == "intro":
            if byte == ord("["):
                self._escape_kind = "csi"
                self._sequence.clear()
            elif byte == ord("]"):
                self._escape_kind = "osc"
                self._sequence.clear()
            else:
                self._escape_kind = None
            return

        if self._escape_kind == "osc":
            if byte == 0x07:  # BEL terminates the OSC 52 clipboard sequence.
                self._escape_kind = None
            return

        self._sequence.append(byte)
        if 0x40 <= byte <= 0x7E:
            self._execute_csi(bytes(self._sequence))
            self._escape_kind = None

    def _execute_csi(self, sequence: bytes) -> None:
        final = chr(sequence[-1])
        parameters = sequence[:-1].decode("ascii", errors="ignore")

        if final in ("H", "f"):
            values = self._parameters(parameters, default=1)
            self.cursor_row = min(self.rows - 1, max(0, values[0] - 1))
            column = values[1] if len(values) > 1 else 1
            self.cursor_column = min(self.columns - 1, max(0, column - 1))
        elif final == "J":
            self._erase_display(self._parameters(parameters, default=0)[0])
        elif final == "K":
            self._erase_line(self._parameters(parameters, default=0)[0])
        elif final == "h" and 1049 in self._parameters(parameters, default=0):
            # EnterAlternateScreen resets the canvas before Ratatui's first draw.
            self._reset()
        # SGR, mouse modes, and unsupported queries do not alter visible cells.

    @staticmethod
    def _parameters(parameters: str, default: int) -> list[int]:
        values: list[int] = []
        for value in parameters.lstrip("?><!=").split(";"):
            try:
                values.append(int(value) if value else default)
            except ValueError:
                values.append(default)
        return values or [default]

    def _reset(self) -> None:
        self.cells = self._blank_cells()
        self.cursor_column = 0
        self.cursor_row = 0

    def _write(self, char: str) -> None:
        width = self._character_width(char)
        if width == 0:
            return
        if self.cursor_column >= self.columns:
            self.cursor_column = 0
            self._line_feed()
        if width == 2 and self.cursor_column == self.columns - 1:
            self.cells[self.cursor_row][self.cursor_column] = " "
            self.cursor_column = 0
            self._line_feed()

        self.cells[self.cursor_row][self.cursor_column] = char
        for offset in range(1, width):
            if self.cursor_column + offset < self.columns:
                self.cells[self.cursor_row][self.cursor_column + offset] = " "
        self.cursor_column += width

    @staticmethod
    def _character_width(char: str) -> int:
        if unicodedata.combining(char):
            return 0
        return 2 if unicodedata.east_asian_width(char) in ("F", "W") else 1

    def _line_feed(self) -> None:
        if self.cursor_row < self.rows - 1:
            self.cursor_row += 1
        else:
            del self.cells[0]
            self.cells.append([" "] * self.columns)

    def _erase_display(self, mode: int) -> None:
        if mode in (2, 3):
            self.cells = self._blank_cells()
        elif mode == 1:
            for row in range(self.cursor_row + 1):
                end = self.cursor_column + 1 if row == self.cursor_row else self.columns
                self.cells[row][:end] = [" "] * end
        else:
            for row in range(self.cursor_row, self.rows):
                start = self.cursor_column if row == self.cursor_row else 0
                self.cells[row][start:] = [" "] * (self.columns - start)

    def _erase_line(self, mode: int) -> None:
        if mode == 1:
            self.cells[self.cursor_row][: self.cursor_column + 1] = [" "] * (
                self.cursor_column + 1
            )
        elif mode == 2:
            self.cells[self.cursor_row] = [" "] * self.columns
        else:
            self.cells[self.cursor_row][self.cursor_column :] = [" "] * (
                self.columns - self.cursor_column
            )

def run(*args: str, cwd: Path) -> None:
    subprocess.run(args, cwd=cwd, check=True, capture_output=True)


def create_fixture(root: Path) -> None:
    run("git", "init", "-q", "-b", "main", cwd=root)
    run("git", "config", "user.name", "Latte Lens E2E", cwd=root)
    run("git", "config", "user.email", "e2e@latte.invalid", cwd=root)

    clean = root / "z-clean.rs"
    changed = root / "a-dir" / "nested" / "b-changed.rs"
    changed.parent.mkdir(parents=True, exist_ok=True)
    clean.write_text("pub fn clean() {}\n", encoding="utf-8")
    changed.write_text("pub fn before() {}\n", encoding="utf-8")
    run("git", "add", "z-clean.rs", "a-dir/nested/b-changed.rs", cwd=root)
    run("git", "commit", "-q", "-m", "fixture", cwd=root)

    changed.write_text("pub fn changed() {}\n", encoding="utf-8")

    nested = root / "vendor" / "nested"
    nested.mkdir(parents=True)
    run("git", "init", "-q", "-b", "main", cwd=nested)
    run("git", "config", "user.name", "Latte Lens E2E", cwd=nested)
    run("git", "config", "user.email", "e2e@latte.invalid", cwd=nested)
    nested_file = nested / "nested-owned.txt"
    nested_file.write_text("nested before\n", encoding="utf-8")
    run("git", "add", "nested-owned.txt", cwd=nested)
    run("git", "commit", "-q", "-m", "nested fixture", cwd=nested)
    nested_file.write_text("nested changed\n", encoding="utf-8")


def read_available(master_fd: int, output: bytearray, screen: TerminalScreen) -> None:
    # Keep the raw stream for terminal-protocol checks and update the screen
    # model incrementally: a redraw and its ANSI sequences can be split across
    # arbitrary PTY reads.
    while True:
        ready, _, _ = select.select([master_fd], [], [], 0)
        if not ready:
            return
        try:
            chunk = os.read(master_fd, 65_536)
        except OSError:
            return
        if not chunk:
            return
        output.extend(chunk)
        screen.feed(chunk)


def wait_for_raw_markers(
    process: subprocess.Popen[bytes],
    master_fd: int,
    output: bytearray,
    markers: tuple[bytes, ...],
    label: str,
    start: int = 0,
    screen: TerminalScreen | None = None,
) -> None:
    if screen is None:
        raise ValueError("a terminal screen is required while draining the PTY")
    deadline = time.monotonic() + TIMEOUT_SECONDS
    while time.monotonic() < deadline:
        read_available(master_fd, output, screen)
        window = output[start:]
        if all(marker in window for marker in markers):
            return
        if process.poll() is not None:
            break
        time.sleep(0.05)

    window = output[start:]
    missing = [marker.decode() for marker in markers if marker not in window]
    tail = bytes(output[-2_000:])
    raise AssertionError(
        f"{label} is missing: {', '.join(missing)}; terminal tail={tail!r}"
    )


def wait_for_screen_markers(
    process: subprocess.Popen[bytes],
    master_fd: int,
    output: bytearray,
    screen: TerminalScreen,
    markers: tuple[str, ...],
    label: str,
    absent: tuple[str, ...] = (),
) -> None:
    """Wait until the bounded current screen, not the historical stream, matches."""

    deadline = time.monotonic() + TIMEOUT_SECONDS
    while time.monotonic() < deadline:
        read_available(master_fd, output, screen)
        rendered = screen.text()
        missing = [marker for marker in markers if marker not in rendered]
        visible = [marker for marker in absent if marker in rendered]
        if not missing and not visible:
            return
        if process.poll() is not None:
            break
        time.sleep(0.05)

    rendered = screen.text()
    missing = [marker for marker in markers if marker not in rendered]
    visible = [marker for marker in absent if marker in rendered]
    details: list[str] = []
    if missing:
        details.append(f"missing: {', '.join(missing)}")
    if visible:
        details.append(f"unexpectedly visible: {', '.join(visible)}")
    tail = bytes(output[-2_000:])
    raise AssertionError(
        f"{label} did not reach the expected screen state ({'; '.join(details)})."
        f"\ncurrent terminal screen:\n{rendered}\nterminal tail={tail!r}"
    )


def wait_for_screen_cell(
    process: subprocess.Popen[bytes],
    master_fd: int,
    output: bytearray,
    screen: TerminalScreen,
    column: int,
    row: int,
    expected: str,
    label: str,
) -> None:
    deadline = time.monotonic() + TIMEOUT_SECONDS
    while time.monotonic() < deadline:
        read_available(master_fd, output, screen)
        if screen.cells[row][column] == expected:
            return
        if process.poll() is not None:
            break
        time.sleep(0.05)
    raise AssertionError(
        f"{label}: expected {expected!r} at ({column}, {row}), "
        f"got {screen.cells[row][column]!r}\n{screen.text()}"
    )


def send_mouse_click(master_fd: int, column: int, row: int) -> None:
    # Crossterm enables SGR mouse mode. Coordinates on the wire are 1-based.
    position = f"{column + 1};{row + 1}".encode()
    os.write(master_fd, b"\x1b[<0;" + position + b"M")
    os.write(master_fd, b"\x1b[<0;" + position + b"m")


def send_mouse_drag(
    master_fd: int, start_column: int, start_row: int, end_column: int, end_row: int
) -> None:
    start = f"{start_column + 1};{start_row + 1}".encode()
    end = f"{end_column + 1};{end_row + 1}".encode()
    os.write(master_fd, b"\x1b[<0;" + start + b"M")
    os.write(master_fd, b"\x1b[<32;" + end + b"M")
    os.write(master_fd, b"\x1b[<0;" + end + b"m")


def send_scroll_down(master_fd: int, column: int, row: int) -> None:
    position = f"{column + 1};{row + 1}".encode()
    os.write(master_fd, b"\x1b[<65;" + position + b"M")


def send_key(master_fd: int, sequence: bytes) -> None:
    os.write(master_fd, sequence)


def exercise(binary: Path, repository: Path) -> bytes:
    master_fd, slave_fd = pty.openpty()
    fcntl.ioctl(slave_fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))

    environment = os.environ.copy()
    environment.setdefault("TERM", "xterm-256color")
    # Default CI exercises the terminal protocol without replacing the human
    # desktop clipboard. A local macOS smoke run can opt into the native path:
    # `LATTELENS_E2E_CLIPBOARD=native python3 scripts/e2e_tui.py ...`.
    clipboard_mode = os.environ.get("LATTELENS_E2E_CLIPBOARD", "osc52")
    if clipboard_mode not in ("native", "osc52"):
        raise ValueError("LATTELENS_E2E_CLIPBOARD must be native or osc52")
    if clipboard_mode == "native" and sys.platform != "darwin":
        raise ValueError("native clipboard E2E is currently supported only on macOS")
    environment["LATTELENS_CLIPBOARD"] = clipboard_mode
    process = subprocess.Popen(
        [str(binary), str(repository)],
        stdin=slave_fd,
        stdout=slave_fd,
        stderr=slave_fd,
        env=environment,
        close_fds=True,
    )
    os.close(slave_fd)

    output = bytearray()
    screen = TerminalScreen(columns=120, rows=30)
    try:
        wait_for_raw_markers(
            process,
            master_fd,
            output,
            (b"?1000h",),
            "initial mouse-enabled terminal",
            screen=screen,
        )
        wait_for_screen_markers(
            process,
            master_fd,
            output,
            screen,
            (
                "LATTE LENS",
                "1 Files",
                "2 Git changes",
                "● Files",
                "Tree",
                "▸ a-dir",
            ),
            "initial collapsed all-files tree with tree focus",
            absent=("nested", "b-changed.rs"),
        )

        # The divider is a real mouse resize handle. Expand it, hit both panel
        # constraints, then restore the default width before coordinate-based
        # interaction checks continue.
        send_mouse_drag(master_fd, 43, 10, 52, 10)
        wait_for_screen_cell(
            process, master_fd, output, screen, 52, 10, "│", "expanded tree divider"
        )
        send_mouse_drag(master_fd, 52, 10, 0, 10)
        wait_for_screen_cell(
            process, master_fd, output, screen, 28, 10, "│", "minimum tree divider"
        )
        send_mouse_drag(master_fd, 28, 10, 119, 10)
        wait_for_screen_cell(
            process, master_fd, output, screen, 95, 10, "│", "minimum content divider"
        )
        send_mouse_drag(master_fd, 95, 10, 43, 10)
        wait_for_screen_cell(
            process, master_fd, output, screen, 43, 10, "│", "restored tree divider"
        )

        # The first root directory is selected. Keyboard Enter opens it, then
        # a single mouse click closes and reopens it. Check current cells so a
        # differential redraw that writes only the changed prefix still proves
        # the complete visible tree row.
        send_key(master_fd, b"\r")
        wait_for_screen_markers(
            process,
            master_fd,
            output,
            screen,
            ("▾ a-dir", "▸ nested"),
            "keyboard-opened collapsed All Files directory",
        )

        send_mouse_click(master_fd, 2, 4)
        wait_for_screen_markers(
            process,
            master_fd,
            output,
            screen,
            ("▸ a-dir", "Tree"),
            "mouse-closed directory with one click",
            absent=("nested", "b-changed.rs"),
        )

        send_mouse_click(master_fd, 2, 4)
        wait_for_screen_markers(
            process,
            master_fd,
            output,
            screen,
            ("▾ a-dir", "▸ nested"),
            "mouse-opened directory with one click",
        )

        send_key(master_fd, b"\r")
        wait_for_screen_markers(
            process,
            master_fd,
            output,
            screen,
            ("▸ a-dir",),
            "keyboard-closed directory",
            absent=("nested", "b-changed.rs"),
        )

        # Entering Git Changes must refresh status and use its independent
        # expansion defaults, exposing every changed ancestor immediately.
        (repository / "y-untracked.txt").write_text("new file\n", encoding="utf-8")
        send_key(master_fd, b"2")
        wait_for_screen_markers(
            process,
            master_fd,
            output,
            screen,
            (
                "Files",
                "Git changes",
                "▾ a-dir",
                "▾ nested",
                "b-changed.rs",
                "▾ .",
                "vendor/nested",
                "nested-owned.txt",
                "Diff",
                "diff --git",
                "changed()",
            ),
            "refreshed Git Changes tree with expanded ancestors and diff",
        )

        # Repository headers are real tree nodes: keyboard and mouse each
        # collapse/reopen the full repository subtree.
        send_key(master_fd, b"\x1b[H")
        send_key(master_fd, b"\r")
        wait_for_screen_markers(
            process,
            master_fd,
            output,
            screen,
            ("▸ .",),
            "keyboard-collapsed repository group",
            absent=("b-changed.rs", "nested-owned.txt"),
        )
        send_key(master_fd, b"\r")
        wait_for_screen_markers(
            process,
            master_fd,
            output,
            screen,
            ("▾ .", "b-changed.rs", "nested-owned.txt"),
            "keyboard-reopened repository group",
        )
        send_mouse_click(master_fd, 2, 4)
        wait_for_screen_markers(
            process,
            master_fd,
            output,
            screen,
            ("▸ .",),
            "mouse-collapsed repository group",
            absent=("b-changed.rs", "nested-owned.txt"),
        )
        send_mouse_click(master_fd, 2, 4)
        wait_for_screen_markers(
            process,
            master_fd,
            output,
            screen,
            ("▾ .", "vendor/nested", "nested-owned.txt"),
            "mouse-reopened repository group",
        )

        # End selects the nested repository's last changed file. Its diff must
        # come from that owning repository, then survive a graph refresh.
        send_key(master_fd, b"\x1b[F")
        wait_for_screen_markers(
            process,
            master_fd,
            output,
            screen,
            ("nested-owned.txt", "+nested changed"),
            "nested repository diff routing",
            absent=("+pub fn changed()",),
        )
        (repository / "vendor" / "nested" / "second.txt").write_text(
            "second nested file\n", encoding="utf-8"
        )
        send_key(master_fd, b"r")
        wait_for_screen_markers(
            process,
            master_fd,
            output,
            screen,
            ("second.txt", "+nested changed"),
            "repository refresh with stable owning selection",
        )

        send_key(master_fd, b"l")
        wait_for_screen_markers(
            process,
            master_fd,
            output,
            screen,
            ("● Diff", "Content"),
            "visible content focus cue",
        )

        send_key(master_fd, b"h")
        send_key(master_fd, b"\x1b[H")
        send_key(master_fd, b"\x1b[A")
        wait_for_screen_markers(
            process,
            master_fd,
            output,
            screen,
            ("● 2 Git changes", "Tabs"),
            "visible scope-tabs focus cue",
        )

        # A mouse scope switch restores the collapsed All Files tree. Clicking
        # its file row then keeps the tree focus and restores the Preview pane.
        send_mouse_click(master_fd, 2, 2)
        wait_for_screen_markers(
            process,
            master_fd,
            output,
            screen,
            ("● Files", "▸ a-dir", "Tree"),
            "mouse-selected collapsed All Files scope",
            absent=("nested", "b-changed.rs"),
        )
        # The nested-repository directory and `y-untracked.txt` precede the
        # known clean file, which is now the fourth visible All Files row.
        send_mouse_click(master_fd, 2, 7)
        wait_for_screen_markers(
            process,
            master_fd,
            output,
            screen,
            ("Preview", "clean()", "Tree"),
            "mouse-selected All Files preview",
        )

        # The Preview text begins at column 49 after the one-cell line number
        # and its gutter. Drag across `clean`: mouse release must copy without
        # waiting for a key, while Ctrl+C remains a repeat-copy shortcut. This
        # exercises the same SGR mouse and control bytes sent by a terminal.
        copy_start = len(output)
        send_mouse_drag(master_fd, 56, 3, 60, 3)
        expected_copy_status = (
            "Copied 5 characters"
            if clipboard_mode == "native"
            else "Sent 5 characters to terminal clipboard"
        )
        wait_for_screen_markers(
            process,
            master_fd,
            output,
            screen,
            (expected_copy_status,),
            "mouse-release Preview clipboard copy",
        )
        if clipboard_mode == "native":
            clipboard = subprocess.run(
                ["pbpaste"], check=True, capture_output=True
            ).stdout
            if clipboard != b"clean":
                raise AssertionError(
                    f"native clipboard mismatch: expected b'clean', got {clipboard!r}"
                )
        else:
            wait_for_raw_markers(
                process,
                master_fd,
                output,
                (b"\x1b]52;c;Y2xlYW4=\x07",),
                "exact Preview OSC 52 clipboard payload",
                start=copy_start,
                screen=screen,
            )
        send_key(master_fd, b"\x03")
        wait_for_screen_markers(
            process,
            master_fd,
            output,
            screen,
            (expected_copy_status,),
            "Ctrl+C Preview clipboard recopy",
        )
        send_scroll_down(master_fd, 90, 10)

        os.write(master_fd, b"qq")
        quit_deadline = time.monotonic() + TIMEOUT_SECONDS
        while process.poll() is None and time.monotonic() < quit_deadline:
            # A PTY has a bounded output buffer. Keep draining redraws while the
            # process handles the quit key, otherwise wait() can deadlock.
            read_available(master_fd, output, screen)
            time.sleep(0.02)
        if process.poll() is None:
            raise AssertionError("TUI did not exit after receiving q twice")
        read_available(master_fd, output, screen)
        if process.returncode != 0:
            raise AssertionError(f"TUI exited with status {process.returncode}")
        if b"?1000l" not in output:
            raise AssertionError("mouse capture was not disabled on exit")
        if OLD_CANVAS_ESCAPE in output:
            raise AssertionError("the removed hard-coded canvas background was emitted")
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        os.close(master_fd)

    return bytes(output)


def main() -> int:
    if sys.platform == "win32":
        print("PTY E2E is supported on Linux and macOS", file=sys.stderr)
        return 2
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} /path/to/latte-lens", file=sys.stderr)
        return 2

    binary = Path(sys.argv[1]).resolve()
    if not binary.is_file():
        print(f"binary does not exist: {binary}", file=sys.stderr)
        return 2

    clipboard_mode = os.environ.get("LATTELENS_E2E_CLIPBOARD", "osc52")
    original_clipboard: bytes | None = None
    if clipboard_mode == "native" and sys.platform == "darwin":
        original_clipboard = subprocess.run(
            ["pbpaste"], check=False, capture_output=True
        ).stdout

    try:
        with tempfile.TemporaryDirectory(prefix="latte-lens-e2e-") as directory:
            repository = Path(directory)
            create_fixture(repository)
            output = exercise(binary, repository)
    finally:
        if original_clipboard is not None:
            subprocess.run(
                ["pbcopy"], input=original_clipboard, check=True, capture_output=True
            )

    print(f"E2E passed: rendered and exited cleanly ({len(output)} terminal bytes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
