#!/usr/bin/env python3
"""Reproducible SoundCloud Rust vs Bun benchmark.

The Bun baseline implements the same public resolve/HLS algorithm as the Rust
client. It deliberately does not call yt-dlp or ffmpeg.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import resource
import shutil
import statistics
import subprocess
import time

ROOT = Path(__file__).resolve().parent.parent
RUST = ROOT / "target/release/ystt"
BUN_SCRIPT = ROOT / "bench/soundcloud_bun.ts"
TRACK = "https://soundcloud.com/treezymonster812/rozovoe-vino-2-ft-morgenshtern"
PLAYLIST = "https://soundcloud.com/kaiangel-9mice/sets/shh-1"
OUT = Path("/tmp/opencode/ystt-soundcloud-bench")


def run(command: list[str], cwd: Path = ROOT) -> tuple[float, resource.struct_rusage]:
    started = time.perf_counter()
    result = subprocess.run(command, cwd=cwd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    elapsed = time.perf_counter() - started
    if result.returncode != 0:
        raise RuntimeError(f"{command} failed: {result.stderr.decode(errors='replace')[:2000]}")
    return elapsed, resource.getrusage(resource.RUSAGE_CHILDREN)


def median(runs: list[float]) -> float:
    return round(statistics.median(runs), 3)


def main() -> None:
    shutil.rmtree(OUT, ignore_errors=True)
    (OUT / "rust").mkdir(parents=True)
    (OUT / "bun").mkdir(parents=True)

    rust_runs = []
    bun_runs = []
    for _ in range(7):
        elapsed, _ = run([
            str(RUST), "--no-cache", "-o", str(OUT / "rust"), "--filename", "sc", TRACK
        ])
        rust_runs.append(elapsed)
    for _ in range(7):
        elapsed, _ = run([BUN_SCRIPT, TRACK, str(OUT / "bun/sc.m4a")])
        bun_runs.append(elapsed)

    rust_track = OUT / "rust/sc.m4a"
    bun_track = OUT / "bun/sc.m4a"
    rust_hash = hashlib.sha256(rust_track.read_bytes()).hexdigest()
    bun_hash = hashlib.sha256(bun_track.read_bytes()).hexdigest()

    playlist_runs = []
    for index in range(3):
        directory = OUT / f"playlist-{index}"
        elapsed, _ = run([
            str(RUST), "--no-cache", "-o", str(directory), "--filename", "{post_id} - {title}", PLAYLIST
        ])
        playlist_runs.append(elapsed)

    rust_usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    report = {
        "track": {
            "rust_median_sec": median(rust_runs),
            "bun_median_sec": median(bun_runs),
            "rust_runs_sec": [round(x, 3) for x in rust_runs],
            "bun_runs_sec": [round(x, 3) for x in bun_runs],
            "bytes": rust_track.stat().st_size,
            "same_sha256": rust_hash == bun_hash,
            "sha256": rust_hash,
        },
        "playlist_15": {"rust_median_sec": median(playlist_runs)},
        "child_max_rss_mib": round(rust_usage.ru_maxrss / 1024, 1),
        "note": "Single-run peak RSS is dominated by the largest child; run "
                "memory via /usr/bin/time or the API for separate figures.",
    }
    print(json.dumps(report, indent=2))
    (ROOT / "bench/results.json").write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
