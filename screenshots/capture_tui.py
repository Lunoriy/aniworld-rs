#!/usr/bin/env python3
"""Drive the real TUI in a pseudo-terminal and render the captured screen.

The binary is executed for real (network included); the pty output is decoded
with pyte into a character grid, then painted with Pillow so the result is a
faithful picture of what the terminal showed.

    pip install pyte pillow
    python capture_tui.py ./target/release/aniworld-rs ./screenshots
"""

from __future__ import annotations

import codecs
import fcntl
import os
import pty
import re
import select
import struct
import sys
import termios
import time

import pyte
from PIL import Image, ImageDraw, ImageFont

COLS, ROWS = 140, 42
FONT_SIZE = 15
PAD = 14
CHROME = 30

DEFAULT_FG = "#e2e8f0"
DEFAULT_BG = "#0a0c13"
NAMED = {
    "black": "#1b1e27",
    "red": "#f87171",
    "green": "#6ae3b9",
    "brown": "#ffb84c",
    "blue": "#7da6ff",
    "magenta": "#c792ea",
    "cyan": "#67e8f9",
    "white": "#e2e8f0",
    "brightblack": "#70788e",
    "brightred": "#fca5a5",
    "brightgreen": "#86efac",
    "brightbrown": "#ffd28a",
    "brightblue": "#a5b4fc",
    "brightmagenta": "#e9b3ff",
    "brightcyan": "#a5f3fc",
    "brightwhite": "#ffffff",
}


def to_hex(value: str | None, background: bool) -> str:
    default = DEFAULT_BG if background else DEFAULT_FG
    if not value or value == "default":
        return default
    if value.startswith("#"):
        return value
    if re.fullmatch(r"[0-9a-fA-F]{6}", value):
        return "#" + value
    if value in NAMED:
        return NAMED[value]
    if value.isdigit():
        index = int(value)
        if index < 16:
            return NAMED.get(
                [
                    "black", "red", "green", "brown", "blue", "magenta", "cyan", "white",
                    "brightblack", "brightred", "brightgreen", "brightbrown", "brightblue",
                    "brightmagenta", "brightcyan", "brightwhite",
                ][index],
                default,
            )
        r = 55 + (index - 16) // 36 % 6 * 40
        g = 55 + (index - 16) // 6 % 6 * 40
        b = 55 + (index - 16) % 6 * 40
        return f"#{r:02x}{g:02x}{b:02x}"
    return default


class Tui:
    def __init__(self, binary: str, env: dict[str, str], args: list[str] | None = None):
        self.args = args or ["tui"]
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.Stream(self.screen)
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.pid = os.fork()
        if self.pid == 0:
            os.setsid()
            fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
            os.dup2(slave, 0)
            os.dup2(slave, 1)
            os.dup2(slave, 2)
            os.close(self.master)
            os.close(slave)
            os.execvpe(binary, [binary, *self.args], {**os.environ, **env})
        os.close(slave)
        self.raw = bytearray()
        self.pump(0.2)

    def pump(self, seconds: float):
        deadline = time.time() + seconds
        while time.time() < deadline:
            ready, _, _ = select.select([self.master], [], [], 0.05)
            if not ready:
                continue
            try:
                chunk = os.read(self.master, 65536)
            except OSError:
                return
            if not chunk:
                return
            self.raw.extend(chunk)
            self.stream.feed(self.decoder.decode(chunk))

    def wait_for(self, text: str, timeout: float = 20.0) -> bool:
        deadline = time.time() + timeout
        while time.time() < deadline:
            self.pump(0.25)
            rows = ["".join(cell.data for cell in self.screen.buffer[y].values()) for y in range(ROWS)]
            if any(text in row for row in rows):
                return True
        return False

    def send(self, text: str):
        os.write(self.master, text.encode())
        self.pump(0.15)

    def snapshot(self, path_html: str, title: str, caption: str):
        cw = 10
        ch = 20
        width = COLS * cw + PAD * 2
        height = ROWS * ch + PAD * 2 + CHROME
        image = Image.new("RGB", (width, height), DEFAULT_BG)
        draw = ImageDraw.Draw(image)

        # window chrome
        draw.rectangle([0, 0, width, CHROME], fill="#151823")
        for index, colour in enumerate(("#f87171", "#ffb84c", "#6ae3b9")):
            draw.ellipse([14 + index * 18, 10, 24 + index * 18, 20], fill=colour)
        draw.text((86, 7), title, font=self.title_font, fill="#b6bdcd")

        regular = ImageFont.truetype(FONT_FILE, FONT_SIZE)
        bold = ImageFont.truetype(FONT_BOLD_FILE, FONT_SIZE)

        for y in range(ROWS):
            for x in range(COLS):
                cell = self.screen.buffer[y][x]
                bg = to_hex(cell.bg, True)
                fg = to_hex(cell.fg, False)
                if cell.reverse:
                    bg, fg = fg, bg
                px, py = PAD + x * cw, CHROME + PAD + y * ch
                if bg != DEFAULT_BG:
                    draw.rectangle([px, py, px + cw, py + ch], fill=bg)
                if cell.data and cell.data != " ":
                    draw.text(
                        (px, py),
                        cell.data,
                        font=bold if cell.bold else regular,
                        fill=fg,
                    )

        draw.rectangle([0, 0, width - 1, height - 1], outline="#232838")
        draw.text(
            (PAD, height - PAD - 6),
            caption,
            font=self.small_font,
            fill="#70788e",
        )
        image.save(path_html)
        with open(path_html.replace(".png", ".txt"), "w") as handle:
            for y in range(ROWS):
                handle.write(
                    "".join(cell.data for cell in self.screen.buffer[y].values()).rstrip()
                    + "\n"
                )

    @property
    def title_font(self):
        return ImageFont.truetype(FONT_BOLD_FILE, 14)

    @property
    def small_font(self):
        return ImageFont.truetype(FONT_FILE, 11)

    def close(self):
        try:
            os.write(self.master, b"q")
            self.pump(1.0)
        except OSError:
            pass
        try:
            os.kill(self.pid, 15)
        except ProcessLookupError:
            pass
        os.close(self.master)


