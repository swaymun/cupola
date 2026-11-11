# iOS diagnostics

| Profile | Scheme / configuration | Included behavior |
| --- | --- | --- |
| Release | `Wonder` / `Release` | Product UI; no recorder, diagnostic UI, uploader, fixtures or scenario runner |
| Diagnostics | `Diagnostics` / `Diagnostics` | Product UI plus bounded local recording, native signposts, MetricKit subscription, capture controls and the test driver |

`WONDER_DIAGNOSTICS` is independent of `DEBUG`. Both profiles use Release optimization
and produce matching dSYMs.

Verify that a Release export carries no diagnostic machinery by comparing the two app
bundles:

```sh
python3 scripts/check-ios-profiles.py path/to/diagnostics/Wonder.app path/to/release/Wonder.app
```

## Recording and privacy

In a Diagnostics build, Settings → Diagnostics shows the recorder. The journal holds
operation categories, durations, counts, byte counts, build, device and OS conditions,
session IDs, numeric system metrics and symbol offsets. It excludes conversation text,
prompts, tool output, credentials, filenames and raw URLs. Select a paired computer to
upload reports (`POST /api/v1/diagnostics/batches`, authenticated like every other
call). Analyze a phone export or a computer's report directory:

```sh
python3 scripts/analyze-ios-diagnostics.py /path/to/diagnostics --json /path/to/metrics.json
```

Use `--session UUID` to isolate one run. Reports separate network, decoding,
projection and local interaction timings (count, p50, p95, maximum).
Do not attach Diagnostics archives to issues; see [SECURITY.md](../../SECURITY.md).
