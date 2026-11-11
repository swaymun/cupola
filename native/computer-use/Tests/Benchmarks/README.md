# Mac screen encoder benchmark

Run from the repository root on an Apple Silicon Mac. This is an opt-in,
synthetic WebRTC loopback benchmark; it needs no screen-recording permission,
simulator, phone, model request, or running helper. Use an optimized build so
fixture generation does not become a debug-build bottleneck.

```sh
mkdir -p .local/fps0
WONDER_LATENCY_BENCH=1 \
  WONDER_LATENCY_BENCH_OUTPUT="$PWD/.local/fps0/matrix-rows.log" \
  swift test -c release \
  --package-path native/computer-use --scratch-path .local/fps0/build --jobs 4 \
  --filter ScreenLatencyBenchmarkTests > .local/fps0/matrix.log 2>&1
python3 native/computer-use/Tests/Benchmarks/summarize.py .local/fps0/matrix.log \
  --expect-rows 96 --csv .local/fps0/matrix.csv
```

The default matrix has 96 rows: 720p/1080p/1440p × 15/30/60/120 fps ×
`maxFramerate` unset/set × H.264 encoder level 5.1/5.2 × BGRA/420v.
Each fresh peer pair warms up for four seconds, then measures twelve seconds.
Allow about half an hour plus the first build. Every row allocates and writes a
fresh IOSurface-backed pixel buffer on every tick. There is no frame, conversion,
or encoded-output cache. The fixture has deterministic text-like detail, a moving
strip, and a central 16-bit change marker with a randomized 280–360 ms cadence.
The marker prevents delayed or dropped changes from aliasing a later change.

`maxBitrateBps` starts at 1/2/4 Mbit/s at 15 fps for 720p/1080p/1440p and
scales linearly with fps. These are experimental ceilings, not claimed minima
or picture-quality guarantees. This includes P1's 8 Mbit/s ceiling for 1440p30.
Compare lower ceilings before choosing a policy.
The sender explicitly maintains resolution. Level 5.1 uses the production
encoder factory; a test-only wrapper requests 5.2 without changing the publisher
or negotiated SDP. `codec` and `encoder` expose actual fallback; `sps` records
the profile/constraint/level bytes from emitted H.264 SPS NAL units. A requested
level is not evidence that the implementation enforces its macroblock-rate limit.

`WONDER_LATENCY_BENCH_PUBLISHER=1` replaces the matrix with the 12 rows the
production publisher produces (720p/1080p/1440p × 15/30/60/120, 420v): ceiling,
`maxFramerate`, `maxBitrateBps`, maintain-resolution and the H.264 level all come
from `VideoEncodingPolicy` and `WebRTCPublisher.applyEncodingLimits`. Expect
`--expect-rows 12`.

Useful environment options (all runs still execute the test):

| Variable | Meaning |
| --- | --- |
| `WONDER_LATENCY_BENCH_FILTER` | Comma-separated OR substrings, e.g. `720p-120fps,1080p-120fps` or `set-l51-420v` (note `set` also occurs in `unset`; use `fps-set-` for explicit-only selection). Empty matches fail. |
| `WONDER_LATENCY_BENCH_SECONDS` | Measurement window, 8–60 seconds, default 12. |
| `WONDER_LATENCY_BENCH_WARMUP` | Warmup, 3–30 seconds, default 4. |
| `WONDER_LATENCY_BENCH_BITRATE_SCALE` | Multiply ceilings by 0–8; `0` leaves maxBitrate unset. |
| `WONDER_LATENCY_BENCH_PACING=onChange` | P1-style sparse updates on a 60 Hz capture grid. Sparse-source fps is not a throughput qualification. |
| `WONDER_LATENCY_BENCH_LOG=1` | Include WebRTC diagnostics for a selected case. |
| `WONDER_LATENCY_BENCH_OUTPUT` | Optional absolute path for immediate progress rows (Swift's test runner buffers stdout). Must not already exist; use a new path on reruns. The final XCTest log remains the pass/fail authority. |

Every `latency-bench` row prints measured source, outbound, encoded and received
fps; last WebRTC `framesPerSecond`; actual encoder/codec/dimensions; interval
encode/decode/jitter averages; change-latency median/p90; process CPU; fixture
generation time; actual payload bitrate; quality-limitation reason; thermal state
and host load. Outbound fps is the delta of `framesSent` divided by the sender
statistics timestamps, not target fps or the last instantaneous sample.

CPU comes from `getrusage(RUSAGE_SELF)` over the same window; 100% means one CPU
core. It includes this process's encoder, decoder and fixture generation, and
excludes WindowServer and external VideoToolbox services. It is **not helper-only
CPU**. `fixture` is wall time spent allocating and filling each source frame.
Latency ends at the decoded renderer callback, not actual display presentation.
`pendingChanges` counts changes not observed by the end of the interval; latency
percentiles cover received changes only and do not treat missing changes as fast.
BGRA/420v are synthetic capture-buffer formats: ScreenCaptureKit's actual capture
cost, display refresh, network behavior, physical decode, battery and power caps
remain unmeasured. Shared-host load can affect timing.

`pass95` reports whether outbound fps reached 95% of target. A diagnostic matrix
row can pass the test while missing throughput: `reasonInferred` names the
counter-supported hypothesis to investigate. XCTest requires decoded change
samples, outbound video, H.264 and unchanged resolution; it does not hide misses
by skipping a configuration. `sender-or-codec-cap-investigate` needs a paired
comparison/log inspection, not an automatic claim about codec or hardware limits.
