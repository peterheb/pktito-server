#!/usr/bin/env python3
"""bench.py: AliceBench's twelve chapters through a running pktito-server,
several times over with several requests at once, and how many seconds of
audio it transcribed a second, all told.

    python3 tools/bench.py [--url URL] [--passes 5] [--at-once 4] [--dir DIR]

The chapters are AliceBench v1's: LibriVox's Alice's Adventures in
Wonderland, 16 kHz mono FLAC, 2:58:51 in all, downloaded into DIR
(~/.cache/alicebench-v1, which the app's scripts/alicebench.sh shares) when
missing and checked by SHA-256. One short chapter is heard first, untimed,
so that the model's pages are read before the clock starts. Then each
chapter is sent once a pass, pass after pass with no wait between them,
--at-once requests in flight, as pktito_json; a chapter's answer must be the
same bytes every pass. A key comes from --key or PKTITO_API_KEY.

The figure is the server's on that machine as it stood: run it where the
server runs, with nothing else busy, and say the server's -t and -w with it.
The server reads the FLACs through its ffmpeg (--ffmpeg PATH).
Stdlib only.
"""

import argparse
import collections
import concurrent.futures
import hashlib
import json
import os
import statistics
import sys
import time
import urllib.error
import urllib.request
import uuid

BASE = "https://models.pktito.net/alicebench/v1"
# name and SHA-256, as pktito-app's scripts/alicebench.sh has them since the
# FLACs' 4,096-sample blocks (2026-10-25; the samples unchanged)
CHAPTERS = [
    (
        "wonderland_ch_01.flac",
        "ad3326f63b5371090294c38a80467dcd288bc309b9def8547c550429844b6a31",
    ),
    (
        "wonderland_ch_02.flac",
        "7507934bbf636c7ef5a3089eba6f5aec3848528ac1150b987ad80840472c3f97",
    ),
    (
        "wonderland_ch_03.flac",
        "919e8f75289091fce57dde9c0e1947132ad42e88095b8921696d0a3fccd0e50d",
    ),
    (
        "wonderland_ch_04.flac",
        "f575db797db9a0db3af94f1a560b191fcd485e9cc212c19da4ebdf9f2af8e20e",
    ),
    (
        "wonderland_ch_05.flac",
        "57d3ec5801fd93782f8d5e6f193a45f11f06ffb5e8358c88b9aecf5bbb2780b5",
    ),
    (
        "wonderland_ch_06.flac",
        "cffb9569b7e875e2588bcd92c0fcc38bdb15be232595d54a57c25ca5fcc6133f",
    ),
    (
        "wonderland_ch_07.flac",
        "59fbba17032e80ae188837a4f77b399fa97817597bf57925952f28e3cd7c731a",
    ),
    (
        "wonderland_ch_08.flac",
        "e47086d07d2f7e73488550531b2eb4c7e20c7761e806a68f0f544a5dc8d51d5a",
    ),
    (
        "wonderland_ch_09.flac",
        "a0ec98e0bcb2cc84502faaffdcede67d309f27cc846d8ec18b00160c4c322f0e",
    ),
    (
        "wonderland_ch_10.flac",
        "463975d3aaedb928618ef1b16bb4ede8640c4a75d0d92b209fcc9c8bd6832aa5",
    ),
    (
        "wonderland_ch_11.flac",
        "6ef258f362737f4976e06713624dde9aff46a8a72728a5ecf2c70ef907384f59",
    ),
    (
        "wonderland_ch_12.flac",
        "922986f0c8f45fe6465daf990b832d664f24b8fdc159c45082ea449abac64ff9",
    ),
]


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def fetch(folder):
    os.makedirs(folder, exist_ok=True)
    for name, want in CHAPTERS:
        path = os.path.join(folder, name)
        if os.path.exists(path) and sha256(path) == want:
            continue
        print(f"bench: downloading {name}", file=sys.stderr)
        part = path + ".part"
        with (
            urllib.request.urlopen(f"{BASE}/{name}", timeout=60) as r,
            open(part, "wb") as out,
        ):
            while block := r.read(1 << 20):
                out.write(block)
        if sha256(part) != want:
            sys.exit(f"bench: {name} isn't AliceBench v1's")
        os.replace(part, path)


def flac_seconds(path):
    """The length a FLAC's STREAMINFO block says: its total samples over its
    rate (the block follows "fLaC" and a 4-byte block header)."""
    with open(path, "rb") as f:
        head = f.read(42)
    if head[:4] != b"fLaC" or head[4] & 0x7F != 0:
        sys.exit(f"bench: {path} has no STREAMINFO where a FLAC's is")
    info = int.from_bytes(head[18:26], "big")
    rate = info >> 44
    samples = info & ((1 << 36) - 1)
    return samples / rate


