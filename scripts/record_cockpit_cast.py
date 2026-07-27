# SPDX-License-Identifier: AGPL-3.0-only
"""Record the cockpit to an asciinema cast without an interactive terminal.

`record-cockpit-cast.sh` drives a *human* tour on a real TTY. This driver does the
same tour unattended, so the cast is a regenerable artifact like `just tui-shots`
rather than a hand-made one that goes stale: it allocates a pty (a pty **is** a
terminal, which is all `asciinema` and `crossterm` require), spawns
`asciinema rec --command "quiver tui …"` inside it, and feeds the tour's keystrokes
with human-ish pauses.

It seeds a handful of extra collections first, because a cockpit is only worth
recording with something in it — a single 8-point collection leaves the
constellation and the per-row load bars with nothing to show.

Usage (see the shell wrapper, which passes these in):

    uv run --project sdks/python python scripts/record_cockpit_cast.py \
        --cast docs/assets/cockpit.cast --url http://127.0.0.1:6333 \
        --api-key quiver-demo-key --quiver ./target/debug/quiver
"""

from __future__ import annotations

import argparse
import fcntl
import json
import math
import os
import pty
import random
import select
import struct
import sys
import termios
import time

from quiver import Client, FilterableField, Point

# The cast's terminal geometry. 120x38 is wide enough for the cockpit's two-column
# layout and still legible embedded in a README.
COLS, ROWS = 120, 38

# Collections seeded for the recording: a spread of index kinds, dimensions and
# sizes so the collections table, its load bars, the index histogram and the
# constellation all have something real to render.
CAST_COLLECTIONS = [
    ("articles", 64, 600, "hnsw", "cosine", {}),
    ("events", 32, 1200, "ivf", "l2", {}),
    ("catalog", 48, 400, "disk_vamana", "cosine", {"pq_subspaces": 8}),
]
TOPICS = ["search", "storage", "ops", "crypto"]

# The tour: (seconds to wait before sending, bytes to send). Keys per the cockpit's
# keymap — j/k move, `v` opens the constellation, enter re-queries, esc goes back,
# ^T cycles the theme, q quits.
#
# Note the help overlay is dismissed with `?`, not esc: in the *browser* view esc is
# bound to quit, so a stray esc there ends the recording mid-tour.
# Collections list alphabetically (articles, catalog, demo, events) and the cursor
# starts on the first, so j/j/k/k walks three of them and returns to `articles` —
# the 600-point collection, which gives the constellation a real point cloud to
# draw. Landing on the 8-point `demo` instead makes the headline view look empty.
TOUR = [
    (4.0, b""),  # let the dashboard paint and take a refresh tick
    (1.5, b"?"),  # the help overlay
    (3.5, b"?"),  # dismiss it
    (1.5, b"j"),  # walk the collections table — the tree follows the selection
    (2.0, b"j"),
    (2.0, b"k"),
    (2.0, b"k"),  # back to `articles`
    (2.0, b"v"),  # open its constellation
    (4.0, b"j"),  # move the cursor between points; the nearest star brightens
    (1.5, b"j"),
    (1.5, b"j"),
    (2.5, b"\r"),  # re-centre the query on the selected point
    (4.0, b"j"),
    (2.5, b"\x1b"),  # back to the browser
    (2.0, b"\x14"),  # ^T: bronze → slate
    (3.0, b"\x14"),  # and back to the brand default
    (2.5, b"q"),  # quit
]


