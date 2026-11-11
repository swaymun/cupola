#!/usr/bin/env python3
"""Run the real pair-device orchestration against fake host/Xcode processes."""
import base64
import datetime
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock
from types import SimpleNamespace

SCRIPT = Path(__file__).with_name("pair-device").resolve()
loader = importlib.machinery.SourceFileLoader("pair_device", str(SCRIPT))
spec = importlib.util.spec_from_loader(loader.name, loader)
pair_device = importlib.util.module_from_spec(spec)
loader.exec_module(pair_device)

FAKE_HOST = '''#!/usr/bin/env python3
import json,os,pathlib,sys,time
root=pathlib.Path(os.environ['PAIR_FAKE_ROOT']); mode=os.environ.get('PAIR_FAKE_MODE','success')
args=sys.argv[1:]
with (root/'host-calls').open('a') as f:f.write(json.dumps(args)+'\\n')
command=args[1]
if command=='status': print(json.dumps({'host':{'hostInstallationId':'host-123','hostName':'Fake computer'}}))
elif command=='devices': print(json.dumps({'devices':[] if mode=='missing-device' else [{'id':'phone-456','label':'Testing phone','revokedAt':'today' if mode=='revoked-device' else None}]}))
elif '--cancel' in args:
 if mode=='cleanup-fails': print(json.dumps({'error':{'message':'cancel unavailable'}}));sys.exit(1)
 print(json.dumps({'event':'cancelled','offerId':args[-1]}))
elif command=='pair':
 assert '--yes' in args and '--watch-stdin' in args
 print(json.dumps({'event':'offer','offerId':'owned-offer','url':'https://fake.invalid/pair#private','origin':'https://fake.invalid','code':'ABCD1234','expiresAtMs':9000000000000}),flush=True)
 if mode=='host-fails':print(json.dumps({'error':{'message':'claim failed'}}),flush=True);sys.exit(1)
 while not (root/'app-started').exists():time.sleep(.01)
 if mode=='no-connected':
  sys.stdin.read();(root/'host-input-closed').write_text('exited');sys.exit(0)
 if mode!='missing-verification':
  print(json.dumps({'event':'verification','hostInstallationId':'wrong-host' if mode=='wrong-verification' else 'host-123','deviceId':'wrong-phone' if mode=='wrong-device' else 'phone-456'}),flush=True)
 print(json.dumps({'event':'connected','deviceId':'phone-456','label':'Testing phone'}),flush=True)
'''
FAKE_XCODE = '''#!/usr/bin/env python3
import json,os,pathlib,plistlib,sys,time
root=pathlib.Path(os.environ['PAIR_FAKE_ROOT']); mode=os.environ.get('PAIR_FAKE_MODE','success');args=sys.argv
p=pathlib.Path(args[args.index('-xctestrun')+1]);d=plistlib.loads(p.read_bytes());t=d['TestConfigurations'][0]['TestTargets'][0]
e=t['EnvironmentVariables'];assert e['WONDER_PAIRING_HOST_ID']=='host-123'
assert t['UITargetAppBundleIdentifier']=='com.swaymun.wonder'
assert len(t['OnlyTestIdentifiers'])==1 and args[args.index('-only-testing:WonderUITests/'+t['OnlyTestIdentifiers'][0])]
if t['OnlyTestIdentifiers'][0].endswith('testPairForLiveRun'): assert e['WONDER_PAIRING_LINK']=='https://fake.invalid/pair#private'
else: assert e['WONDER_PAIRING_ADDRESS']=='https://fake.invalid' and e['WONDER_PAIRING_CODE']=='ABCD1234'
(root/'app-started').write_text('yes')
result=pathlib.Path(args[args.index('-resultBundlePath')+1]);result.mkdir()
summary={'totalTestCount':1,'passedTests':0 if mode=='skip' else 1,'skippedTests':1 if mode=='skip' else 0,'failedTests':0}
(root/'test-summary').write_text(json.dumps(summary))
if mode in ['app-hangs','no-connected']:time.sleep(60)
if mode=='app-fails':sys.exit(1)
'''
FAKE_XCRUN = '''#!/usr/bin/env python3
import json,os,pathlib,sys,time
root=pathlib.Path(os.environ['PAIR_FAKE_ROOT'])
if sys.argv[1]=='simctl':
 if sys.argv[2]=='list':print(json.dumps({'devices':{'iOS':[{'udid':'00000000-0000-4000-8000-000000000001','state':'Booted'}]}}))
 elif 'recordVideo' in sys.argv:time.sleep(60)
 sys.exit(0)
assert sys.argv[1:5]==['xcresulttool','get','test-results','summary']
print((root/'test-summary').read_text())
'''

