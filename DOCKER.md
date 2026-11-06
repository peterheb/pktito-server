# Running pktito-server in Docker

The image holds only the server. The model stays on the host and is mounted read-only. Run the commands below in a directory of their own, which will hold the model and the key: they leave the server on this machine alone, behind that key.

1. **Get the image.** GitHub's registry has it for x86-64 and 64-bit Arm, and Docker pulls the one for this machine. libpktito picks its kernels by the CPU when the server starts, so it runs on any machine of its architecture. The `tag` gives it the name the steps below use.

   ```sh
   docker pull ghcr.io/peterheb/pktito-server:0.9.6
   docker tag ghcr.io/peterheb/pktito-server:0.9.6 pktito-server
   ```

   Or build it, on the architecture you will run on: put `libpktito` and `pktito-server` side by side here, with the server's `Dockerfile` and `Dockerfile.dockerignore` in `pktito-server/`. The libpktito needs to be 0.9.6, as the README's Build says, and the build needs BuildKit, the default from Docker 23.

   ```sh
   docker build -f pktito-server/Dockerfile -t pktito-server .
   ```

2. **Get a model.** models.pktito.net has the English model, v2, and the multilingual one, v3, and https://pktito.net/docs/models lists both with their SHA-256s. This fetches v2 into `models` and checks it; `shasum -a 256 -c -` does the same check on a Mac. The model must be readable by everyone, since the server runs as uid 65532.

   ```sh
   mkdir -p models
   curl -fL -o models/parakeet-tdt-0.6b-v2.pkf https://models.pktito.net/8bfa6750b83c/parakeet-tdt-0.6b-v2.pkf
   echo "8bfa6750b83cd7c447bad82057c7a12473563b72b3d35487fcca6da0786d83c7  models/parakeet-tdt-0.6b-v2.pkf" | sha256sum -c -
   chmod -R a+rX models
   ```

   For v3, fetch https://models.pktito.net/35a59c9a433d/parakeet-tdt-0.6b-v3.pkf, check it against `35a59c9a433d4da2289640db93e73c42478b576e49a293090ef61b4ae595152a`, and serve `/models/parakeet-tdt-0.6b-v3.pkf` below. Or make the models yourself from NVIDIA's checkpoints, with Python and numpy: libpktito's README, "Then a model".

3. **Create an API key.**

   ```sh
   (umask 077; echo "PKTITO_API_KEY=$(openssl rand -hex 24)" > pktito.env)
   ```

4. **Run it.** Choose `-t` from the README's table.

   ```sh
   docker run -d --name pktito-server --restart unless-stopped --init \
     --memory 3g --memory-swap 3g --read-only --cap-drop ALL \
     --security-opt no-new-privileges --env-file pktito.env \
     -p 127.0.0.1:8000:8000 -v "$PWD/models:/models:ro" \
     pktito-server -m /models/parakeet-tdt-0.6b-v2.pkf -t 8 \
     --max-requests 4 --max-hours 1 --max-upload-mb 128
   ```

   The memory limit includes the model's page cache. It makes the kernel stop this container, not its neighbours, if a large upload pushes it too far. The request limits keep the requests under it. They hold about `--max-requests` × (`--max-upload-mb` + 230 MB × `--max-hours`), 1.4 GB here (the README's Memory). At the defaults that is 19 GB, so raise the limits and `--memory` together.

   `--init` lets `docker stop` stop the server at once. The server has no handler for SIGTERM, and as the container's first process it ignores the signal until Docker kills it. On Docker Desktop that took 3.5 s and ended in SIGKILL (exit 137); with `--init` it took 0.5 s. The server writes no files and needs no privileges, so the container gets neither.

   The image listens on port 8000 on all of the container's addresses, and `-p` decides who can reach it: here, this machine alone. Passing `-l` stops the server, because the image already sets it. To listen elsewhere inside the container, put `--entrypoint pktito-server` before the image and pass `-l` with the other arguments.

5. **Check it.**

   ```sh
   curl -fLO https://raw.githubusercontent.com/peterheb/libpktito/v0.9.6/tests/data/test.wav
   curl -s 127.0.0.1:8000/health
   curl -s -H "Authorization: Bearer $(cut -d= -f2 pktito.env)" \
     -F file=@test.wav 127.0.0.1:8000/v1/audio/transcriptions
   ```

   test.wav is libpktito's test recording. `/health` needs no key; transcription does, and is a 401 without it. `/health` should report the workers and their threads (2 workers of 4 threads at `-t 8`).

   The image has no shell and no `curl`, so a `HEALTHCHECK` that runs `curl` cannot run inside it, and `docker ps` shows no health. Probe `/ping` or `/health` from outside the container instead: an uptime monitor, a load balancer, or Kubernetes' `httpGet`. They need no key, and neither does `/metrics` for Prometheus (the README's Monitoring).

## Serving other machines

The server above answers this machine alone. To serve others, replace `127.0.0.1` in `-p 127.0.0.1:8000:8000` with the address of this machine that they reach it at, for example `-p 192.168.1.20:8000:8000`, and run the container again (`docker rm -f pktito-server`, then step 4). Keep the key: anyone who can reach that address can send it uploads.

