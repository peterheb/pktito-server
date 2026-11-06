# pktito-server

OpenAI's speech-to-text API (`POST /v1/audio/transcriptions`) over
libpktito: NVIDIA's Parakeet TDT
0.6B v2 (English) and v3 (25 European languages) on the CPU. It is meant for
a machine with other work to do, an LLM beside it, say. The server takes the
share of the cores it is given, and requests share that between them. Each
request's answer is the transcript `pktito-c` gives the same recording:
its `--json` byte for byte, and its text in OpenAI's formats.

```
$ pktito-server -m parakeet-tdt-0.6b-v3.pkf -t 8
$ curl -s -F file=@talk.wav localhost:8000/v1/audio/transcriptions
{"text":"And says, how do I get to Dublin? And the answer that comes back is, ..."}
```

## Build

It needs libpktito 0.9.6, whose core/pktito.h has the step-wise
transcription, `pk_steps`, and `pk_sentences`:

```sh
cd libpktito && make libpktito.a
cd ../pktito-server && cargo build --release       # PKTITO_DIR=... for a tree not at ../libpktito
```

It runs on Linux and macOS. Windows is not supported: the WAV reader takes
an upload through `fmemopen`, which Windows does not have.

DOCKER.md runs it in a container, from the image on GitHub's registry
(linux/amd64 and linux/arm64) or one built here. The registry's is built
by .github/workflows/image.yml, against the libpktito commit it names.

## Run

A model is one of libpktito's `.pkf` files. https://pktito.net/docs/models
links v2 and v3 on models.pktito.net, with their SHA-256s, and libpktito's
README, "Then a model", makes them from NVIDIA's checkpoints.

| option               |                                                                                                        |
| -------------------- | ------------------------------------------------------------------------------------------------------ |
| `-m FILE.pkf`        | model to serve (repeatable); the first is the default                                                  |
| `-t N`               | total threads (default: one per core); a multiple of 4 gives every worker 4                            |
| `-w N`               | workers, the threads dealt among them (default: one per 4 threads); a worker hears one chunk at a time |
| `-l ADDR`            | address to listen on (127.0.0.1:8000)                                                                  |
| `--api-key KEY`      | require `Authorization: Bearer KEY` on the API, `/v1` (or `PKTITO_API_KEY`)                            |
| `--max-upload-mb MB` | largest upload (256)                                                                                   |
| `--max-hours H`      | longest recording (4)                                                                                  |
| `--max-requests N`   | most requests in progress (16); more get 503                                                           |
| `--fast`             | up to 7% faster; transcripts may differ slightly                                                       |
| `--ffmpeg PATH`      | ffmpeg 6.0 or later, for formats other than WAV and AIFF (none: those alone); see "Audio"              |

`RUST_LOG=debug` says more. At `info`, each request logs its audio, its
time and its speed.

## How requests share the threads

The `-t` threads are dealt among `-w` workers, by default a worker for
every 4 threads, as libpktito's lanes take a transcriber's threads. Where
they don't divide evenly, some workers have a thread more than the others:
14 threads make 3 workers, of 5, 5 and 4. With `-w` given, the server says
so as it starts. Each worker has a transcriber of its own for each model,
and the model itself is shared.

A request's work is a series of jobs:

- **The plan.** The upload is decoded, then Silero's VAD runs over it and
  the chunks are cut (`pk_steps_new`), on the threads of the worker that
  takes the job.
- **Its chunks.** Each chunk is a job, and so is each stretch that
  libpktito's retry hears again (`pk_steps_job`). Each is heard with
  `pk_asr_run` on whichever worker is free.

A free worker takes the next job of the request whose turn it is, and that
request's turn goes to the back. So the requests share the workers a job at
a time, a chunk of 30.72 s at most:

- A short request sent behind long ones waits for one chunk on a worker,
  not for the long requests to finish.
- A request alone has every worker, its chunks heard several at once, as
  libpktito's lanes hear them.