def capture_cli(binary: str, outdir: str, env: dict[str, str], font_args):
    """Two plain-CLI screens: metadata, and the new numbering in a dry run."""
    info = Tui(
        binary,
        env,
        ["info", "https://aniworld.to/anime/stream/goblin-slayer"],
    )
    info.pump(14)
    info.snapshot(f"{outdir}/06-cli-info.png", "aniworld-rs info", "terminal capture, 140×42")
    info.close()

    plan = Tui(
        binary,
        env,
        [
            "download",
            "--dry-run",
            "--season",
            "21",
            "--episodes",
            "1-3",
            "https://aniworld.to/anime/stream/one-piece",
        ],
    )
    plan.pump(25)
    plan.snapshot(
        f"{outdir}/07-cli-numbering.png",
        "aniworld-rs download --dry-run",
        "terminal capture — [Episode N] numbering on One Piece season 21",
    )
    plan.close()


def main():
    binary, outdir = sys.argv[1], sys.argv[2]
    mode = sys.argv[3] if len(sys.argv) > 3 else "tui"
    global FONT_FILE, FONT_BOLD_FILE
    FONT_FILE = os.environ.get("TUI_FONT", "/tmp/DejaVuSansMono.ttf")
    FONT_BOLD_FILE = os.environ.get("TUI_FONT_BOLD", "/tmp/DejaVuSansMono-Bold.ttf")

    os.makedirs(outdir, exist_ok=True)
    env = {
        "ANIWORLD_DOWNLOAD_PATH": "/tmp/shotdl",
        "ANIWORLD_BANDWIDTH_LIMIT": "3M",
        "ANIWORLD_JOBS": "1",
        "ANIWORLD_FFMPEG": os.environ.get("ANIWORLD_FFMPEG", "ffmpeg"),
        "TERM": "xterm-256color",
    }
    if mode == "cli":
        capture_cli(binary, outdir, env, None)
        return

    tui = Tui(binary, env)
    caption = "aniworld-rs — real pty capture, 140×42"

    tui.pump(1.5)
    tui.snapshot(f"{outdir}/01-empty.png", "aniworld-rs", caption + " — fresh start")

    tui.send("/")
    tui.send("goblin slayer")
    tui.send("\r")
    tui.wait_for("result(s)")
    tui.pump(0.5)
    tui.snapshot(f"{outdir}/02-search.png", "aniworld-rs", caption + " — search")

    tui.send("\r")
    tui.wait_for("season(s)")
    tui.wait_for("episode(s) loaded")
    tui.pump(1.0)
    tui.snapshot(f"{outdir}/03-browse.png", "aniworld-rs", caption + " — series loaded")

    tui.send("\r")
    tui.wait_for("segments")
    tui.pump(3.0)
    tui.snapshot(f"{outdir}/04-download.png", "aniworld-rs", caption + " — episode downloading")

    tui.send("\t")
    tui.pump(1.0)
    tui.snapshot(f"{outdir}/05-queue-focus.png", "aniworld-rs", caption + " — queue focused")

    tui.close()
    print("raw bytes:", len(tui.raw))


if __name__ == "__main__":
    main()