## Formats other than WAV and AIFF

The server reads WAV and AIFF itself. Anything else (FLAC, MP3, M4A, Ogg, a browser's WebM, a video's sound) it reads only through an ffmpeg you give it, 6.0 or later. Without one, such an upload is a 400 that names `--ffmpeg`.

The image has none. The ffmpeg is yours: you choose the build, and you update it for speed, codecs or security fixes. Nothing updates it for you. The server uses only options ffmpeg has long had, so a newer ffmpeg can replace an older one. Each upload gets its own ffmpeg process, held to what decoding needs (the README's Audio): the upload in memory and no path or address to open, no environment and so no key, 512 MiB of memory, CPU seconds by `--max-hours`, no file written, and 30 s to write something before it is stopped. An ffmpeg that a crafted file took over would still have the container's network. Keep the key, so that only your clients can send uploads.

1. **Get a self-contained build.** It must need no library but the C library: the image has that, and none of the others a distribution's ffmpeg needs. BtbN's FFmpeg-Builds (https://github.com/BtbN/FFmpeg-Builds/releases) link everything else in, and are rebuilt daily, from each release branch as well as from master. This fetches the 9.0 branch's `lgpl` build, checks it against BtbN's checksums, and puts its `ffmpeg` in a directory `ffmpeg` here, where the server may run it but not read it (below). On Arm, `linuxarm64` replaces `linux64`: on a GB10, that build (n9.0.2) ran under the flags below, and its FLAC and Ogg gave the WAV's transcripts (2026-11-04).

   ```sh
   b=ffmpeg-n9.0-latest-linux64-lgpl-9.0
   curl -fLO https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/$b.tar.xz
   curl -fLO https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/checksums.sha256
   grep " $b.tar.xz\$" checksums.sha256 | sha256sum -c -
   mkdir -p ffmpeg && tar xJf $b.tar.xz -C ffmpeg --strip-components 2 $b/bin/ffmpeg
   chmod 711 ffmpeg/ffmpeg
   ffmpeg/ffmpeg -version | head -1
   ```

   The `chmod` keeps each ffmpeg's memory from the others. The server runs as uid 65532, "other" to the file, so it can run the program but not read it, and Linux runs a program its user cannot read as the server runs itself: not dumpable, with its memory, environment and open files in /proc belonging to root. An ffmpeg that a crafted file took over then cannot read or change another's, whatever the host's Yama ptrace_scope. In Docker on 2026-11-04, another process of uid 65532 was refused all three of a running ffmpeg's at 711; at 755 it read the environment and the open files.

2. **Run the server with it.** The directory is mounted read-only, and `--ffmpeg` names the program in it:

   ```sh
   docker rm -f pktito-server
   docker run -d --name pktito-server --restart unless-stopped --init \
     --memory 3g --memory-swap 3g --read-only --cap-drop ALL \
     --security-opt no-new-privileges --env-file pktito.env \
     -p 127.0.0.1:8000:8000 -v "$PWD/models:/models:ro" \
     -v "$PWD/ffmpeg:/opt/ffmpeg:ro" \
     pktito-server -m /models/parakeet-tdt-0.6b-v2.pkf -t 8 \
     --max-requests 4 --max-hours 1 --max-upload-mb 128 \
     --ffmpeg /opt/ffmpeg/ffmpeg
   docker logs pktito-server 2>&1 | grep 'formats besides'
   ```

   At its start the server runs the ffmpeg under the limits it decodes with, and logs its version. It stops if the ffmpeg does not run, or is older than 6.0.

3. **Update it** by running all of step 1's commands again (with a newer branch in `b` when there is one), the `chmod` with them: tar gives the new file the archive's mode, 755, and the server runs either way without saying so. Then restart, so that the server checks the new build and logs its version:

   ```sh
   docker restart pktito-server
   ```

   tar replaces the file rather than writing into it, so an ffmpeg still running keeps the old one; copying over the file instead fails while one runs ("Text file busy"). Mount the directory, not the file: Docker Desktop could not restart a container whose mounted file had been replaced.

ffmpeg adds up to 512 MiB a worker while it decodes. At `-t 8` there are two workers, and `--memory 3g` still holds them with the request limits above, since the kernel can drop the model's 600 MB of page cache.

The image holds no ffmpeg, so publishing it carries none of ffmpeg's licence terms. The ffmpeg you mount is under its own licence, which its archive states: LGPL 3 for BtbN's `lgpl` builds (their LICENSE.txt), GPL for the `gpl` ones. The server is MIT, and runs ffmpeg as a separate program that it does not link.

## Threads

Without `-t`, the server takes one thread for each core it may run on. Docker's CPU limits reach that count differently:

- `--cpuset-cpus` limits which CPUs run the threads, and the count follows it. CPUs are hardware threads, not cores: on a Ryzen 9 9950X, CPUs 0-3 are two cores, and the server started 2 threads.
- `--cpus` limits CPU time, not cores, and the count does not see it. At `--cpus 4` that machine's server started 16 threads, which share four CPUs' time. Pass `-t` with it.