def message(answer):
    """An error's message, from the server's JSON, or the answer's start"""
    try:
        return json.loads(answer)["error"]["message"]
    except (ValueError, KeyError, TypeError):
        return answer[:200].decode("utf-8", "replace") or "nothing"


def post(url, path, key):
    """The file as OpenAI's multipart form, response_format=pktito_json: the
    status, the answer's bytes, and when it was sent and answered"""
    boundary = uuid.uuid4().hex
    with open(path, "rb") as f:
        audio = f.read()
    body = b"".join(
        [
            f'--{boundary}\r\nContent-Disposition: form-data; name="response_format"\r\n\r\npktito_json\r\n'.encode(),
            f'--{boundary}\r\nContent-Disposition: form-data; name="file"; filename="{os.path.basename(path)}"\r\n'
            "Content-Type: audio/flac\r\n\r\n".encode(),
            audio,
            f"\r\n--{boundary}--\r\n".encode(),
        ]
    )
    req = urllib.request.Request(
        f"{url}/v1/audio/transcriptions", data=body, method="POST"
    )
    req.add_header("Content-Type", f"multipart/form-data; boundary={boundary}")
    if key:
        req.add_header("Authorization", f"Bearer {key}")
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=3600) as r:
            status, answer = r.status, r.read()
    except urllib.error.HTTPError as e:
        status, answer = e.code, e.read()
    return status, answer, t0, time.monotonic()


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--url", default="http://127.0.0.1:8000")
    ap.add_argument("--passes", type=int, default=5)
    ap.add_argument("--at-once", type=int, default=4, help="requests in flight (4)")
    ap.add_argument("--dir", default=os.path.expanduser("~/.cache/alicebench-v1"))
    ap.add_argument("--key", default=os.environ.get("PKTITO_API_KEY"))
    ap.add_argument(
        "--save", metavar="DIR", help="each chapter's answer into DIR/NAME.json"
    )
    a = ap.parse_args()
    url = a.url.rstrip("/")

    fetch(a.dir)
    paths = [os.path.join(a.dir, name) for name, _ in CHAPTERS]
    seconds = {p: flac_seconds(p) for p in paths}
    try:
        with urllib.request.urlopen(f"{url}/v1/models", timeout=10) as r:
            models = [m["id"] for m in json.load(r)["data"]]
    except (OSError, ValueError, KeyError) as e:
        sys.exit(f"bench: no pktito-server at {url}: {e}")
    print(
        f"server {url}, model {models[0]}; {len(paths)} chapters, "
        f"{sum(seconds.values()) / 3600:.2f} h of audio a pass, {a.passes} passes, {a.at_once} at once"
    )

    shortest = min(paths, key=seconds.get)
    status, _, t0, t1 = post(url, shortest, a.key)
    if status != 200:
        sys.exit(f"bench: the first request was answered {status}")
    print(f"first, untimed: {os.path.basename(shortest)} in {t1 - t0:.2f} s")

    jobs = [(n, p) for n in range(a.passes) for p in paths]
    start = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=a.at_once) as pool:
        done = list(pool.map(lambda job: (job, post(url, job[1], a.key)), jobs))
    wall = time.monotonic() - start

    answers, failed, why, took = {}, collections.Counter(), b"", []
    if a.save:
        os.makedirs(a.save, exist_ok=True)
    for (n, p), (status, answer, t0, t1) in done:
        if status != 200:
            failed[status] += 1
            why = why or answer
            continue
        answers.setdefault(p, set()).add(hashlib.sha256(answer).hexdigest())
        took.append((t1 - t0) / seconds[p])
        if a.save and n == 0:
            name = os.path.basename(p).rsplit(".", 1)[0] + ".json"
            with open(os.path.join(a.save, name), "wb") as f:
                f.write(answer)
    # A figure over the requests answered times some other load than the one
    # asked for: 24 at once against --max-requests 16 had 44 of 60 turned
    # away (503), and the 16 answered made 1295.7x (c8a.24xlarge, 2026-11-04)
    if failed:
        statuses = ", ".join(f"{status}: {n}" for status, n in sorted(failed.items()))
        print(
            f"{len(done)} requests, {sum(failed.values())} not 200 ({statuses}): no figure"
        )
        print(f"the first said: {message(why)}")
        return 1
    audio = sum(seconds[p] for _, p in jobs)
    print(
        f"{len(done)} requests, all 200, in {wall:.1f} s: "
        f"{audio / 3600:.2f} h of audio, {audio / wall:.1f}x real time"
    )
    print(
        f"a request's time over its audio's: median {statistics.median(took):.4f}, "
        f"longest {max(took):.4f} (with {a.at_once} sharing the server)"
    )
    varied = [os.path.basename(p) for p, h in answers.items() if len(h) > 1]
    print(
        "each chapter's answer the same bytes every pass"
        if not varied
        else f"answers that differ between passes: {', '.join(varied)}"
    )
    return 1 if varied else 0


if __name__ == "__main__":
    sys.exit(main())
