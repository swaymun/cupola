#!/usr/bin/env python3
"""Bounded resource soak for an isolated wonderd: fails when it grows per cycle.

Each cycle reopens both synthetic Project threads (history pages, detail, child
transcript), streams one recorded fixture turn per provider with its approvals,
reconnects the event socket, pairs/revokes/forgets a second device and starts
and ends a computer session against a fake capture helper. After a warm-up the
daemon's RSS, open files, threads, child processes, database and log sizes are
sampled per cycle and compared with budgets. No model, provider home or owner
database is touched. Requires Python cryptography, jsonschema and
websocket-client (scripts/live-fixtures-requirements.txt) and a built wonderd.

  python3 scripts/soak-daemon.py --daemon target/release/wonderd \\
      --bundles .local/soak-bundles --output .local/soak/$(date +%s)

Generate bundles once with `scripts/generate-session-fixtures.py --turns 120
--output .local/soak-bundles` (about a minute); each cycle consumes one turn.
Exit status 1 means a budget failed, a step did not run once per cycle, the
computer helper did not prepare a session, or a process outlived the daemon's
shutdown. The daemon must exit successfully, without forced termination.
SIGTERM and Ctrl-C also clean up the run before exiting unsuccessfully.
report.json names each measurement; log sizes include the daemon's
captured stdout/stderr (daemon.log). On Linux the soak adopts orphans as a child
subreaper, so a process that outlives its parent still counts. On exit it stops
the daemon by itself, kills whatever outlived it and checks nothing survived.
On macOS an inherited private TMPDIR identifies escaped processes even if their
parent exited before sampling. Process environments are never logged.
`--self-test` checks the descendant counter and that cleanup against stand-in
process trees, and that captured output counts toward the log budget.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import signal
import socket
import subprocess
import sys
import time
from types import SimpleNamespace
import urllib.error
import urllib.parse
import urllib.request
import uuid

HERE = Path(__file__).resolve().parent


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, HERE / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


live = load('soak_live', 'run-live-fixtures.py')
http = live.http
replay = http.replay

WORKLOADS = ('reopen', 'stream', 'reconnect', 'pair', 'computer')
# Growth allowed between the first post-warm-up sample and the last one. The
# slope budgets catch steady leaks the totals could hide behind allocator reuse.
BUDGETS = {
    'rssGrowthMiB': 16,
    'rssSlopeKiBPerCycle': 384,
    'openFilesGrowth': 4,
    'threadsGrowth': 4,
    'childProcessesGrowth': 0,
    'databaseGrowthKiBPerTurn': 600,
    'logGrowthKiBPerCycle': 64,
    'activePairedDevices': 1,
    'liveComputerSessions': 0,
}

# The fake helper appends "launched" on start and "prepared" as it answers
# capture.prepare, so the soak can require that every session really ran it.
FAKE_HELPER = r'''import json, sys
if sys.argv[1:] == ["--watch-dialogs"]:
    print(json.dumps({"dialogs": []}), flush=True)
    sys.stdin.read()
    sys.exit(0)
if sys.argv[1:]:
    sys.exit("Unsupported fixture helper mode")
record = open(RECORD, "a", buffering=1)
record.write("launched\n")
status = {"state": "awaitingSource", "sessionID": None, "generation": 1, "sourceID": None, "geometryRevision": 0}
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method", "")
    if "sessionID" in request.get("params", {}):
        status["sessionID"] = request["params"]["sessionID"]
    if method == "capabilities":
        result = {"protocolVersion": 1, "control": {"available": False}}
    elif "stop" in method:
        print(json.dumps({"id": request.get("id"), "result": {"status": dict(status, state="ended")}}), flush=True)
        break
    else:
        result = {"status": status}
    if method == "capture.prepare":
        record.write("prepared\n")
    if "id" in request:
        print(json.dumps({"id": request["id"], "result": result}), flush=True)
'''


def helper_records(path):
    lines = path.read_text().split() if path.exists() else []
    return lines.count('launched'), lines.count('prepared')


def directory_bytes(path):
    total = 0
    for root, _, files in os.walk(path):
        for name in files:
            try:
                total += os.lstat(os.path.join(root, name)).st_size
            except FileNotFoundError:
                pass
    return total


def become_subreaper():
    # Linux: a process orphaned inside the daemon's tree reparents to the soak
    # instead of init, so it stays counted while the daemon runs and is found
    # after the daemon exits. Elsewhere the soak sees only the live tree.
    if sys.platform != 'linux':
        return False
    import ctypes
    return ctypes.CDLL(None, use_errno=True).prctl(36, 1, 0, 0, 0) == 0  # PR_SET_CHILD_SUBREAPER


def process_table():
    # Rows of (pid, ppid, pgid, stat, start time); the start time tells a
    # process from a later one that reused its pid.
    lister = subprocess.Popen(['ps', '-A', '-o', 'pid=,ppid=,pgid=,stat=,lstart='], stdout=subprocess.PIPE, text=True,
                              env=dict(os.environ, LC_ALL='C'))
    output, _ = lister.communicate()
    if lister.returncode:
        raise RuntimeError('ps failed')
    rows = [line.split(None, 4) for line in output.splitlines() if line.strip()]
    return [(int(pid), int(ppid), int(pgid), stat, started) for pid, ppid, pgid, stat, started in rows if int(pid) != lister.pid]


def marked_processes(marker):
    # Darwin has no child subreaper. The run's private TMPDIR survives the
    # bridge environment allowlist and identifies reparented descendants,
    # including one that escaped between samples or before startup completed.
    # Only return PIDs; never retain or log another process's environment.
    output = subprocess.run(['ps', 'eww', '-A', '-o', 'pid=,command='],
                            capture_output=True, text=True, check=True).stdout
    return {int(fields[0]) for line in output.splitlines()
            if (fields := line.split(None, 1)) and fields[0].isdigit()
            and ' ' + marker + ' ' in ' ' + line + ' '}


def descendants(pid, reaper=None, table=None, owner_marker=None):
    # Children the daemon spawns may start their own process group or session
    # (provider runtimes, helpers), so count the daemon's process group and
    # everything descended from it or from a group member, plus what an
    # orphan-adopting `reaper` (the subreaper soak) holds. Zombies count too:
    # an unreaped child is a leak.
    rows = process_table() if table is None else table
    children = {}
    for child, parent, *_ in rows:
        children.setdefault(parent, []).append(child)
    found = set()
    frontier = [pid] + [p for p, _, group, *_ in rows if group == pid and p != pid] + ([reaper] if reaper else [])
    if owner_marker and not reaper:
        frontier.extend(marked_processes(owner_marker))
    while frontier:
        current = frontier.pop()
        if current in found:
            continue
        found.add(current)
        frontier.extend(children.get(current, ()))
    found.difference_update({pid, reaper})
    return sorted(found)


def reap_adopted(daemon_pid):
    # Collect exited orphans the subreaper adopted; the daemon is Popen's.
    reaped = 0
    for pid, ppid, _, stat, _ in process_table():
        if ppid == os.getpid() and pid != daemon_pid and stat.startswith('Z'):
            try:
                reaped += os.waitpid(pid, os.WNOHANG)[0] == pid
            except ChildProcessError:
                pass
    return reaped


def live_processes(pids):
    return sorted(pid for pid, _, _, stat, _ in process_table() if pid in pids and not stat.startswith('Z'))


def wait_until(check, seconds):
    deadline = time.monotonic() + seconds
    while (value := check()) and time.monotonic() < deadline:
        time.sleep(0.1)
    return value


def stop_daemon(process, reaper, grace=15, owner_marker=None):
    """Stop the daemon alone, then kill and verify everything that outlived it.

    Returns (orphans, remaining): processes from the daemon's tree still running
    after it exited, and any that survived SIGKILL. The daemon must stop its own
    children, so it gets SIGTERM by itself, not its process group.
    """
    table = process_table()
    owned = set(descendants(process.pid, reaper, table, owner_marker))
    tree = {(pid, started) for pid, *_, started in table
            if pid in owned}
    if process.poll() is None:
        process.send_signal(signal.SIGTERM)
        try:
            process.wait(timeout=grace)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)

    def outliving():
        reap_adopted(process.pid)
        table = process_table()
        found = set(descendants(process.pid, reaper, table, owner_marker))
        if not reaper:
            # Without a subreaper, orphans reparent out of reach: keep the
            # snapshot's processes, matched by start time, not a reused pid.
            found |= {pid for pid, *_, started in table if (pid, started) in tree}
        return live_processes(found)
    orphans = remaining = wait_until(outliving, 2)
    deadline = time.monotonic() + 5
    while remaining and time.monotonic() < deadline:
        for pid in remaining:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        time.sleep(0.1)
        remaining = outliving()
    return orphans, remaining


# A stand-in daemon. Leaky: a child in its group whose own child starts a new
# session, a child in its own session whose child stays in that session, an
# unreaped (zombie) child and, with ORPHAN, a child that exits after starting a
# new-session child (only a subreaper still finds that one). On SIGTERM it exits
# and leaves them running. Tidy: two children it stops and reaps on SIGTERM, as
# the daemon must.
SELF_TEST_TREE = r'''import signal, subprocess, sys, time
sleep = [sys.executable, '-c', 'import time; time.sleep(60)']
spawn = 'import subprocess, sys, time; subprocess.Popen(%r, start_new_session=%s); time.sleep(60)'
if sys.argv[1] == 'tidy':
    children = [subprocess.Popen(sleep), subprocess.Popen(sleep, start_new_session=True)]
    def stop(*_):
        for child in children:
            child.kill()
            child.wait()
        sys.exit(0)
    signal.signal(signal.SIGTERM, stop)
else:
    subprocess.Popen([sys.executable, '-c', spawn % (sleep, True)])
    subprocess.Popen([sys.executable, '-c', spawn % (sleep, False)], start_new_session=True)
    subprocess.Popen(['true'])
    if sys.argv[1] == 'orphan':
        subprocess.Popen([sys.executable, '-c', 'import subprocess; subprocess.Popen(%r, start_new_session=True)' % (sleep,)])
time.sleep(60)
'''


def self_test_tree(mode, reaper, expected, live):
    # Count the stand-in's tree, stop it like the soak does, and check what
    # outlived it was found, killed and is gone.
    daemon = subprocess.Popen([sys.executable, '-c', SELF_TEST_TREE, mode], start_new_session=True)
    try:
        counted = []
        deadline = time.monotonic() + 10
        while len(counted) != expected and time.monotonic() < deadline:
            time.sleep(0.1)
            counted = descendants(daemon.pid, reaper)
        assert len(counted) == expected, (mode, 'expected %d daemon descendants' % expected, counted)
    finally:
        orphans, remaining = stop_daemon(daemon, reaper, grace=5)
    assert len(orphans) == live, (mode, 'expected %d orphans after shutdown' % live, orphans)
    assert not remaining and not live_processes(set(counted)), (mode, 'descendants survived cleanup', remaining)
    print(f'self-test passed: {mode} stand-in, {expected} descendants counted, {live} outlived it and were killed')


def self_test_early_exit(scratch):
    # The child detaches and its parent exits before the first sample. The old
    # shutdown-only tree snapshot lost it on macOS and reported clean shutdown.
    marker = 'TMPDIR=' + str(scratch)
    code = ("import subprocess,sys; p=subprocess.Popen([sys.executable,'-c',"
            "'import time;time.sleep(60)'],start_new_session=True);print(p.pid,flush=True)")
    daemon = subprocess.Popen([sys.executable, '-c', code], env=dict(os.environ, TMPDIR=str(scratch)),
                              start_new_session=True, stdout=subprocess.PIPE, text=True)
    unrelated = subprocess.Popen([sys.executable, '-c', 'import time;time.sleep(60)'], start_new_session=True)
    child = int(daemon.stdout.readline())
    try:
        daemon.wait(timeout=5)
        counted = descendants(daemon.pid, owner_marker=marker)
        assert child in counted, ('early-exit orphan not counted', child, counted)
        assert unrelated.pid not in counted, 'unrelated process counted as owned'
        orphans, remaining = stop_daemon(daemon, None, grace=1, owner_marker=marker)
        assert orphans == [child] and not remaining, (orphans, remaining)
        assert not live_processes({child}), 'early-exit orphan survived cleanup'
        assert unrelated.poll() is None, 'cleanup signalled an unrelated process'
    finally:
        daemon.stdout.close()
        stop_daemon(daemon, None, grace=1, owner_marker=marker)
        if live_processes({child}):
            os.kill(child, signal.SIGKILL)
        unrelated.terminate()
        unrelated.wait(timeout=5)
    print('self-test passed: detached child counted and cleaned after daemon already exited')


def log_bytes(log_dir, captured):
    # The daemon's own log files plus its captured stdout/stderr: output that
    # bypasses the log directory still lands on the host's disk.
    return directory_bytes(log_dir) + (captured.stat().st_size if captured.exists() else 0)


def shutdown_checks(exit_code, orphans):
    return [{'name': 'daemonExitCode', 'measured': exit_code, 'budget': 0, 'passed': exit_code == 0},
            {'name': 'orphansAfterShutdown', 'measured': len(orphans), 'budget': 0, 'passed': not orphans}]


def interrupted(signum, frame):
    # Raise through the run's finally block instead of abandoning a daemon
    # that intentionally has its own session/process group.
    raise SystemExit(128 + signum)


def wait_for_idle_sample(data_dir, helper, seconds=10, quiet_seconds=2):
    # macOS observes dialogs while a turn runs and drops its watcher on the
    # next poll. Compare idle endpoints, so that legitimate transient child
    # neither fails a clean run nor conceals one leaked child. Still count
    # every descendant in the samples and keep the zero-growth budget.
    import sqlite3
    quiet_since = None
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        with sqlite3.connect(f'file:{data_dir / "wonder.sqlite3"}?mode=ro', uri=True) as db:
            active = db.execute("SELECT COUNT(*) FROM messages WHERE state IN "
                                "('accepted_by_wonder','dispatching_to_codex','accepted_by_codex','streaming')").fetchone()[0]
        commands = subprocess.run(['ps', '-A', '-o', 'command='], capture_output=True, text=True, check=True).stdout
        watcher = str(helper) + ' --watch-dialogs' in commands
        if active or watcher:
            quiet_since = None
        else:
            quiet_since = quiet_since or time.monotonic()
            if time.monotonic() - quiet_since >= quiet_seconds:
                return
        time.sleep(0.1)
    raise AssertionError('Work or dialog watcher did not settle before resource sampling')


def self_test():
    reaper = os.getpid() if become_subreaper() else None
    self_test_tree('leaky', reaper, 5, 4)
    if reaper:
        # The orphan and the exited orphaner's zombie, adopted by the soak.
        self_test_tree('orphan', reaper, 7, 5)
    self_test_tree('tidy', reaper, 2, 0)
    for code in (0, 1, 42, -signal.SIGTERM, -signal.SIGKILL):
        checks = shutdown_checks(code, [])
        assert all(check['passed'] for check in checks) == (code == 0), checks
    assert not all(check['passed'] for check in shutdown_checks(0, [123])), 'orphan accepted at shutdown'
    print('self-test passed: nonzero or forced daemon exit cannot pass shutdown checks')
    # Growth written only to captured stdout/stderr must fail the log budget.
    import tempfile
    with tempfile.TemporaryDirectory() as scratch:
        self_test_early_exit(Path(scratch))
        self_test_work(Path(scratch))
        log_dir, captured = Path(scratch) / 'logs', Path(scratch) / 'daemon.log'
        log_dir.mkdir()
        (log_dir / 'wonderd.log').write_bytes(b'x' * 1024)
        captured.write_bytes(b'')
        samples = []
        for cycle in range(5):
            samples.append({'cycle': cycle, 'logBytes': log_bytes(log_dir, captured), 'counters': {},
                            'process': {'rssKiB': 0, 'openFiles': 0, 'threads': 0, 'childProcesses': 0},
                            'database': {'logicalBytes': 0, 'activeDevices': 0}, 'liveComputerSessions': 0})
            with captured.open('ab') as out:
                out.write(b'y' * 128 * 1024)
        assert samples[-1]['logBytes'] == 1024 + 4 * 128 * 1024, samples[-1]['logBytes']
        log_budget = next(b for b in evaluate(samples, 1, BUDGETS, 0, {}) if b['name'] == 'logGrowthKiBPerCycle')
        assert not log_budget['passed'], log_budget
        print(f"self-test passed: captured output counted ({log_budget['measured']} KiB/cycle fails the log budget)")


def process_sample(pid, reaper, owner_marker=None):
    if sys.platform == 'linux':
        fields = dict(line.split(':', 1) for line in Path(f'/proc/{pid}/status').read_text().splitlines() if ':' in line)
        rss_kib = int(fields['VmRSS'].split()[0])
        threads = int(fields['Threads'])
        files = len(os.listdir(f'/proc/{pid}/fd'))
    else:
        rss_kib = int(subprocess.run(['ps', '-o', 'rss=', '-p', str(pid)], capture_output=True, text=True, check=True).stdout)
        threads = len(subprocess.run(['ps', '-M', '-p', str(pid)], capture_output=True, text=True, check=True).stdout.splitlines()) - 1
        files = len(subprocess.run(['lsof', '-n', '-P', '-p', str(pid)], capture_output=True, text=True).stdout.splitlines()) - 1
    return {'rssKiB': rss_kib, 'threads': threads, 'openFiles': files,
            'childProcesses': len(descendants(pid, reaper, owner_marker=owner_marker))}


def database_sample(data_dir):
    import sqlite3
    database = data_dir / 'wonder.sqlite3'
    files = {name: (data_dir / name).stat().st_size for name in ('wonder.sqlite3', 'wonder.sqlite3-wal', 'wonder.sqlite3-shm')
             if (data_dir / name).exists()}
    # Read-only and outside the daemon's pool; WAL readers never block writers.
    with sqlite3.connect(f'file:{database}?mode=ro', uri=True, timeout=5) as db:
        tables = [row[0] for row in db.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")]
        counts = {table: db.execute(f'SELECT COUNT(*) FROM "{table}"').fetchone()[0] for table in tables}
        try:
            sizes = dict(db.execute('SELECT name, SUM(pgsize) FROM dbstat GROUP BY name').fetchall())
        except sqlite3.OperationalError:
            sizes = {}
        page_size = db.execute('PRAGMA page_size').fetchone()[0]
        pages = db.execute('PRAGMA page_count').fetchone()[0]
        free = db.execute('PRAGMA freelist_count').fetchone()[0]
        live_sessions = db.execute("SELECT COUNT(*) FROM computer_sessions WHERE ended_at IS NULL").fetchone()[0]
        # Forgotten devices stay as scrubbed tombstones that history refers to.
        active = db.execute("SELECT COUNT(*) FROM devices WHERE revoked_at IS NULL AND is_local = 0").fetchone()[0]
    return {'liveComputerSessions': live_sessions, 'activeDevices': active, 'files': files, 'bytes': sum(files.values()), 'logicalBytes': page_size * (pages - free),
            'rows': {k: v for k, v in counts.items() if v}, 'tableBytes': {k: v for k, v in sizes.items() if v}}


def pair_by_link(probe):
    # The scanned-link claim; the typed human code is rate limited per window.
    public = probe.private.public_key().public_numbers()
    jwk = {'kty': 'EC', 'crv': 'P-256', 'x': http.b64(public.x.to_bytes(32, 'big')), 'y': http.b64(public.y.to_bytes(32, 'big'))}
    offer = probe.call('/api/v1/pairing/offers', {}, local=True)
    fragment = dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(offer['url']).fragment))
    claimed = probe.call('/api/v1/pairing/claim', {'offerId': fragment['offerId'], 'secret': fragment['secret'], 'publicKey': jwk,
                                                   'label': 'Soak churn device', 'sessionExpiration': '1h'}, public=True)
    challenge = claimed['challenge']
    fields = ['wonder-session-v1'] + [challenge[k] for k in ('deviceId', 'challengeId', 'nonce', 'origin',
                                                              'hostInstallationId', 'issuedAtMs', 'expiresAtMs')]
    # The owner confirms with the verification text the phone derives from its claim.
    code = hashlib.sha256('\n'.join(map(str, fields)).encode()).hexdigest()[:12].upper()
    probe.call('/api/v1/pairing/pending/' + claimed['deviceId'] + '/confirm', {'code': code}, local=True)
    probe.session = probe.call('/api/v1/pairing/session', {'challengeId': challenge['challengeId'],
                                                           'signature': probe.sign(fields)}, public=True)


class Soak:
    def __init__(self, probe, prepared, data_dir, log_dir, helper_record):
        self.probe = probe
        self.prepared = prepared
        self.data_dir = data_dir
        self.log_dir = log_dir
        # None churns sessions without a capture helper (--no-computer-helper).
        self.helper_record = helper_record
        self.threads = {}
        self.next_turn = {}
        self.counters = {'streamedTurns': 0, 'historyReads': 0, 'eventReconnects': 0, 'pairings': 0, 'computerSessions': 0,
                         'helperPrepared': 0, 'approvals': 0}

    def attach(self):
        for provider in ('codex', 'claude'):
            config, bundle, project, ident = http.attach_provider(self.probe, self.prepared, provider)
            self.threads[provider] = SimpleNamespace(config=config, bundle=bundle, project=project, ident=ident)
            self.next_turn[provider] = config['historyTurns']

    def capacity(self):
        return min(len(t.bundle['threadRead']['thread']['turns']) - t.config['historyTurns'] for t in self.threads.values())

    def reopen(self, provider):
        thread = self.threads[provider]
        base = '/api/v1/conversations/' + thread.ident
        self.probe.call('/api/v1/projects')
        self.probe.call(f'/api/v1/projects/{thread.project}/threads?limit=10')
        self.probe.call('/api/v1/project-conversations/' + thread.ident)
        first = self.probe.call(base + '/history?limit=100')
        if cursor := first['thread'].get('nextCursor'):
            self.probe.call(base + '/history?' + urllib.parse.urlencode({'limit': 100, 'before': cursor}))
        children = self.probe.call(f'/api/v1/project-conversations/{thread.ident}/subagents')
        for child in children.get('subagents', [])[:1]:
            self.probe.call(f'/api/v1/project-conversations/{thread.ident}/subagents/{child["threadId"]}/transcript')
        self.counters['historyReads'] += 1

    def stream(self, provider):
        thread = self.threads[provider]
        base = '/api/v1/conversations/' + thread.ident
        index = self.next_turn[provider]
        self.next_turn[provider] += 1
        expected = thread.bundle['threadRead']['thread']['turns'][index]
        message = {'deviceId': self.probe.session['deviceId'], 'clientMessageId': str(uuid.uuid4()), 'body': replay.TRIGGER}
        def send():
            # A slow host asks clients to wait while it catches up (503).
            try:
                return self.probe.call(base + '/messages', message)
            except AssertionError as error:
                if not (isinstance(error.args[0], tuple) and error.args[0][1] == 503):
                    raise
        http.eventually(send, provider + ' send admitted', 60)
        answered = set()

        def terminal():
            for approval in self.probe.call('/api/v1/approvals'):
                if approval['conversationId'] == thread.ident and approval['approvalId'] not in answered:
                    self.probe.resolve(approval)
                    answered.add(approval['approvalId'])
            snapshot = self.probe.call(base + '/history?limit=3')
            for saved in snapshot.get('messages', []):
                if saved.get('clientMessageId') == message['clientMessageId'] and saved.get('state') == 'failed':
                    raise AssertionError(('dispatch failed', provider, saved))
            return next((t for t in snapshot['thread']['turns'] if t['id'] == expected['id']
                         and t['status'] in ('completed', 'interrupted', 'failed')), None)
        turn = http.eventually(terminal, provider + ' soak turn', 90)
        assert turn['status'] == expected['status'], (provider, turn['status'], expected['status'])
        self.counters['streamedTurns'] += 1
        self.counters['approvals'] += len(answered)

    def reconnect(self, count):
        # A phone reconnects from just before the newest committed event. Each
        # connection must authenticate and replay that event, or it did not
        # happen: a refused or silent socket fails the soak instead of counting.
        checkpoint = self.probe.call('/api/v1/sync/checkpoint')
        newest = checkpoint['lastSequence']
        if newest < 1:
            raise AssertionError('No committed event to replay after reconnecting')
        resume = dict(checkpoint, lastSequence=newest - 1)
        slow_handshake = live.check_dependencies().WebSocketTimeoutException
        for _ in range(count):
            deadline = time.monotonic() + 10

            def replayed():
                # The client allows 0.2 s for the upgrade; a loaded host may
                # take longer, so retry it within the same 10 s deadline.
                try:
                    events = live.Events(self.probe, resume)
                except slow_handshake:
                    return None
                try:
                    return http.eventually(events.read, 'event replay after reconnect', max(0.1, deadline - time.monotonic()))
                finally:
                    events.close()
            _, frames = http.eventually(replayed, 'event reconnect', 10)
            if newest not in [frame.get('sequence') for frame in frames]:
                raise AssertionError(('reconnect replayed the wrong events', newest, frames))
            self.counters['eventReconnects'] += 1

    def pairing_churn(self):
        guest = http.Probe(int(self.probe.address.rsplit(':', 1)[1]), self.probe.origin)
        guest.capability = self.probe.capability
        pair_by_link(guest)
        events = live.Events(guest)
        events.close()
        # The owner's local revoke carries a JSON null body (no device signature).
        revoke = urllib.request.Request(f'{self.probe.address}/api/v1/devices/{guest.session["deviceId"]}/revoke', data=b'null',
                                        method='POST', headers={'Origin': self.probe.origin, 'Content-Type': 'application/json',
                                                                'Host': urllib.parse.urlsplit(self.probe.origin).netloc,
                                                                'x-wonder-loopback-capability': self.probe.capability})
        assert urllib.request.urlopen(revoke, timeout=30).status == 204, 'revoke failed'
        self.probe.call(f'/api/v1/devices/{guest.session["deviceId"]}/forget', {}, local=True, expected=204)
        guest.call('/api/v1/projects', expected=401)
        self.counters['pairings'] += 1

    def computer_session(self):
        thread = self.threads['codex']
        created = self.probe.call('/api/v1/computer/sessions', {
            'clientRequestId': str(uuid.uuid4()), 'conversationId': thread.ident,
            'hostInstallationId': self.probe.session['hostInstallationId']})
        path = '/api/v1/computer/sessions/' + created['id']
        if self.helper_record:
            # The helper must launch and answer capture.prepare: the session
            # reaches awaitingSource. Failed or unavailable is not a session.
            def prepared():
                session = self.probe.call(path)
                if session['state'] in ('failed', 'unavailable', 'ended'):
                    raise AssertionError(('computer helper did not prepare', session))
                return session if session['state'] == 'awaitingSource' else None
            http.eventually(prepared, 'computer session prepared', 15)
            launched, ready = helper_records(self.helper_record)
            if launched != ready or ready <= self.counters['helperPrepared']:
                raise AssertionError(('computer helper did not record its preparation', launched, ready))
            self.counters['helperPrepared'] = ready
        self.probe.call(path + '?' + urllib.parse.urlencode({'generation': created['generation']}), method='DELETE')
        assert self.probe.call(path)['state'] == 'ended', 'computer session did not end'
        self.counters['computerSessions'] += 1

    def cycle(self, reconnects, workloads):
        for provider in ('codex', 'claude'):
            if 'reopen' in workloads:
                self.reopen(provider)
            if 'stream' in workloads:
                self.stream(provider)
        if 'reconnect' in workloads:
            self.reconnect(reconnects)
        if 'pair' in workloads:
            self.pairing_churn()
        if 'computer' in workloads:
            self.computer_session()


def slope(values):
    n = len(values)
    if n < 2:
        return 0.0
    mean_x, mean_y = (n - 1) / 2, sum(values) / n
    return sum((x - mean_x) * (y - mean_y) for x, y in enumerate(values)) / sum((x - mean_x) ** 2 for x in range(n))


def evaluate(samples, warmup, budgets, turns_per_cycle, expected_per_cycle):
    first, last = samples[warmup], samples[-1]
    cycles = max(1, last['cycle'] - first['cycle'])
    # A budget only means something if the work ran: each step must have
    # completed once per measured cycle (reconnects as often as requested).
    work = [{'name': 'completed:' + name, 'measured': last['counters'][name] - first['counters'][name],
             'budget': per_cycle * cycles, 'passed': last['counters'][name] - first['counters'][name] == per_cycle * cycles}
            for name, per_cycle in expected_per_cycle.items()]
    # Judge the steady state: the slope over the second half of the run.
    tail = samples[warmup + (len(samples) - warmup) // 2:]
    measured = {
        'rssGrowthMiB': (last['process']['rssKiB'] - first['process']['rssKiB']) / 1024,
        'rssSlopeKiBPerCycle': slope([s['process']['rssKiB'] for s in tail]),
        'openFilesGrowth': last['process']['openFiles'] - first['process']['openFiles'],
        'threadsGrowth': last['process']['threads'] - first['process']['threads'],
        'childProcessesGrowth': last['process']['childProcesses'] - first['process']['childProcesses'],
        'databaseGrowthKiBPerTurn': (last['database']['logicalBytes'] - first['database']['logicalBytes']) / 1024 / (cycles * max(1, turns_per_cycle)),
        'logGrowthKiBPerCycle': (last['logBytes'] - first['logBytes']) / 1024 / cycles,
        'activePairedDevices': last['database']['activeDevices'],
        'liveComputerSessions': last['liveComputerSessions'],
    }
    if not turns_per_cycle:
        del measured['databaseGrowthKiBPerTurn']
    results = [{'name': k, 'measured': round(v, 2), 'budget': budgets[k], 'passed': v <= budgets[k]} for k, v in measured.items()]
    return work + results


def self_test_work(scratch):
    # The observable contract is completed fixture work, not merely a created
    # row or socket. These probes replay the review's false-success cases at
    # the existing Soak boundary; no real model or owner data is involved.
    from unittest.mock import patch

    record = scratch / 'helper.record'
    helper = 'RECORD = ' + repr(str(record)) + '\n' + FAKE_HELPER
    watcher = subprocess.Popen([sys.executable, '-c', helper, '--watch-dialogs'],
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    try:
        assert json.loads(watcher.stdout.readline()) == {'dialogs': []}
        assert helper_records(record) == (0, 0), 'dialog watcher counted as a capture session'
    finally:
        watcher.communicate(timeout=5)
    assert watcher.returncode == 0, watcher.returncode
    requests = [{'id': 1, 'method': 'capabilities'},
                {'id': 2, 'method': 'capture.prepare', 'params': {'sessionID': 'fixture'}},
                {'id': 3, 'method': 'capture.stop'}]
    captured = subprocess.run([sys.executable, '-c', helper],
                              input='\n'.join(map(json.dumps, requests)) + '\n',
                              capture_output=True, text=True, check=True, timeout=5)
    replies = [json.loads(line) for line in captured.stdout.splitlines()]
    assert replies[1]['result']['status']['state'] == 'awaitingSource', replies
    assert helper_records(record) == (1, 1), 'capture preparation was not recorded'
    print('self-test passed: dialog watcher stays separate from capture preparation receipts')

    import sqlite3
    with sqlite3.connect(scratch / 'wonder.sqlite3') as db:
        db.execute('CREATE TABLE messages(state TEXT)')
        db.execute("INSERT INTO messages VALUES ('streaming')")
    helper_path = scratch / 'computer-helper'
    helper_path.write_text('#!' + sys.executable + '\n' + helper)
    helper_path.chmod(0o700)
    watcher = subprocess.Popen([str(helper_path), '--watch-dialogs'],
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    try:
        watcher.stdout.readline()
        for active in (True, False):
            if not active:
                with sqlite3.connect(scratch / 'wonder.sqlite3') as db:
                    db.execute("UPDATE messages SET state='completed'")
            try:
                wait_for_idle_sample(scratch, helper_path, seconds=0.15, quiet_seconds=0)
            except AssertionError:
                pass
            else:
                raise AssertionError('active turn/watcher accepted as an idle resource endpoint')
    finally:
        watcher.communicate(timeout=5)
    with sqlite3.connect(scratch / 'wonder.sqlite3') as db:
        db.execute("UPDATE messages SET state='streaming'")
    try:
        wait_for_idle_sample(scratch, helper_path, seconds=0.15, quiet_seconds=0)
    except AssertionError:
        pass
    else:
        raise AssertionError('active work accepted after watcher shutdown')
    with sqlite3.connect(scratch / 'wonder.sqlite3') as db:
        db.execute("UPDATE messages SET state='completed'")
    wait_for_idle_sample(scratch, helper_path, seconds=1, quiet_seconds=0)
    print('self-test passed: resource endpoints require idle work and watcher shutdown')

    class ComputerProbe:
        session = {'hostInstallationId': 'fixture-host'}

        def __init__(self, state):
            self.state, self.ended = state, False

        def call(self, path, body=None, method=None):
            if body is not None:
                return {'id': 'fixture', 'generation': 1}
            if method == 'DELETE':
                self.ended = True
                return None
            return {'state': 'ended' if self.ended else self.state}

    cases = [('failed', ''), ('unavailable', ''), ('ended', ''),
             ('awaitingSource', ''), ('awaitingSource', 'launched\n'),
             ('awaitingSource', 'launched\nprepared\n')]
    for state, receipt in cases:
        record.write_text(receipt)
        soak = Soak(ComputerProbe(state), None, None, None, record)
        soak.threads['codex'] = SimpleNamespace(ident='fixture')
        succeeded = state == 'awaitingSource' and receipt.endswith('prepared\n')
        try:
            soak.computer_session()
        except AssertionError:
            assert not succeeded, (state, receipt)
        else:
            assert succeeded, ('failed helper counted as completed', state, receipt)
        assert soak.counters['computerSessions'] == int(succeeded), soak.counters
        assert soak.counters['helperPrepared'] == int(succeeded), soak.counters
    print('self-test passed: helper failure/missing preparation rejected; prepared helper completes')

    class EventProbe:
        def call(self, path):
            return {'hostEpoch': 'fixture', 'lastSequence': 10}

    real_eventually = http.eventually
    def bounded(action, description, seconds=40):
        return real_eventually(action, description, min(seconds, 0.15))

    for delivered in (None, (0, [{'sequence': 9}]), (0, [{'sequence': 10}])):
        closed = []
        class Events:
            def __init__(self, probe, checkpoint):
                assert checkpoint['lastSequence'] == 9, checkpoint

            def read(self):
                return delivered

            def close(self):
                closed.append(True)

        soak = Soak(EventProbe(), None, None, None, None)
        succeeded = delivered is not None and delivered[1][0]['sequence'] == 10
        with patch.object(live, 'Events', Events), patch.object(http, 'eventually', bounded):
            try:
                soak.reconnect(2)
            except AssertionError:
                assert not succeeded, delivered
            else:
                assert succeeded, ('silent/wrong replay counted as reconnect', delivered)
        assert soak.counters['eventReconnects'] == (2 if succeeded else 0), soak.counters
        assert closed, 'event socket was not closed'
    print('self-test passed: silent/wrong event replay rejected; expected replay completes')

    samples = [{'cycle': cycle, 'logBytes': 0, 'counters': {'computerSessions': 0},
                'process': {'rssKiB': 0, 'openFiles': 0, 'threads': 0, 'childProcesses': 0},
                'database': {'logicalBytes': 0, 'activeDevices': 1}, 'liveComputerSessions': 0}
               for cycle in range(5)]
    work = evaluate(samples, 1, BUDGETS, 0, {'computerSessions': 1})[0]
    assert not work['passed'] and work['measured'] == 0 and work['budget'] == 3, work
    print('self-test passed: skipped workload fails despite flat resource samples')


def main():
    if sys.argv[1:] == ['--self-test']:
        self_test()
        return 0
    os.umask(0o077)
    reaper = os.getpid() if become_subreaper() else None
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--daemon', type=Path, required=True)
    parser.add_argument('--bundles', type=Path, required=True, help='Directory with synthetic codex.json and claude.json')
    parser.add_argument('--output', type=Path, required=True, help='New directory under .local')
    parser.add_argument('--cycles', type=int, default=40, help='Measured cycles after warm-up, 4–500')
    parser.add_argument('--warmup', type=int, default=3)
    parser.add_argument('--reconnects', type=int, default=5, help='Event socket reconnects per cycle')
    parser.add_argument('--history-turns', type=int, default=60)
    parser.add_argument('--workload', action='append', choices=WORKLOADS,
                        help='Repeat only these steps (repeatable) to attribute growth; default all')
    parser.add_argument('--no-computer-helper', action='store_true', help='Churn computer sessions without a capture helper')
    parser.add_argument('--budget', action='append', default=[], metavar='NAME=VALUE', help='Override one budget')
    args = parser.parse_args()
    if not 4 <= args.cycles <= 500 or not 1 <= args.warmup <= 20 or not 0 <= args.reconnects <= 50:
        parser.error('Use 4–500 cycles, 1–20 warm-up cycles and 0–50 reconnects')
    budgets = dict(BUDGETS)
    for override in args.budget:
        name, _, value = override.partition('=')
        if name not in budgets:
            parser.error('Unknown budget ' + name)
        budgets[name] = float(value)
    output = args.output.absolute()
    if '.local' not in output.parts or output.exists() or output.is_symlink():
        parser.error('Use a new directory under .local')
    output.mkdir(parents=True, mode=0o700)
    started = time.monotonic()
    for provider in ('codex', 'claude'):
        replay.prepare(SimpleNamespace(bundle=args.bundles / (provider + '.json'), output=output / provider,
                                       history_turns=args.history_turns, speed=10000, timing=False))
    helper, helper_record = output / 'computer-helper', output / 'computer-helper.record'
    helper.write_text('#!' + sys.executable + '\nRECORD = ' + repr(str(helper_record)) + '\n' + FAKE_HELPER)
    helper.chmod(0o700)
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    probe = http.Probe(port)
    data_dir, log_dir = output / 'daemon', output / 'logs'
    env = {k: v for k, v in os.environ.items() if not k.startswith('WONDER_')}
    env.update(WONDER_DATA_DIR=str(data_dir), WONDER_LOG_DIR=str(log_dir), TMPDIR=str(output / 'tmp'),
               WONDER_CODEX_BIN=str(output / 'codex/replay-runtime'), CODEX_HOME=str(output / 'codex-home'),
               CLAUDE_CONFIG_DIR=str(output / 'claude-home'), WONDER_CLAUDE_BRIDGE=str(output / 'claude/replay-runtime'),
               WONDER_NODE_BIN=sys.executable, WONDER_LOOPBACK_CAPABILITY=probe.capability,
               WONDER_LISTEN_ADDR=f'127.0.0.1:{port}', WONDER_PUBLIC_ORIGIN=probe.origin)
    if not args.no_computer_helper:
        env.update(WONDER_COMPUTER_USE_ENABLED='1', WONDER_COMPUTER_USE_BIN=str(helper))
    for folder in ('codex-home', 'claude-home', 'tmp'):
        (output / folder).mkdir()
    owner_marker = 'TMPDIR=' + env['TMPDIR']
    report = {'daemonSha256': hashlib.sha256(args.daemon.read_bytes()).hexdigest(), 'modelCalls': 0,
              'environment': {'platform': platform.platform(), 'python': platform.python_version(), 'cpus': os.cpu_count()},
              'options': {k: v for k, v in vars(args).items() if k not in ('daemon', 'bundles', 'output')} | {'budgets': budgets},
              'samples': []}
    result = 1
    adopted = {'reaped': 0}
    signal.signal(signal.SIGTERM, interrupted)
    with (output / 'daemon.log').open('w') as log:
        process = subprocess.Popen([str(args.daemon.resolve())], env=env, stdout=log, stderr=log, start_new_session=True)
        try:
            def listening():
                if process.poll() is not None:
                    raise AssertionError('Isolated daemon exited during startup')
                try:
                    return probe.call('/healthz', local=True)
                except urllib.error.URLError:
                    return None
            http.eventually(listening, 'isolated daemon listening')
            probe.pair()

            def ready():
                try:
                    return probe.call('/readyz', local=True)
                except AssertionError as error:
                    if error.args[0][1] != 503:
                        raise
            http.eventually(ready, 'runtime ready', 60)
            soak = Soak(probe, output, data_dir, log_dir, None if args.no_computer_helper else helper_record)
            soak.attach()
            workloads = args.workload or WORKLOADS
            streamed = 2 if 'stream' in workloads else 0
            if streamed and soak.capacity() < args.warmup + args.cycles:
                raise ValueError(f'Bundles hold {soak.capacity()} reserved turns; need {args.warmup + args.cycles}')

            def sample(cycle, seconds):
                if sys.platform == 'darwin' and not args.no_computer_helper and cycle in (args.warmup, args.warmup + args.cycles):
                    wait_for_idle_sample(data_dir, helper)
                database = database_sample(data_dir)
                if reaper:
                    adopted['reaped'] += reap_adopted(process.pid)
                row = {'cycle': cycle, 'cycleSeconds': round(seconds, 3), 'process': process_sample(process.pid, reaper, owner_marker),
                       'database': database, 'dataBytes': directory_bytes(data_dir), 'logBytes': log_bytes(log_dir, output / 'daemon.log'),
                       'liveComputerSessions': database.pop('liveComputerSessions'), 'counters': dict(soak.counters),
                       'exitedOrphansReaped': adopted['reaped']}
                report['samples'].append(row)
                print(json.dumps({'cycle': cycle, 'seconds': row['cycleSeconds'], **row['process'],
                                  'dbKiB': database['logicalBytes'] // 1024, 'logKiB': row['logBytes'] // 1024}), flush=True)
            sample(0, 0)
            for cycle in range(1, args.warmup + args.cycles + 1):
                began = time.monotonic()
                soak.cycle(args.reconnects, workloads)
                sample(cycle, time.monotonic() - began)
                if process.poll() is not None:
                    raise AssertionError('Daemon exited during the soak')
            expected = {'historyReads': 2 * ('reopen' in workloads), 'streamedTurns': streamed,
                        'eventReconnects': args.reconnects * ('reconnect' in workloads), 'pairings': int('pair' in workloads),
                        'computerSessions': int('computer' in workloads),
                        'helperPrepared': int('computer' in workloads and not args.no_computer_helper)}
            report['budgets'] = evaluate(report['samples'], args.warmup, budgets, streamed,
                                         {k: v for k, v in expected.items() if v})
            report['result'] = 'passed' if all(b['passed'] for b in report['budgets']) else 'failed'
            result = 0 if report['result'] == 'passed' else 1
        except BaseException as error:
            report['result'] = 'error'
            report['error'] = repr(error)
            raise
        finally:
            # The daemon must stop its own children: whatever outlives it
            # fails the soak, and the soak kills it and checks it is gone.
            # Repeated interruption must not cut this bounded cleanup short.
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            signal.signal(signal.SIGINT, signal.SIG_IGN)
            orphans, remaining = stop_daemon(process, reaper, owner_marker=owner_marker)
            report['seconds'] = round(time.monotonic() - started, 1)
            report['exitCode'] = process.returncode
            report['shutdown'] = {'orphans': orphans, 'survivedCleanup': remaining, 'subreaper': bool(reaper)}
            if 'budgets' in report:
                checks = shutdown_checks(process.returncode, orphans)
                report['budgets'].extend(checks)
                if not all(check['passed'] for check in checks):
                    report['result'], result = 'failed', 1
            if remaining:
                report['result'], result = 'error', 1
                report['error'] = f'Daemon descendants survived SIGKILL: {remaining}'
            (output / 'report.json').write_text(json.dumps(report, indent=2))
    for budget in report.get('budgets', []):
        print(('PASS ' if budget['passed'] else 'FAIL ') + f"{budget['name']}: {budget['measured']} (budget {budget['budget']})")
    if report['shutdown']['survivedCleanup']:
        print('ERROR ' + report['error'])
    print(f"{report['result']} in {report['seconds']}s; report {output / 'report.json'}")
    return result


if __name__ == '__main__':
    sys.exit(main())