def seed(url: str, api_key: str) -> None:
    """Create the cast's collections and wait until each one actually answers.

    The wait is not politeness: a freshly written collection reports its full point
    count while its index is still being built off-lock, and a search in that window
    returns an empty result set rather than blocking (see ADR-0081). Recording
    through it produces a cast of an empty constellation.
    """
    random.seed(7)
    with Client(url, api_key=api_key) as q:
        for name, dim, count, index, metric, extra in CAST_COLLECTIONS:
            try:
                q.delete_collection(name)
            except Exception:  # noqa: BLE001 — best-effort reset on re-run
                pass
            q.create_collection(
                name,
                dim=dim,
                metric=metric,
                index=index,
                filterable=[
                    FilterableField("topic", "keyword"),
                    FilterableField("score", "numeric"),
                ],
                **extra,
            )
            centers = [[random.gauss(0, 1) for _ in range(dim)] for _ in range(6)]
            points = []
            for i in range(count):
                center = centers[i % len(centers)]
                raw = [center[j] + random.gauss(0, 0.12) for j in range(dim)]
                norm = math.sqrt(sum(x * x for x in raw)) or 1.0
                points.append(
                    Point(
                        f"p-{i:05d}",
                        [x / norm for x in raw],
                        {"topic": TOPICS[i % len(TOPICS)], "score": i % 100},
                    )
                )
            for start in range(0, len(points), 200):
                q.upsert(name, points[start : start + 200])
            print(f"    seeded {name}: {count} × {dim}-d ({index}/{metric})")

        for info in q.list_collections():
            probe = [0.1] * info.dim
            for _ in range(120):
                if q.search(info.name, probe, k=5):
                    break
                time.sleep(0.25)
            else:
                sys.exit(f"error: {info.name} never returned a result; not recording")
        print("    all collections answer queries — indexes are warm")


def record(cast: str, url: str, api_key: str, quiver: str) -> None:
    """Run the tour under `asciinema rec` in a pty and write `cast`."""
    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.environ["COLUMNS"], os.environ["LINES"] = str(COLS), str(ROWS)
        os.execvp(
            "asciinema",
            [
                "asciinema", "rec", cast, "--overwrite",
                "--title", "Quiver cockpit",
                "--command", f"{quiver} tui --url {url} --api-key {api_key}",
            ],
        )

    # Without an explicit window size the pty reports 0x0, the cast records 0x0 and
    # the cockpit has no area to lay out in.
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

    def drain(seconds: float) -> bool:
        """Read (and discard) child output for `seconds`; False once it closes."""
        end = time.monotonic() + seconds
        while True:
            left = end - time.monotonic()
            if left <= 0:
                return True
            readable, _, _ = select.select([fd], [], [], left)
            if not readable:
                continue
            try:
                if not os.read(fd, 65536):
                    return False
            except OSError:
                return False

    for delay, keys in TOUR:
        if not drain(delay):
            break
        if keys:
            os.write(fd, keys)
    drain(3.0)
    try:
        os.close(fd)
    except OSError:
        pass
    os.waitpid(pid, 0)


def verify(cast: str) -> None:
    """Fail loudly if the cast is empty, mis-sized, or never reached the tour.

    A silently-broken cast is worse than none: it gets committed and shipped.
    """
    with open(cast, encoding="utf-8") as handle:
        header = json.loads(handle.readline())
        frames = [json.loads(line) for line in handle if line.strip()]
    if (header.get("width"), header.get("height")) != (COLS, ROWS):
        sys.exit(f"error: cast is {header.get('width')}x{header.get('height')}, want {COLS}x{ROWS}")
    if len(frames) < 20:
        sys.exit(f"error: cast has only {len(frames)} frames — the tour did not run")
    # The wordmark is block-character art, so assert on panel titles and the
    # constellation footer instead — i.e. that the tour actually reached both views.
    output = "".join(frame[2] for frame in frames if frame[1] == "o").lower()
    for marker in ("the cockpit", "collections", "relationships", "constellation", "re-query"):
        if marker not in output:
            sys.exit(f"error: cast never rendered {marker!r}")
    duration = frames[-1][0]
    print(f"    {cast}: {len(frames)} frames, {duration:.1f}s, {COLS}x{ROWS} — verified")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cast", required=True)
    parser.add_argument("--url", required=True)
    parser.add_argument("--api-key", required=True)
    parser.add_argument("--quiver", required=True)
    parser.add_argument(
        "--no-seed",
        action="store_true",
        help="record whatever the server already holds instead of seeding",
    )
    args = parser.parse_args()

    os.makedirs(os.path.dirname(os.path.abspath(args.cast)), exist_ok=True)
    if not args.no_seed:
        print("==> seeding the cast's collections")
        seed(args.url, args.api_key)
    print("==> recording the tour")
    record(args.cast, args.url, args.api_key, args.quiver)
    print("==> verifying")
    verify(args.cast)


if __name__ == "__main__":
    main()
