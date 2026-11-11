#!/usr/bin/env python3
"""Verify matching app identity and exclusion of diagnostic machinery in Release.

Pass exported app bundles; scans every bundled Mach-O so a
helper/framework cannot accidentally retain the driver or recorder.
"""
import argparse
import json
import plistlib
import subprocess
from pathlib import Path

RECORDER_MARKERS = ('DiagnosticJournal', 'DiagnosticScenarioControl', '/api/v1/diagnostics/batches')
DRIVER_MARKERS = ('DiagnosticTestDriver', 'DiagnosticDriverHTTP', 'DiagnosticDriverSurface',
                  'WONDER_TEST_DRIVER_TOKEN', 'wonder-test-driver.json')
MACHO_MAGIC = (b'\xcf\xfa\xed\xfe', b'\xce\xfa\xed\xfe', b'\xfe\xed\xfa\xcf', b'\xfe\xed\xfa\xce',
               b'\xca\xfe\xba\xbe', b'\xbe\xba\xfe\xca', b'\xca\xfe\xba\xbf')


def check_profiles(diagnostics, release):
    result = {}
    for profile, path in [('diagnostics', diagnostics), ('release', release)]:
        info = plistlib.loads((path / 'Info.plist').read_bytes())
        assert info['CFBundleIdentifier'] == 'com.swaymun.wonder'
        assert info['WonderBuildProfile'] == profile
        executable = path / info['CFBundleExecutable']
        main_strings = subprocess.check_output(['strings', str(executable)], text=True)
        for marker in RECORDER_MARKERS + DRIVER_MARKERS:
            assert (marker in main_strings) == (profile == 'diagnostics'), (profile, marker)
        scanned = []
        if profile == 'release':
            for binary in sorted(p for p in path.rglob('*') if p.is_file()):
                with binary.open('rb') as handle:
                    if handle.read(4) not in MACHO_MAGIC:
                        continue
                contents = binary.read_bytes()
                symbols = subprocess.check_output(['nm', '-gj', str(binary)], stderr=subprocess.DEVNULL)
                for marker in RECORDER_MARKERS + DRIVER_MARKERS:
                    assert marker.encode() not in contents, (profile, str(binary), 'string', marker)
                    assert marker.encode() not in symbols, (profile, str(binary), 'symbol', marker)
                scanned.append(str(binary.relative_to(path)))
        result[profile] = {'executableBytes': executable.stat().st_size,
                           'appBytes': sum(f.stat().st_size for f in path.rglob('*') if f.is_file()),
                           'releaseMachOScanned': scanned, 'driverMarkersChecked': list(DRIVER_MARKERS)}
    return result


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('diagnostics', type=Path)
    parser.add_argument('release', type=Path)
    args = parser.parse_args()
    print(json.dumps(check_profiles(args.diagnostics, args.release), indent=2))