FAKE_SSH = '''#!/usr/bin/env python3
import subprocess,sys
# The remote command owns a separate session; killing SSH cannot kill it.
child=subprocess.Popen(sys.argv[-1],shell=True,start_new_session=True)
sys.exit(child.wait())
'''


class FlowTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        # Independent runs reuse fake device IDs, but must not share the owner's
        # pairing locks. The competing-invocation test still shares this home.
        fixture_home = self.root / 'home'
        fixture_home.mkdir()
        home = mock.patch.dict(os.environ, HOME=str(fixture_home))
        home.start()
        self.addCleanup(home.stop)
        self.bin = self.root / 'bin'; self.bin.mkdir()
        for name, content in [('fake-cupola', FAKE_HOST), ('xcodebuild', FAKE_XCODE), ('xcrun', FAKE_XCRUN), ('ssh', FAKE_SSH)]:
            p = self.bin / name; p.write_text(content); p.chmod(0o755)
        app = self.root / 'Wonder.app'; app.mkdir()
        (app / 'Info.plist').write_bytes(plistlib.dumps({'CFBundleIdentifier': 'com.swaymun.wonder'}))
        self.source = self.root / 'Diagnostics.xctestrun'
        self.source.write_bytes(plistlib.dumps({'TestConfigurations': [{'TestTargets': [{
            'IsUITestBundle': True, 'UITargetAppBundleIdentifier': 'com.swaymun.wonder',
            'UITargetAppPath': '__TESTROOT__/Wonder.app',
            'EnvironmentVariables': {'EXISTING': 'retained'}, 'SkipTestIdentifiers': ['old'],
        }]}]}))
        self.evidence = self.root / 'evidence'
        self.env = dict(os.environ, PATH=str(self.bin)+os.pathsep+os.environ['PATH'], PAIR_FAKE_ROOT=str(self.root))
        self.command = [sys.executable, str(SCRIPT), '--host', 'local', '--device', 'fake-physical-device',
                        '--host-cli', str(self.bin / 'fake-cupola'), '--xctestrun', str(self.source),
                        '--evidence', str(self.evidence), '--timeout', '2']

    def flow(self, mode='success', extra=()):
        result = subprocess.run([*self.command, *extra], env=dict(self.env, PAIR_FAKE_MODE=mode), text=True, capture_output=True, timeout=15)
        report = json.loads((self.evidence / 'result.json').read_text())
        calls = [json.loads(s) for s in (self.root / 'host-calls').read_text().splitlines()]
        self.assertEqual(list(self.root.glob('auto-pair-*.xctestrun')), [], 'private patched environment must be removed')
        self.assertEqual(calls[-1], ['--json','pair','--cancel','owned-offer'])
        self.assertNotIn('#private', (self.evidence / 'host-events.ndjson').read_text())
        return result, report, calls

    def test_link_flow_requires_host_and_one_executed_app_test(self):
        result, report, calls = self.flow()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report['status'], 'paired')
        self.assertEqual(report['surface'], 'physical')
        self.assertEqual(report['deviceId'], 'phone-456')
        self.assertEqual(report['offerCleanup'], 'cancelled')
        self.assertIn(['--json', 'devices'], calls)
        self.assertNotIn('verificationText', report)
        self.assertNotIn('Verification text:', result.stdout)

    def test_code_uses_existing_address_and_code_test(self):
        result, report, _ = self.flow(extra=['--method', 'code'])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(report['test']['passedTests'], 1)

    def test_app_failure_names_app_and_cancels(self):
        result, report, _ = self.flow('app-fails')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('app:', report['error'])

    def test_host_failure_names_host_and_cancels(self):
        result, report, _ = self.flow('host-fails')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('host: claim failed', report['error'])

    def test_skipped_app_test_is_failure(self):
        result, report, _ = self.flow('skip')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('one executed', report['error'])

    def test_missing_host_device_is_failure(self):
        result, report, _ = self.flow('missing-device')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('absent or revoked', report['error'])

    def test_revoked_host_device_is_failure(self):
        result, report, _ = self.flow('revoked-device')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('absent or revoked', report['error'])

    def test_verification_from_another_host_is_failure(self):
        result, report, _ = self.flow('wrong-verification')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('mismatched', report['error'])

    def test_verification_from_another_device_is_failure(self):
        result, report, _ = self.flow('wrong-device')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('mismatched', report['error'])

    def test_missing_verification_is_failure(self):
        result, report, _ = self.flow('missing-verification')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('missing', report['error'])

    def test_app_timeout_cancels_offer(self):
        result, report, _ = self.flow('app-hangs')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('app: pairing timed out', report['error'])

    def test_remote_timeout_closes_input_and_the_detached_pair_exits(self):
        result, report, _ = self.flow('no-connected', extra=['--host', 'fake-remote', '--host-platform', 'posix'])
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((self.root / 'host-input-closed').read_text(), 'exited')
        self.assertNotIn('hostProcessCleanup', report)

    def test_cleanup_failure_cannot_exit_success(self):
        result, report, _ = self.flow('cleanup-fails')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(report['status'], 'failed')
        self.assertIn('failed:', report['offerCleanup'])

    def test_sigint_stops_app_and_cancels_offer(self):
        process = subprocess.Popen(self.command, env=dict(self.env, PAIR_FAKE_MODE='app-hangs'), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        deadline = time.monotonic() + 5
        while not (self.root / 'app-started').exists() and time.monotonic() < deadline:
            time.sleep(.02)
        process.send_signal(signal.SIGINT)
        stdout, stderr = process.communicate(timeout=15)
        self.assertNotEqual(process.returncode, 0, stdout + stderr)
        report = json.loads((self.evidence / 'result.json').read_text())
        self.assertEqual(report['offerCleanup'], 'cancelled')

    def test_second_invocation_on_same_device_exits_75_before_host_or_app(self):
        process = subprocess.Popen(self.command, env=dict(self.env, PAIR_FAKE_MODE='app-hangs'), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            deadline = time.monotonic() + 5
            while not (self.root / 'app-started').exists() and time.monotonic() < deadline:
                time.sleep(.02)
            before = (self.root / 'host-calls').read_text()
            second = subprocess.run([*self.command, '--evidence', str(self.root / 'second')], env=self.env, capture_output=True, text=True, timeout=5)
            self.assertEqual(second.returncode, 75, second.stderr)
            self.assertIn('already being paired', second.stderr)
            self.assertEqual((self.root / 'host-calls').read_text(), before)
        finally:
            process.terminate()
            process.communicate(timeout=10)

    def test_other_app_xctestrun_is_rejected_before_offer(self):
        text = self.source.read_bytes().replace(b'com.swaymun.wonder', b'com.saimun.wonder.native')
        self.source.write_bytes(text)
        result = subprocess.run(self.command, env=self.env, text=True, capture_output=True, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('must target Cupola (com.swaymun.wonder)', result.stderr)
        self.assertNotIn('"pair"', (self.root / 'host-calls').read_text())

    def test_windows_ssh_quotes_cli_and_data_without_shell_interpolation(self):
        host = pair_device.Host('windows-build-host', 'windows', "C:\\Program Files\\Cupola's app\\cupola.exe", "C:\\Data's folder")
        command = host.command('pair', '--cancel', "offer ' & $x")
        self.assertEqual(command[:3], ['ssh', '-T', '-o'])
        script = base64.b64decode(command[-1].split()[-1]).decode('utf-16le')
        self.assertIn("Cupola''s app", script)
        self.assertIn("'offer '' & $x'", script)
        self.assertIn("$env:WONDER_DATA_DIR='C:\\Data''s folder'", script)
        self.assertNotIn("offer ' & $x", command[-1])

    def test_remote_build_uses_selected_checkout_and_development_binary(self):
        checkout = self.root / "VM's checkout with spaces"
        checkout.mkdir()
        cargo = self.bin / 'cargo'
        cargo.write_text('''#!/usr/bin/env python3
import json,os,pathlib,shutil,sys
root=pathlib.Path(os.environ['PAIR_FAKE_ROOT'])
(root/'cargo-call.json').write_text(json.dumps({'args':sys.argv[1:],'cwd':os.getcwd(),'jobs':os.environ['CARGO_BUILD_JOBS']}))
target=pathlib.Path(os.environ['CARGO_TARGET_DIR'])/'debug'
target.mkdir(parents=True)
shutil.copy2(root/'bin/fake-cupola',target/'cupola')
''')
        cargo.chmod(0o755)
        self.evidence.mkdir()
        args = SimpleNamespace(host='fake-remote', host_platform='posix', host_source=str(checkout),
                               host_cli=None, evidence=self.evidence, build_timeout=15)
        with mock.patch.dict(os.environ, dict(self.env, HOME=str(self.root))):
            cli = pair_device.host_cli(args)
            status = pair_device.Host('fake-remote', 'posix', cli).call('status')
        self.assertEqual(cli, str(checkout / '.local/build/auto-pair-cli/debug/cupola'))
        self.assertEqual(status['host']['hostInstallationId'], 'host-123')
        call = json.loads((self.root / 'cargo-call.json').read_text())
        self.assertEqual(Path(call['cwd']).resolve(), checkout.resolve())
        self.assertEqual(call['jobs'], '4')
        self.assertEqual(call['args'], ['build', '--locked', '-p', 'wonderd', '--features',
                                      'dev-pairing', '--bin', 'cupola', '--bin', 'wonderd'])

    def test_remote_build_failure_never_falls_back_to_installed_cli(self):
        self.evidence.mkdir()
        args = SimpleNamespace(host='fake-remote', host_platform='posix',
                               host_source=str(self.root / 'missing checkout'), host_cli=None,
                               evidence=self.evidence, build_timeout=15)
        with mock.patch.dict(os.environ, self.env), self.assertRaisesRegex(pair_device.Failure, 'VM dev-pairing CLI build failed'):
            pair_device.host_cli(args)
        self.assertFalse((self.root / 'host-calls').exists())

    def test_windows_vm_build_quotes_source_and_uses_development_feature(self):
        host = pair_device.Host('windows-build-host', 'windows', 'unused')
        command = pair_device.remote_host_build(host, "C:\\VM's checkout & data")
        script = base64.b64decode(command[-1].split()[-1]).decode('utf-16le')
        self.assertIn("Set-Location 'C:\\VM''s checkout & data'", script)
        self.assertIn("'--features' 'dev-pairing'", script)
        self.assertIn("'scripts/windows/cargo-windows'", script)
        self.assertNotIn('C:\\VM', command[-1])

    def test_xctestrun_keeps_existing_environment_and_restricts_target(self):
        target = self.root / 'patched.xctestrun'
        pair_device.testing_configuration(self.source, target, {'WONDER_PAIRING_HOST_ID': 'host'}, 'WonderUITests/WonderUITests/testPairForLiveRun')
        data = plistlib.loads(target.read_bytes())['TestConfigurations'][0]['TestTargets'][0]
        self.assertEqual(data['EnvironmentVariables']['EXISTING'], 'retained')
        self.assertEqual(data['OnlyTestIdentifiers'], ['WonderUITests/testPairForLiveRun'])
        self.assertEqual(data['SkipTestIdentifiers'], [])

    def simulator_flow(self, mode='success', refused=False):
        args = SimpleNamespace(evidence=self.evidence, device='00000000-0000-4000-8000-000000000001',
            host='local', host_platform='posix', host_cli=str(self.bin / 'fake-cupola'), host_data_dir=None,
            xctestrun=self.source, method='link', timeout=2, derived_data=self.root / 'build', build_timeout=30)
        original = subprocess.run
        lease_calls = []
        def run(command, **kwargs):
            if str(command[0]).endswith('/scripts/wonderctl'):
                lease_calls.append(command[2])
                return subprocess.CompletedProcess(command, 75 if refused else 0, 'another checkout owns lease' if refused else '{}', '')
            return original(command, **kwargs)
        with mock.patch.dict(os.environ, dict(self.env, PAIR_FAKE_MODE=mode)), mock.patch.object(pair_device.subprocess, 'run', side_effect=run):
            if mode == 'success' and not refused:
                pair_device.pair(args)
            else:
                with self.assertRaises(pair_device.Failure) as failure:
                    pair_device.pair(args)
                if refused:
                    self.assertEqual(failure.exception.exit_code, 75)
        return lease_calls

    def test_simulator_acquires_and_releases_wonderctl_lease(self):
        self.assertEqual(self.simulator_flow(), ['acquire', 'release'])

    def test_simulator_failure_releases_wonderctl_lease(self):
        self.assertEqual(self.simulator_flow('app-fails'), ['acquire', 'release'])

    def test_refused_simulator_lease_never_touches_host_or_app(self):
        self.assertEqual(self.simulator_flow(refused=True), ['acquire'])
        self.assertFalse((self.root / 'host-calls').exists())
        self.assertFalse((self.root / 'app-started').exists())

    def test_profile_selection_requires_testing_development_team_device_and_lifetime(self):
        now = datetime.datetime(2026, 10, 17)
        profiles = []
        for bundle in pair_device.PROFILE_BUNDLES.values():
            valid = dict(Name=bundle, ProvisionedDevices=['owned-phone'], CreationDate=now,
                         ExpirationDate=now + datetime.timedelta(days=30),
                         Entitlements={'application-identifier': '8KKNVD7758.' + bundle, 'get-task-allow': True})
            profiles.append(valid)
            # Newer wrong-device, expired, App Store and production profiles
            # must never displace an eligible Testing development profile.
            newer = dict(valid, CreationDate=now + datetime.timedelta(days=1))
            profiles.extend([dict(newer, ProvisionedDevices=['other-phone']),
                             dict(newer, ExpirationDate=now),
                             dict(newer, Entitlements={'application-identifier': '8KKNVD7758.' + bundle, 'get-task-allow': False}),
                             dict(newer, Entitlements={'application-identifier': '8KKNVD7758.com.swaymun.wonder', 'get-task-allow': True})])
        self.assertEqual(pair_device.select_profiles(profiles, 'owned-phone', now), pair_device.PROFILE_BUNDLES)
        self.assertEqual(pair_device.select_profiles(profiles, 'missing-phone', now), {})


if __name__ == '__main__':
    unittest.main(verbosity=2)
