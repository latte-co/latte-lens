"""Small POSIX PTY driver and bounded terminal evidence capture."""

from __future__ import annotations

import fcntl
import os
import pty
import select
import struct
import subprocess
import termios
import time
import unicodedata
from dataclasses import dataclass, field
from pathlib import Path


DEFAULT_WAIT_SECONDS = 10.0
MAX_EVIDENCE_BYTES = 200 * 1024
OLD_CANVAS_ESCAPE = b"48;2;12;10;9"


class TerminalScreen:
    """Streaming screen model for the ANSI subset emitted by Crossterm."""

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

    def find(self, marker: str) -> tuple[int, int] | None:
        for row, cells in enumerate(self.cells):
            column = "".join(cells).find(marker)
            if column >= 0:
                return column, row
        return None

    def vertical_rule_columns(self, glyphs: str = "│┃") -> list[int]:
        """Return columns that are a rule on most non-header rows."""

        sampled_rows = list(range(2, max(3, self.rows - 1)))
        threshold = max(1, (len(sampled_rows) + 1) // 2)
        return [
            column
            for column in range(self.columns)
            if sum(self.cells[row][column] in glyphs for row in sampled_rows)
            >= threshold
        ]

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
        if byte == 0x08:
            self.cursor_column = max(0, self.cursor_column - 1)
        elif byte == 0x09:
            self.cursor_column = min(self.columns - 1, (self.cursor_column // 8 + 1) * 8)
        elif byte in (0x0A, 0x0B, 0x0C):
            self._line_feed()
        elif byte == 0x0D:
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
            if byte == 0x07:
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
        elif final == "A":
            self.cursor_row = max(0, self.cursor_row - self._parameters(parameters, 1)[0])
        elif final == "B":
            self.cursor_row = min(
                self.rows - 1, self.cursor_row + self._parameters(parameters, 1)[0]
            )
        elif final == "C":
            self.cursor_column = min(
                self.columns - 1, self.cursor_column + self._parameters(parameters, 1)[0]
            )
        elif final == "D":
            self.cursor_column = max(0, self.cursor_column - self._parameters(parameters, 1)[0])
        elif final == "G":
            self.cursor_column = min(
                self.columns - 1, max(0, self._parameters(parameters, 1)[0] - 1)
            )
        elif final == "d":
            self.cursor_row = min(
                self.rows - 1, max(0, self._parameters(parameters, 1)[0] - 1)
            )
        elif final == "J":
            self._erase_display(self._parameters(parameters, default=0)[0])
        elif final == "K":
            self._erase_line(self._parameters(parameters, default=0)[0])
        elif final == "h" and 1049 in self._parameters(parameters, default=0):
            self._reset()

    @staticmethod
    def _parameters(parameters: str, default: int) -> list[int]:
        values: list[int] = []
        for value in parameters.lstrip(">?<!=" ).split(";"):
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


class E2EAssertionError(AssertionError):
    """An assertion with a stable failure kind for machine-readable evidence."""

    def __init__(self, kind: str, message: str) -> None:
        super().__init__(message)
        self.kind = kind


@dataclass
class PtySession:
    process: subprocess.Popen[bytes]
    master_fd: int
    screen: TerminalScreen
    output: bytearray = field(default_factory=bytearray)
    assertions: list[str] = field(default_factory=list)
    master_closed: bool = False
    forced_termination: bool = False

    @classmethod
    def launch(
        cls,
        binary: Path,
        repository: Path,
        environment: dict[str, str],
        *,
        columns: int = 120,
        rows: int = 30,
    ) -> "PtySession":
        master_fd, slave_fd = pty.openpty()
        fcntl.ioctl(slave_fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))
        try:
            process = subprocess.Popen(
                [str(binary), str(repository)],
                cwd=repository,
                stdin=slave_fd,
                stdout=slave_fd,
                stderr=slave_fd,
                env=environment,
                close_fds=True,
                start_new_session=True,
            )
        finally:
            os.close(slave_fd)
        return cls(process, master_fd, TerminalScreen(columns, rows))

    def drain(self) -> None:
        while True:
            ready, _, _ = select.select([self.master_fd], [], [], 0)
            if not ready:
                return
            try:
                chunk = os.read(self.master_fd, 65_536)
            except OSError:
                return
            if not chunk:
                return
            self.output.extend(chunk)
            self.screen.feed(chunk)

    def wait_raw(
        self,
        markers: tuple[bytes, ...],
        label: str,
        *,
        start: int = 0,
        timeout: float = DEFAULT_WAIT_SECONDS,
    ) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.drain()
            window = self.output[start:]
            if all(marker in window for marker in markers):
                self.assertions.append(label)
                return
            if self.process.poll() is not None:
                break
            time.sleep(0.02)
        missing = [marker.decode(errors="replace") for marker in markers if marker not in self.output[start:]]
        raise E2EAssertionError(
            "raw_convergence",
            f"{label} is missing: {', '.join(missing)}; terminal tail={self.raw_tail()!r}",
        )

    def wait_screen(
        self,
        markers: tuple[str, ...],
        label: str,
        *,
        absent: tuple[str, ...] = (),
        timeout: float = DEFAULT_WAIT_SECONDS,
    ) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.drain()
            rendered = self.screen.text()
            missing = [marker for marker in markers if marker not in rendered]
            visible = [marker for marker in absent if marker in rendered]
            if not missing and not visible:
                self.assertions.append(label)
                return
            if self.process.poll() is not None:
                break
            time.sleep(0.02)
        rendered = self.screen.text()
        missing = [marker for marker in markers if marker not in rendered]
        visible = [marker for marker in absent if marker in rendered]
        details = []
        if missing:
            details.append(f"missing: {', '.join(missing)}")
        if visible:
            details.append(f"unexpectedly visible: {', '.join(visible)}")
        raise E2EAssertionError(
            "screen_convergence",
            f"{label} did not reach expected state ({'; '.join(details)})."
            f"\ncurrent terminal screen:\n{rendered}\nterminal tail={self.raw_tail()!r}",
        )

    def wait_until(
        self,
        predicate,
        label: str,
        *,
        timeout: float = DEFAULT_WAIT_SECONDS,
    ) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.drain()
            if predicate(self.screen):
                self.assertions.append(label)
                return
            if self.process.poll() is not None:
                break
            time.sleep(0.02)
        raise E2EAssertionError(
            "screen_convergence",
            f"{label} did not reach expected semantic state.\n{self.screen.text()}",
        )

    def key(self, sequence: bytes) -> None:
        os.write(self.master_fd, sequence)

    def click(self, column: int, row: int) -> None:
        position = f"{column + 1};{row + 1}".encode()
        os.write(self.master_fd, b"\x1b[<0;" + position + b"M")
        os.write(self.master_fd, b"\x1b[<0;" + position + b"m")

    def click_marker(self, marker: str, *, offset: int = 0) -> None:
        position = self.screen.find(marker)
        if position is None:
            raise E2EAssertionError("input_target", f"cannot click missing marker: {marker}")
        column, row = position
        self.click(column + offset, row)

    def drag(self, start_column: int, start_row: int, end_column: int, end_row: int) -> None:
        start = f"{start_column + 1};{start_row + 1}".encode()
        end = f"{end_column + 1};{end_row + 1}".encode()
        os.write(self.master_fd, b"\x1b[<0;" + start + b"M")
        os.write(self.master_fd, b"\x1b[<32;" + end + b"M")
        os.write(self.master_fd, b"\x1b[<0;" + end + b"m")

    def scroll_down(self, column: int, row: int) -> None:
        position = f"{column + 1};{row + 1}".encode()
        os.write(self.master_fd, b"\x1b[<65;" + position + b"M")

    def quit_cleanly(self, timeout: float = DEFAULT_WAIT_SECONDS) -> None:
        self.key(b"qq")
        deadline = time.monotonic() + timeout
        while self.process.poll() is None and time.monotonic() < deadline:
            self.drain()
            time.sleep(0.02)
        if self.process.poll() is None:
            raise E2EAssertionError("child_exit_timeout", "TUI did not exit after receiving q twice")
        self.drain()
        if self.process.returncode != 0:
            raise E2EAssertionError("child_exit", f"TUI exited with status {self.process.returncode}")
        if b"?1000l" not in self.output:
            raise E2EAssertionError("terminal_cleanup", "mouse capture was not disabled on exit")
        if OLD_CANVAS_ESCAPE in self.output:
            raise E2EAssertionError("terminal_protocol", "removed hard-coded canvas background was emitted")
        self.assertions.extend(
            ["clean child exit", "mouse capture disabled", "legacy canvas escape absent"]
        )

    def close(self) -> None:
        if self.process.poll() is None:
            self.forced_termination = True
            self.process.terminate()
            deadline = time.monotonic() + 2
            while self.process.poll() is None and time.monotonic() < deadline:
                self.drain()
                time.sleep(0.02)
            if self.process.poll() is None:
                self.process.kill()
                self.process.wait()
        self.drain()
        if not self.master_closed:
            os.close(self.master_fd)
            self.master_closed = True

    def raw_tail(self) -> bytes:
        return bytes(self.output[-MAX_EVIDENCE_BYTES:])

    def evidence(self) -> dict[str, object]:
        return {
            "pid": self.process.pid,
            "returncode": self.process.poll(),
            "terminal_bytes": len(self.output),
            "assertions": list(self.assertions),
            "screen": self.screen.text()[-MAX_EVIDENCE_BYTES:],
            "terminal_tail_hex": self.raw_tail().hex(),
        }

    def cleanup_receipt(self) -> dict[str, object]:
        return {
            "pid": self.process.pid,
            "process_exited": self.process.poll() is not None,
            "returncode": self.process.poll(),
            "forced_termination": self.forced_termination,
            "pty_closed": self.master_closed,
        }