Chunks finish in any order. `pk_steps_take` hands them back in order, and
the transcript has the bits `pk_asr_transcribe` gives. A recording's
retries come one at a time, ahead of its next chunk: each spends from the
recording's budget for retries, 20% of its length (libpktito's README,
"When the model drops speech").

What this costs a request that is alone:

- **A recording of one chunk** is heard on one worker's threads, not on
  all of them.
- **With `-w 1`**, a single worker with every thread, a one-chunk request
  goes as fast as through `pktito-c`. A longer one is
  then heard a chunk at a time, without the lanes `pktito-c` would hear it
  in. The lanes ran AliceBench 1.22 times as fast on a 9950X's 16 threads
  (libpktito's core/pktito.h). With workers of 4 threads, the default, a
  long request's chunks spread over the workers much as `pktito-c`'s lanes
  take them, a lane for each four threads.
- **The plan** runs on one worker while the others wait. Its VAD is about
  3.5% of a run: 0.007 s against 0.195 s on test.wav on a 9950X
  (libpktito's backend/avx2/CLIMB.md, 2026-11-02).

### On a GB10

These were measured on a DGX Spark's GB10 (10 Cortex-X925, 10 Cortex-A725)
on 2026-11-03, with its LLM stopped: `-t 20`, the v2 model, libpktito's
neon-sve backend, gcc 13.3.0. AliceBench chapters 1 to 8 were sent at once,
two rounds each. x is the audio's length over the batch's wall time:

| `-w` | threads each | 1 request | 2        | 4        | 8        | a 60 s request behind 4 chapters |
| ---- | ------------ | --------- | -------- | -------- | -------- | -------------------------------- |
| 1    | 20           | 117-122x  | 117x     | 125-127x | 119-126x | 4.13 s                           |
| 2    | 10           | 172-180x  | 178-182x | 180x     | 176x     | 2.98 s                           |
| 4    | 5            | 203-212x  | 212-215x | 218-220x | 220-221x | 2.27 s                           |
| 5    | 4            | 197-211x  | 211-218x | 211-215x | 215x     | 2.64 s                           |

- **Requests at once cost nothing.** Each row is level from 1 request to 8.
- **pktito-c** heard chapters 1 to 4 alone at 201 to 218x on the same 20
  threads.
- **One worker with every thread is the slowest.** One chunk keeps few of
  20 threads busy, and each pass waits on the A725s. On chapter 2, one
  10-thread worker on the X925s alone took 4.60 s, against 5.93 s for one
  20-thread worker on all 20 cores.
- **The server matches pktito-c on the same cores.** On the X925s alone, two
  workers of 5 took 4.06 to 4.17 s on chapter 2, and pktito-c took 4.05 to
  4.09 s.

A request alone, the server warm, best of 5, in seconds:

| recording | chunks | `-w 1` | `-w 4` |
| --------- | ------ | ------ | ------ |
| 25 s      | 1      | 0.178  | 0.223  |
| 60 s      | 3      | 0.458  | 0.308  |
| 120 s     | 6      | 0.986  | 0.544  |

So on a GB10, `-w 4`, four workers of 5 threads, is the best of these at
every load. It costs 45 ms against one wide worker on a recording of one
chunk, and nothing from three chunks up. Smaller workers, below, hear more
at once.

### Smaller workers

On 2026-11-06, pktito-server 0.9.6 with the v2 model and `-t` at the
vCPUs gave these, on three EC2 machines of 16 vCPUs and a GB10's 20 cores.
Every answer was the same at every `-w`, and none failed.

- **Alone:** AliceBench's 12 chapters one at a time, the mean of 2 runs
  (within 1.7% of each other).
- **A 30 s clip:** 12 clips, from 300 s into each chapter, one at a time,
  the median of 24 over 2 runs. A clip is one chunk, so one worker's
  threads hear it.
- **At once:** the 12 chapters sent two for each worker at once, at most
  all 12, in one pass. Read it against the same machine's other rows only.

| machine                      | threads a worker | alone  | a 30 s clip | at once |
| ---------------------------- | ---------------- | ------ | ----------- | ------- |
| c9g.4xlarge (Graviton5)      | 4                | 183.1x | 0.53 s      | 196.8x  |
|                              | 2                | 177.3x | 1.03 s      | 200.4x  |
|                              | 1                | 161.6x | 2.03 s      | 200.5x  |
| c8a.4xlarge (Zen 5)          | 4                | 330.8x | 0.29 s      | 356.2x  |
|                              | 2                | 333.0x | 0.53 s      | 387.3x  |
|                              | 1                | 329.0x | 0.88 s      | 407.9x  |
| c8i.4xlarge (Granite Rapids) | 4                | 197.1x | 0.27 s      | 207.1x  |
|                              | 2                | 203.1x | 0.47 s      | 216.9x  |
|                              | 1                | 206.0x | 0.82 s      | 223.8x  |
| GB10                         | 4                | 208.8x | 0.32 s      | 218.5x  |
|                              | 2                | 230.5x | 0.61 s      | 249.8x  |
|                              | 1                | 231.4x | 1.20 s      | 282.0x  |

- **A recording of one chunk takes as long as its worker's threads make
  it.** Against 4 threads a worker, a 30 s clip took 1.7 to 2.0 times as
  long at 2, and 3.0 to 3.9 times at 1.
- **At once, smaller workers hear more:** 2% to 14% more at 2 threads a
  worker, and 2% to 29% at 1, the most on the GB10.
- **Alone, the GB10 is 10% faster** at 2 threads a worker or 1. At 2, c9g
  is 3.2% slower, c8a level and c8i 3.0% faster; at 1, c9g is 12% slower
  and c8i 4.5% faster.

So the default is a worker for every 4 threads. It hears a recording of one
chunk, up to about 30 s, in about half the time 2 threads a worker take,
and a long recording alone is at most 10% slower (the GB10). A server kept
busy hears more with smaller workers: `-w` at half the threads, or at all
of them.

### Threads in fours

Give `-t` as a multiple of 4: every worker then has 4 threads, the width of
libpktito's lanes. On the same GB10, with its LLM stopped and the server not
pinned, `tools/bench.py --passes 3 --at-once 4` gave these (AliceBench
through Ubuntu's ffmpeg 6.1.1, 8.94 hours of audio, every request a 200, one
run each, 2026-11-04):

| `-t` | workers | speed  | a thread |
| ---- | ------- | ------ | -------- |
| 4    | 1 of 4  | 87.0x  | 21.8x    |
| 8    | 2 of 4  | 160.1x | 20.0x    |
| 12   | 3 of 4  | 197.2x | 16.4x    |

Each worker added less than the one before: 73x for the second, 37x for the
third.

### Beside an LLM

These were measured on 2026-11-04 on one of three DGX Sparks serving
GLM-5.3-Flash-NVFP4 together (vLLM, tensor parallel over the three), with
the server on that Spark: workers of 4 threads, the v2 model, not pinned. The three Sparks
step together, so whatever slows one slows the whole model. The LLM had one
stream, one fixed prompt and 512 tokens out. Alone, it ran 23.1 to 23.6
engine steps a second (51 to 53 tokens a second), and each change below is
against the mean of the runs alone in the same session.

| the server                  | runs | the LLM's change   | its speed                | its memory                                      |
| --------------------------- | ---- | ------------------ | ------------------------ | ----------------------------------------------- |
| idle                        | 3    | -0.4% to +1.3%     | no CPU used              | 28-30 MB before its first request, 641 MB after |
| a 10 s recording every 10 s | 2    | -0.2%, 0.0%        | 0.17 s a recording (60x) | 622-641 MB                                      |
| busy, `-t 2`                | 1    | 0.0%               | 37x                      | 842 MB at most                                  |
| busy, `-t 4`                | 3    | 0.0%, -0.9%, -1.4% | 66-70x                   | 857-896 MB at most                              |
| busy, `-t 8`                | 1    | -3.5%              | 124x                     | 945 MB at most                                  |
| busy, no `-t` (20 threads)  | 4    | -4.2% to -7.3%     | 149-153x                 | 1,093-1,117 MB at most                          |

"Busy" is two requests at once, AliceBench chapters 1 and 2 over and over.
The `-t 4` and no-`-t` runs were taken both ways: the server's work
starting under the LLM's stream, and the LLM's stream starting into a
server already busy for a minute. Memory is the server's resident set, the
model's 600 MB of page cache included (Memory, below).

- **Idle and occasional use cost the LLM nothing measurable.** A server
  waiting for requests uses no CPU. At a recording every 10 s, six a minute,
  the LLM's speed didn't change. That a slower trickle costs nothing either
  wasn't measured, only inferred. The first request after a start took
  0.21 s once and 0.86 s once.
- **Busy, the cost grows with `-t`.** With no `-t`, the server's threads
  took every core, and the LLM's threads had to wait for one. Leaving out
  one light thread that wakes about 4,000 times a second, they waited about
  60 ms of every second in all. That's about 2.7 ms a step, close to the 2
  to 3 ms its steps of about 40 ms slowed by. At `-t 4` they waited no
  longer than with the LLM alone. Where `-t 8`'s 3.5% comes from isn't
  known: in its one run, the LLM's threads didn't wait.
- **Eight streams lose nothing.** Earlier that day, the LLM at 8 streams
  was as fast beside the server at `-t 8`, `12` and `16` as alone (87.8 to
  92.8 tokens a second, against 87.3 and 88.3).
- **The LLM was slower still in 11 of 16 earlier runs, and why isn't
  known.** At 1 stream it lost 15% to 17% beside a busy server: unpinned in
  4 of 4 runs (`-t 8`, `12` and `16`), on 10 and 13-19 in 6 of 10, and on
  6-8 and 15-19 in 1 of 1. It hasn't come back since 13:05 that day, in 13
  runs in a row, and every figure in the table above was taken after it
  stopped.
- **Pinning isn't recommended.** On 10 and 13-19, one cluster, the LLM still
  lost 15% to 17% in 6 of 10 runs. The server's two workers of 4 there mix
  X925s and A725s, and each pass waits on an A725, so with the LLM idle it
  ran at 116x against 161x unpinned.

So, on a machine shared with an LLM:

- Give the server a few cores. `-t 4` cost the LLM 1.4% at most here, and
  still heard 66-70 times real time.
- Don't pin it. Under a CPU mask anyway (taskset, systemd's
  `CPUAffinity=`, a container's cpuset), set `-t` too: libpktito counts only
  the cores in the mask from acb7d1d on, and an older tree counts them all.
- Measure your own. The LLM, its streams and the machine change all of
  these figures, and the 15% to 17% above was never explained.

A systemd unit for that, with the request limits from Memory, below:

```ini
[Unit]
Description=pktito-server: speech to text on the CPU
After=network.target

[Service]
ExecStart=/usr/local/bin/pktito-server -m /srv/pktito/parakeet-tdt-0.6b-v2.pkf -t 4 \
    --max-requests 4 --max-hours 1 --max-upload-mb 128 -l 127.0.0.1:8000
DynamicUser=yes
MemoryMax=3G
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

Those limits bound the server at about 2.1 GB: the model's 600 MB, a
worker's 70 MB, and 1.4 GB for four requests. If it ever went past 3 GB,
`MemoryMax=` would stop the server, not the LLM.

## Memory

What the server holds:

- **The model**, mapped read-only from its file: about 600 MB for v2,
  shared by every worker, and page cache the kernel can drop and read back.
- **Each worker's transcriber**, about 70 MB: what the encoder works in for
  a chunk of 30.72 s.
- **Each request**: its upload until it is decoded, then its samples at
  16 kHz, 230 MB an hour of audio. A 16-bit WAV is 115 MB an hour.
- **Each worker's ffmpeg**, with `--ffmpeg`, while it decodes: 512 MiB at
  most (its limit; macOS refuses the limit, so there it has none), 60 MB for
  an hour of AAC.

On a GB10 (workers of 4 threads, the v2 model, AliceBench's chapters 1 and 2 over and
over), the memory that is the server's own, beyond the model's 601 MB, was
251 MB at `-t 8` a request at a time, 297 MB two at a time, and 433 MB at
`-t 16` two at a time. Through 10,000 requests on a 9950X (`-t 16 -w 4`, 8 at
once: recordings of 5 to 60 s in every format, decoded then in the server by
symphonia, uploads that are not audio, clients that hang up), its resident
memory, the model's included, stayed between 878 and 895 MB, and it ended
with no request in hand.

A request holds one of `--max-requests` slots from before its upload is read
until it is done, so what the requests hold has a bound: about
`--max-requests` × (`--max-upload-mb` + 230 MB × `--max-hours`), beyond the
model and the workers. At the defaults that is 16 × (256 MB + 922 MB), about
19 GB, so lower the limits to what the machine can spare.

On a DGX Spark, the GPU's memory is the system's, and what the server may use
is what the LLM leaves. Running out there stops processes without warning,
or sets the machine swapping. Size the limits to MemAvailable with the LLM
loaded: `--max-requests 4 --max-hours 1 --max-upload-mb 128` comes to about
1.4 GB.

## The API

`POST /v1/audio/transcriptions` takes a multipart form:

| field                       |                                                                                                                                                                                                  |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `file`                      | the recording (see "Audio" below)                                                                                                                                                                |
| `model`                     | a model's name, from `GET /v1/models`. Any other name (an OpenAI client's `whisper-1`) gets the first model that writes `language`, or the first model given                                     |
| `language`                  | ISO 639-1 (`en`); a locale (`en-US`) is its language, and `auto` is none. A model that does not write it is a 400. Parakeet finds the language itself, so this only picks the model              |
| `response_format`           | `json` (the default), `text`, `srt`, `vtt`, `verbose_json`, `pktito_json`                                                                                                                        |
| `timestamp_granularities[]` | with `verbose_json`: `segment` (the default), `word`, or both                                                                                                                                    |
| `stream`                    | `true`: server-sent events, OpenAI's `transcript.text.delta` for the sentences each chunk ends, in order (a sentence that runs on waits for the chunk that ends it), then `transcript.text.done` |
| `retry`, `context`          | pktito's own, both on by default. `false` is pktito-c's `--no-retry` or `--no-context`; with both off, each chunk is NeMo's transcript of it                                                     |
| `prompt`, `temperature`     | read and left: a transducer has nothing they set                                                                                                                                                 |

**Audio.** WAV and AIFF go through libpktito's own reader and resampler, the
ones pktito-c uses: any rate, any channel count, brought to 16 kHz mono. A
WAV of two channels at 22.05 kHz gives the same `pktito_json` bytes as
pktito-c's `--json` for it (tests/api.rs). Anything else is read only
through the ffmpeg given with `--ffmpeg PATH`, 6.0 or later; without one it
is a 400 that says so. The server looks for no ffmpeg on PATH, and nothing
updates the one it is given: its build, codecs and fixes are the admin's.

Each upload gets an ffmpeg of its own (src/ffmpeg.rs), with only what
decoding needs, so that what a damaged file does to it stays there:

- the upload as its standard input, a file in memory that it can seek in,
  as an MP4 indexed at its end needs, read through ffmpeg's `fd:` protocol
  alone, so that it opens no path and no address;
- no environment, so not the API key, nor the server's through /proc, which
  are root's while the server, not dumpable with `--ffmpeg`, runs; and no
  descriptors but its three, whose files are mode 0, so that no other
  process of the server's user, another ffmpeg among them, can open them
  through /proc;
- 512 MiB of memory (RLIMIT_DATA, which macOS refuses; ffmpeg 8.0 decoded an
  hour of AAC or Opus in 128), 60 s of CPU and 120 s more an hour of
  `--max-hours`, no file written, no core, one decoder thread;
- and it is stopped when the audio passes `--max-hours`, when it writes
  nothing for 30 s, or when it has run for twice its CPU seconds.

It writes the first audio stream as float WAV, which libpktito's reader
takes as it takes an uploaded WAV: a FLAC of test.wav gives test.wav's
`pktito_json` byte for byte (tests/api.rs). Its arguments are options ffmpeg
has long had, so that it can be updated apart from the server: `-nostdin
-hide_banner -loglevel error -threads 1 -protocol_whitelist fd -i fd: -map
0:a:0 -c:a pcm_f32le -f wav pipe:1`. On macOS, which has no memfd, the
upload goes to it through a temporary file, unlinked once open. macOS
refuses the memory limit, so there ffmpeg's memory has no bound. It applied
the CPU limit late to a stand-in that forked as it wrote (22.7 s of CPU at
10 s, where one that did not fork stopped at 2.0 s of 2), and the overall
time stopped that one (M4 Pro, 2026-11-04).

An ffmpeg that a crafted upload took over would still run as the server's
user, on its network: in Docker as DOCKER.md runs it, a stand-in for one
reached the server's port (2026-11-04). Where the kernel lets a process
trace others of its user (Yama's ptrace_scope 0; it was 1 on the GB10s and
under WSL2), it could also read another ffmpeg's memory, unless the
server's user can run ffmpeg's file but not read it, as DOCKER.md's
`chmod 711` makes it: Linux runs such a program not dumpable, with its /proc
entries root's. The key keeps uploads to the clients that have it.

The decoder the server had in its own process, symphonia 0.5.5, panicked on
17,713 of 74,301 damaged files and took 4 to 16 GB on others of 53 KB
(fuzzed 2026-11-04). That fuzz kept 16,433 inputs: 429 on which symphonia
panicked, 15,978 on which it allocated over 256 MB at once, and 26 others.
Through ffmpeg 8.0.1, 8 at a time (`-t 8 -w 4`), this server heard 4,876 of
them, as far as ffmpeg could read them, and refused 11,557 with a 400 giving
ffmpeg's reason, 136 of them its memory limit. None was a 500 or a dropped
connection, none took over 0.3 s, the server's memory peaked at 659 MB, and
test.wav after them gave pktito-c's bytes.

**The formats:**

- `json` is `{"text": ...}`: the transcript's sentences, as `pk_sentences`
  finds them and pktito-c prints them a line each, a space between them.
- `text` is that text and a newline.
- `srt` and `vtt` are pktito-c's `--srt` and `--vtt`: libpktito's caption
  cues, one or two lines of 42 characters.
- `verbose_json` is OpenAI's: `task`, `language` (its English name),
  `duration`, `text`, and `segments`, `words` or both:
  - A segment is a sentence, as libpktito's `pk_sentences` finds them
    (a sentence over 30 s broken at a comma or a pause): `id`, `start`,
    `end`, `text`, `tokens`. Its times are its first word's start and its
    last word's end.
  - A word is `word`, `start`, `end`.
  - Whisper's `avg_logprob`, `compression_ratio`, `no_speech_prob`, `seek`
    and `temperature` are left out: a transducer has nothing they measure.
- `pktito_json` is pktito-c's `--json`, byte for byte: every chunk's start,
  end, text, and its words and tokens with their times, then `meta`.

**The other endpoints:**

- `POST /v1/audio/vad` takes `file` (and `model`, for its VAD). It answers
  with pktito-c `segments`' speech, Silero's defaults, in seconds:
  `{"duration": 60.0, "segments": [{"start": 0.002, "end": 2.046}, ...]}`
  for test.wav.
- `GET /v1/models` and `GET /v1/models/{id}` are OpenAI's; `owned_by` is
  the model file's credit.
- `/health`, `/ping`, `/version` and `/metrics`, under Monitoring, below.

**Connections.** A client has 30 s to send a request's line and headers,
from when it connects or from its last answer, so a connection left idle is
closed. An upload that stops, with no bytes of it for 30 s, is answered 408
and its slot given back. A request takes its slot before its upload is read,
so a server with every slot taken answers 503 before the file is sent. A
connection closes in stages: the client's bytes are read until it closes,
for 30 s at most, so a client that sends all of its file before it reads
still gets that 503 instead of a reset. A job that fails inside the server
answers its own request with a 500, and its worker goes on.

**Errors** are OpenAI's, `{"error": {"message", "type", "param", "code"}}`:

- 400: a form or an upload the server cannot take.
- 401: no key, or the wrong one.
- 404: a model the server does not have.
- 408: an upload that stopped coming.
- 413: an upload over `--max-upload-mb`.
- 500: a job that failed inside the server; the log says why.
- 503: every one of `--max-requests` taken, by requests in hand or uploads
  coming.

With OpenAI's Python client:

```python
from openai import OpenAI

client = OpenAI(base_url="http://localhost:8000/v1", api_key="KEY")
with open("talk.wav", "rb") as f:
    print(client.audio.transcriptions.create(model="parakeet-tdt-0.6b-v3", file=f).text)
```

## Monitoring

The key is the API's, `/v1`. These four need none, as vLLM's need none, so
that a probe or a scraper holds no secret; SGLang's health and metrics need
none either. They tell what the server runs and how busy it is, never what a
request sent. To keep them from others, publish the port to the clients and
monitors alone (DOCKER.md's `-p`).

- `GET /health` answers 200 while a worker can hear a request, and 503 when
  none can. A worker can't when no transcriber can be made for it, memory or
  threads having run out, or when its thread has ended:

  ```json
  {
    "status": "ok",
    "workers": 3,
    "worker_threads": [5, 5, 4],
    "workers_busy": 1,
    "workers_failed": 0,
    "requests": 2,
    "max_requests": 16,
    "fast": false,
    "backend": "avx512-vnni (...)",
    "compiler": "gcc 15.2.0"
  }
  ```

  `status` is `ok` when every worker can, `degraded` when some can't, and
  `unavailable` with the 503. `requests` are the requests in hand, their
  uploads coming in or their audio being heard.

- `GET /ping` is `/health`'s status alone, with no body, as vLLM's and
  SGLang's `/ping` and `/health` are.
- `GET /version` is the server's version and libpktito's:
  `{"version": "0.9.6", "libpktito": "0.9.6"}`.
- `GET /metrics` is Prometheus's text format. The names are the server's and
  the measure's, as vLLM's and SGLang's are, and vLLM's own where the
  measure is the same:

| metric                                                   |                                                                                                                            |
| -------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| `pktito:audio_seconds_total{model_name}`                 | audio transcribed, in seconds. Its rate is the speed: `rate(pktito:audio_seconds_total[5m])` at 200 is 200 times real time |
| `pktito:audio_pending_seconds`                           | audio decoded and not yet transcribed, the work in hand. Over the speed, how long it will take                             |
| `pktito:e2e_request_latency_seconds{model_name}`         | a histogram of the transcriptions' times, each from its upload in hand to its last chunk heard                             |
| `pktito:num_requests_running`                            | requests begun: decoded, cut into chunks, or their chunks heard                                                            |
| `pktito:num_requests_waiting`                            | requests holding a slot and not yet begun, their uploads coming in or in hand                                              |
| `pktito:max_requests`                                    | `--max-requests`, the slots: one more request is a 503                                                                     |
| `pktito:http_requests_total{route,code}`                 | the API's answers, by route and status                                                                                     |
| `pktito:workers{state}`                                  | the workers `idle`, `busy` or `failed`                                                                                     |
| `pktito:threads`                                         | `-t`                                                                                                                       |
| `pktito:server_info{version,libpktito,backend,compiler}` | always 1                                                                                                                   |
| `process_start_time_seconds`                             | when the server started                                                                                                    |

```yaml
scrape_configs:
  - job_name: pktito
    static_configs:
      - targets: ["127.0.0.1:8000"]
```

## Benchmarking

`tools/bench.py` times a running server on AliceBench: the twelve chapters
of LibriVox's _Alice's Adventures in Wonderland_, 2:58:51 of 16 kHz FLAC,
downloaded (170 MB) and checked by SHA-256 the first time. It sends them 5
times over with 4 requests at once, after one untimed, and gives the hours
of audio over the time they took. The server reads them through its ffmpeg:

```sh
pktito-server -m parakeet-tdt-0.6b-v2.pkf --ffmpeg /usr/bin/ffmpeg &
python3 tools/bench.py              # --passes, --at-once, --url, --dir, --save
```

Each chapter's answer must be the same bytes every pass. Each request in
flight takes one of the server's `--max-requests` slots (16 unless set), and
one beyond them is a 503: bench.py then names the statuses and gives no
figure, so give the server at least `--at-once`. Through `--save`, the
answers are `pktito-c --json`'s for the chapters' WAVs, byte for byte, all
12 (a 9950X, 2026-11-04). Run it on the server's own machine
with nothing else busy, and give the server's `-t` and `-w` with the figure.
It needs Python 3.8 or later and nothing else.

## Checking

`PKTITO_DIR=../libpktito cargo test --release` needs the tree's models
(models/parakeet-tdt-0.6b-v2.pkf and v3) and its built `pktito-c`. The tests
hold:

- **tests/api.rs**, a request at a time:
  - `pktito_json` of test.wav against libpktito's tests/data/test.v2.json,
    test.v3.json and the two fast-mode files, byte for byte, and of a
    stereo 22.05 kHz WAV against pktito-c's `--json`.
  - `srt` and `vtt` against pktito-c's `--srt` and `--vtt`.
  - The VAD against `pktito-c segments`.
  - `json`, `text`, `verbose_json` and the stream: pktito-c's lines and
    word times, the stream with both models.
  - The errors, the key and the models.
  - `/health`, `/ping`, `/version` and `/metrics` without the key, a
    transcription's answer, audio and time in the metrics after it, and the
    page as Prometheus's format has it.
  - An upload neither WAV nor AIFF, without ffmpeg: a 400 that names
    `--ffmpeg`.
  - Through ffmpeg (`PKTITO_FFMPEG`, or the one on PATH; skipped without one
    of 6.0 or later), on one worker: test.wav as FLAC, test.wav's
    `pktito_json` byte for byte; as AAC in an MP4 indexed at its end, and as
    Opus in WebM, heard; a damaged M4A (tests/data/README), a 400, and the
    next request heard as pktito-c would; a recording too long, a 400.
- **tests/engine.rs**, requests at once:
  - Four two-minute requests in chunks of 10 s on three workers, each
    against pktito-c's `--json`.
  - A short request sent last is done before the long ones.
  - A request whose caller goes is dropped.
  - A recording too long, an upload not audio, and a server with every
    request slot taken.
  - A slot held from before its upload, and given back when the upload
    never fills it.
  - A job that panics fails its own request, and the one worker then hears
    the next.
  - The audio heard, every second of it, and each request's time; a
    recording with no speech heard to its end at once; the health by the
    workers.
- **tests/serve.rs**, the connections, on a port, with timeouts of a second:
  - A client that sends nothing, or half its headers, is closed, and so is
    one left idle after its answer.
  - An upload that stops is answered 408, and its slot comes back.
  - With both slots held, 50 uploads of 50 MB are each answered 503 after
    64 KB of them, and a client that sends all 16 MB of its upload before
    it reads still gets its 503.
- **The unit tests**: Rust's `{:.2}` and `{:.3}` against the C library's
  `printf`, over the times a transcript has, the ties, and a million more
  values.

`scripts/fmt.sh` puts every source file in its language's standard form,
each formatter at its defaults (cargo fmt, prettier 3.6.2, ruff 0.14.3); run
it before each commit. `scripts/fmt.sh --check` changes nothing and exits 1
if a file isn't formatted.

## License

The code is MIT, the same as libpktito's. The weights in a `.pkf` are
NVIDIA's, under CC-BY-4.0. LICENSE has both.
