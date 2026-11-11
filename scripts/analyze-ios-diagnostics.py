#!/usr/bin/env python3
"""Summarize content-free phone exports or the paired Mac's diagnostics directory."""
import argparse
import json
import math
from collections import defaultdict
from pathlib import Path


def distribution(values):
    values = sorted(values)
    at = lambda p: values[max(0, math.ceil(len(values) * p) - 1)]
    return dict(samples=len(values), p50Ms=at(.5), p95Ms=at(.95), maxMs=values[-1])


def analyze(paths, session=None):
    groups = defaultdict(list)
    memory = defaultdict(list)
    comparisons = defaultdict(lambda: defaultdict(list))
    scenarios = []
    conditions = {}
    system_reports = []
    seen = set()
    records = 0
    for path in paths:
        try:
            value = json.loads(path.read_text())
        except (ValueError, OSError):
            continue
        for wrapper in value if isinstance(value, list) else [value]:
            if not isinstance(wrapper, dict):
                continue
            batch = wrapper.get('batch', wrapper)
            identity = batch.get('id')
            sid = batch.get('sessionId')
            if not identity or identity in seen or (session and sid != session):
                continue
            seen.add(identity)
            for event in batch.get('events', []):
                records += 1
                operation, phase = event.get('operation'), event.get('phase')
                group = (sid, batch.get('build'), batch.get('deviceModel'), batch.get('osVersion'), operation, phase)
                duration = event.get('durationMs', 0)
                timed = phase in ('duration', 'readiness.proxy', 'failed') or (phase == 'sample' and operation in ('scenario', 'main.probe', 'display.gap'))
                if isinstance(duration, (int, float)) and math.isfinite(duration) and timed:
                    groups[group].append(duration)
                if operation == 'session' and phase == 'sample':
                    elapsed = event.get('elapsedMs', 0)
                    if elapsed >= conditions.get(sid, {}).get('elapsedMs', -1):
                        conditions[sid] = dict(elapsedMs=elapsed, metrics=event.get('metrics', {}))
                if operation and operation.startswith('system.'):
                    system_reports.append(dict(session=sid, build=batch.get('build'), **event))
                if operation == 'memory' and event.get('bytes', 0) > 0:
                    memory[(sid, 'resident')].append((event.get('elapsedMs', 0), event['bytes']))
                    footprint = event.get('metrics', {}).get('physicalFootprintBytes', 0)
                    if footprint > 0:
                        memory[(sid, 'physicalFootprint')].append((event.get('elapsedMs', 0), footprint))
                if operation == 'scenario':
                    mode = event.get('metrics', {}).get('recording')
                    if mode is not None:
                        comparisons[sid][bool(mode)].append(duration)
                    else:
                        scenarios.append(dict(session=sid, **event))
    metrics = [dict(zip(('session', 'build', 'device', 'os', 'operation', 'phase'), key), **distribution(values))
               for key, values in sorted(groups.items())]
    memory_summary = []
    for (sid, measurement), points in memory.items():
        points.sort()
        # Compare the first and final minute of the final five-minute window.
        tail = [p for p in points if p[0] >= points[-1][0] - 300000]
        early = [p[1] for p in tail if p[0] <= tail[0][0] + 60000]
        late = [p[1] for p in tail if p[0] >= tail[-1][0] - 60000]
        memory_summary.append(dict(session=sid, measurement=measurement, samples=len(points), firstBytes=points[0][1], lastBytes=points[-1][1],
                                   minBytes=min(p[1] for p in points), maxBytes=max(p[1] for p in points),
                                   finalWindowSeconds=(tail[-1][0]-tail[0][0])/1000,
                                   finalWindowMeanChangeBytes=sum(late)/len(late)-sum(early)/len(early)))
    overhead = []
    for sid, modes in comparisons.items():
        if not modes[True] or not modes[False]:
            continue
        enabled, disabled = distribution(modes[True]), distribution(modes[False])
        percent = (enabled['p95Ms']/disabled['p95Ms']-1)*100 if disabled['p95Ms'] else None
        overhead.append(dict(session=sid, enabled=enabled, disabled=disabled, addedP95Percent=percent,
                             targetMet=percent is not None and percent < 5))
    return dict(batches=len(seen), events=records, metrics=metrics, memory=memory_summary, recordingComparison=overhead,
                conditions=conditions, systemReports=system_reports,
                scenarios=sorted(scenarios, key=lambda e: (e['session'], e.get('elapsedMs', 0))),
                note='Readiness, comparisons, and display gaps are proxies; touch and actual hitch acceptance are separate.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source', type=Path)
    parser.add_argument('--json', type=Path)
    parser.add_argument('--session', help='Only analyze this capture session UUID')
    args = parser.parse_args()
    result = analyze(args.source.rglob('*.json') if args.source.is_dir() else [args.source], args.session)
    if args.json:
        args.json.write_text(json.dumps(result, indent=2)+'\n')
    print(f"{result['batches']} batches; {result['events']} events")
    print('Session | Build | Device | Operation | Measurement | n | p50 ms | p95 ms | max ms')
    for m in result['metrics']:
        print(f"{m['session']} | {m['build']} | {m['device']} | {m['operation']} | {m['phase']} | {m['samples']} | {m['p50Ms']:.2f} | {m['p95Ms']:.2f} | {m['maxMs']:.2f}")
    for m in result['memory']:
        print(f"Memory {m['session']} ({m['measurement']}): {m['samples']} samples, {m['minBytes']/1048576:.1f}–{m['maxBytes']/1048576:.1f} MiB; final {m['finalWindowSeconds']:.0f}s mean change {m['finalWindowMeanChangeBytes']/1048576:+.1f} MiB")
    for comparison in result['recordingComparison']:
        print(f"Recording comparison {comparison['session']}: added p95 {comparison['addedP95Percent']:+.2f}%; <5% target met: {comparison['targetMet']}")
    for scenario in result['scenarios']:
        print(f"Scenario {scenario['session']}: {scenario['phase']}, {scenario.get('count', 0)} cycles/samples, {scenario.get('durationMs', 0)/1000:.1f}s")
    for session, conditions in result['conditions'].items():
        print(f"Conditions {session}: {json.dumps(conditions, sort_keys=True)}")
    for report in result['systemReports']:
        print(f"System report {report['session']} ({report['operation']}): {json.dumps(report.get('metrics', {}), sort_keys=True)}; {len(report.get('frames', []))} symbol frames")
    print(result['note'])


if __name__ == '__main__':
    main()
