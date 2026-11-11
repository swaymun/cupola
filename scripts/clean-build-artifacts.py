#!/usr/bin/env python3
"""Plan and expire explicitly owned Wonder artifacts and inactive build caches.

Dry run by default. Legacy/unknown data is reported and preserved. Build leases,
run consumers, pins and source guards take precedence over TTL and budgets.
"""
import argparse
import hashlib
import json
import math
import os
import re
from pathlib import Path
import plistlib
import shutil
import stat
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
XCODE_PARTS = (
    'Build/Intermediates.noindex', 'Build/Products', 'ModuleCache.noindex',
    'Index.noindex', 'SDKStatCaches.noindex', 'CompilationCache.noindex',
    'SourcePackages/artifacts', 'SourcePackages/workspace-state.json',
    'SDKExplicitPrecompiledModules',
)
SKIP = {'.git', 'node_modules', 'checkouts', 'repositories', 'RetainedSymbols', 'orchestrate', 'storage', 'backups'}
PACKAGES = ('apps/native', 'native/computer-use')
SWIFTBUILD_PARTS = ('Products', 'Intermediates.noindex', 'ModuleCache.noindex',
                    'CompilationCache.noindex', 'SDKExplicitPrecompiledModules',
                    'SDKStatCaches.noindex', 'PCH')


def swiftbuild_directory(path):
    return (path / 'manifest.pif').is_file() and any(
        marker.is_file() and marker.read_text().strip() == 'swiftbuild'
        for marker in path.glob('.buildSystem_*'))


def contained(path, root):
    """Reject symlink traversal, including links that point back into the root."""
    path = Path(os.path.abspath(path))
    relative = path.relative_to(root)
    cursor = root
    for part in relative.parts:
        cursor /= part
        if cursor.is_symlink():
            raise ValueError(f'Refusing symlink: {cursor}')
    if path == root:
        raise ValueError('Refusing repository root')
    return path


def xcode_directory(path):
    try:
        info = plistlib.loads((path / 'info.plist').read_bytes())
        return str(info.get('WorkspacePath', '')).endswith('/Wonder.xcodeproj')
    except (OSError, ValueError, plistlib.InvalidFileException):
        return False


def discover(root):
    root = Path(root).resolve()
    candidates = []
    registered = subprocess.run(['git', '-C', str(root), 'worktree', 'list', '--porcelain'], text=True, capture_output=True)
    other_worktrees = {Path(line[9:]).resolve() for line in registered.stdout.splitlines() if line.startswith('worktree ')} - {root.resolve()}
    for search in (root / '.local', root / 'target', root / 'apps/ios/DerivedData'):
        if not search.is_dir() or search.is_symlink():
            continue
        for base, dirs, _ in os.walk(search, followlinks=False):
            path = Path(base)
            dirs[:] = [name for name in dirs if name not in SKIP
                       and not name.endswith(('.app', '.xcarchive', '.xcresult', '.dSYM', '.trace', '.framework'))
                       and not (path / name).is_symlink()
                       and path / name not in other_worktrees]
            if xcode_directory(path):
                candidates.extend(path / part for part in XCODE_PARTS)
                cache_roots = {part.split('/')[0] for part in XCODE_PARTS}
                dirs[:] = [name for name in dirs if name not in cache_roots | {'Logs', 'TestResults'}]
            elif (path / '.fingerprint').is_dir() and (path / 'deps').is_dir():
                candidates.append(path)
                dirs[:] = []
            elif (path / 'workspace-state.json').is_file() and ((path / 'build.db').is_file() or swiftbuild_directory(path)):
                for triple in dirs:
                    if '-apple-' in triple:
                        candidates.extend(path / triple / profile for profile in ('debug', 'release'))
                if swiftbuild_directory(path):
                    candidates.extend(path / 'out' / part for part in SWIFTBUILD_PARTS)
                dirs[:] = [name for name in dirs if '-apple-' not in name and name not in {'artifacts', 'out'}]
    for package in PACKAGES:
        build = root / package / '.build'
        if build.is_dir() and not build.is_symlink():
            if swiftbuild_directory(build):
                candidates.extend(build / 'out' / part for part in SWIFTBUILD_PARTS)
            for triple in build.iterdir():
                if triple.is_dir() and not triple.is_symlink() and '-apple-' in triple.name:
                    candidates.extend(triple / profile for profile in ('debug', 'release'))
    return sorted({contained(p, root) for p in candidates if p.exists()})


# Owner, 2026-10-26: the app changes daily, so evidence older than yesterday is stale.
EXPIRING = ('.local/evidence', '.local/merged-worktrees')
INVENTORY = (*EXPIRING, '.local/build', '.local/symbols', '.local/release',
             '.local/backups', '.local/nightly', '.local/control/evidence')


def expired(root, keep_days=2, now=None):
    plan = storage_plan(root, {'evidence_days': keep_days}, now=now, inventory=False)
    return [Path(root) / row['path'] for row in plan['rows'] if row['status'] == 'expired']


def remove_expired(paths, root):
    return apply_plan(root, selected=list(paths), inventory=False)


def preserve_symbols(path):
    # Keep symbols beside the compiler cache before removing Build/Products.
    destination = path.parent / 'RetainedSymbols' / path.name
    symbols = []
    for base, dirs, _ in os.walk(path, followlinks=False):
        for name in list(dirs):
            child = Path(base) / name
            if child.is_symlink():
                dirs.remove(name)
            elif name.endswith('.dSYM'):
                symbols.append(child)
                dirs.remove(name)
    for symbol in symbols:
        digest = hashlib.sha256()
        for file in sorted(symbol.rglob('*')):
            if file.is_file():
                digest.update(str(file.relative_to(symbol)).encode())
                digest.update(file.read_bytes())
        target = destination / digest.hexdigest() / symbol.name
        if target.exists():
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.move(str(symbol), str(target))
        root = ROOT
        try:
            root = main_root(path)
            revision = subprocess.check_output(['git', '-C', str(root), 'rev-parse', 'HEAD'], text=True, stderr=subprocess.DEVNULL).strip()
            register(root, target, 'symbols', 'manual-build-cleanup', revision, completed=True)
        except (OSError, ValueError, subprocess.SubprocessError):
            # Legacy utility fixtures/unknown source remain retained, not guessed.
            pass


def remove_candidates(paths, root):
    tracked = subprocess.check_output(['git', '-C', str(root), 'ls-files', '-z']).split(b'\0')
    tracked_paths = [root / os.fsdecode(p) for p in tracked if p]
    # Validate the whole plan before the first deletion.
    for path in paths:
        contained(path, root)
        if any(p == path or path in p.parents for p in tracked_paths):
            raise ValueError(f'Refusing tracked files under {path}')
    for path in paths:
        if path.is_file():
            # Xcode's artifact receipts otherwise point to the deleted binaries
            # and package resolution incorrectly considers them downloaded.
            path.unlink()
        else:
            preserve_symbols(path)
            shutil.rmtree(path)

# Storage records are additive: legacy artifacts without records are inventoried,
# never adopted or expired just because a directory is old.
import contextlib
import csv
import fcntl
import socket
import sys
import tempfile
import threading
import types

KINDS = {'evidence', 'symbols', 'outcome', 'cache-rust', 'cache-ios', 'worktree'}
DEFAULT_POLICY = {'evidence_days': 2, 'cache_days': 7, 'cache_bytes': 32 * 1024**3,
                  'cache_rust_bytes': 16 * 1024**3, 'cache_ios_bytes': 16 * 1024**3,
                  'min_free_bytes': 20 * 1024**3, 'receipt_days': 7}


def main_root(root):
    result = subprocess.run(['git', '-C', str(root), 'worktree', 'list', '--porcelain'],
                            text=True, capture_output=True, check=True)
    return Path(result.stdout.splitlines()[0].removeprefix('worktree ')).resolve()


def state_dir(root):
    root = Path(root).resolve()
    return contained(root / '.local/storage', root)


@contextlib.contextmanager
def transaction(root, blocking=True):
    """One lock inode for every run and worktree of this repository."""
    folder = state_dir(root)
    folder.mkdir(parents=True, exist_ok=True)
    with (folder / 'lifecycle.lock').open('a+') as guard:
        fcntl.flock(guard, fcntl.LOCK_EX | (0 if blocking else fcntl.LOCK_NB))
        yield


def atomic_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(mode='w', dir=path.parent, delete=False) as out:
        temporary = Path(out.name)
        json.dump(value, out, indent=2, sort_keys=True)
        out.write('\n')
        out.flush()
        os.fsync(out.fileno())
    os.replace(temporary, path)


def record_key(path):
    return hashlib.sha256(os.fsencode(os.path.abspath(path))).hexdigest()


def record_path(root, path):
    return state_dir(root) / 'records' / (record_key(path) + '.json')


def artifact_path(root, path):
    root = Path(root).resolve()
    path = Path(path)
    path = contained(path if path.is_absolute() else root / path, root)
    relative = path.relative_to(root)
    if not (relative.parts[0] == '.local' and len(relative.parts) > 1
            or relative.parts[0] == 'target'):
        raise ValueError(f'Artifact must be below Wonder .local/ or target/: {path}')
    if relative.parts[:2] in (('.local', 'storage'), ('.local', 'orchestrate'),
                              ('.local', 'backups')):
        raise ValueError(f'Protected storage or user data: {path}')
    return path


def owner_released(root, path, old):
    """True only when the recorded owner's unit is found in a store and is merged/abandoned with no live
    process or dispatch. An unknown unit or unreadable store is uncertainty, so the claim stays."""
    try:
        rows = owners(root, [old.get('store')])
        index = owner_index(rows)
        known = [r for r in index['unit'].get(old.get('unit'), [])
                 if not old.get('store') or r['_store'] == old['store']]
        return bool(known) and ownership_reason(root, path, dict(old, dispatch=''), owner_rows=rows, index=index) is None
    except (OSError, ValueError, KeyError):
        return False


def _register(root, path, kind, unit, revision, store=None, completed=False, ttl_days=None,
              consumers=None, pin=None, dispatch='', timestamp=None):
    path = artifact_path(root, path)
    if kind not in KINDS or not unit or not revision:
        raise ValueError('Artifact needs type, unit and source revision')
    clock = time.time() if timestamp is None else timestamp
    receipt = record_path(root, path)
    old = json.loads(receipt.read_text()) if receipt.exists() else {}
    if old and not old.get('removed_at') and (old['unit'] != unit or old['type'] != kind):
        if not owner_released(root, path, old):
            raise ValueError(f'Artifact already belongs to {old["unit"]}: {path}')
        takeover = f'{path.relative_to(root)}: {old["unit"]} ({old["type"]}) -> {unit} ({kind})'
        print(f'artifact takeover: {takeover}: previous owner merged/abandoned', file=sys.stderr)
        with (state_dir(root) / 'takeovers.log').open('a') as log:
            log.write(f'{time.strftime("%Y-%m-%dT%H:%M:%S", time.localtime(clock))}\t{takeover}\n')
        # The new owner starts a fresh lifecycle; pins, consumers and completion belong to the old one.
        old = {k: v for k, v in old.items() if k in ('ttl_days',)}
    entry = dict(old, schema=1, path=str(path.relative_to(root)), type=kind, unit=unit,
                 host=socket.gethostname(), revision=revision, dispatch=dispatch,
                 store=str(Path(store).resolve()) if store else '',
                 created_at=old.get('created_at', clock) if not old.get('removed_at') else clock, last_used_at=clock,
                 completed_at=old.get('completed_at') if completed and old.get('completed_at') and not old.get('removed_at')
                    and old.get('revision') == revision else clock if completed else None,
                 ttl_days=ttl_days if ttl_days is not None else old.get('ttl_days'),
                 consumers=list(consumers if consumers is not None else old.get('consumers', [])),
                 pins=dict(old.get('pins', {})), removed_at=None, absent_since=None)
    if pin:
        entry['pins'][pin] = None
    atomic_json(receipt, entry)
    return entry


def register(root, path, kind, unit, revision, **kwargs):
    root = Path(root).resolve()
    with transaction(root):
        return _register(root, path, kind, unit, revision, **kwargs)


def update_record(root, path, **changes):
    path = artifact_path(root, path)
    with transaction(root):
        receipt = record_path(root, path)
        entry = json.loads(receipt.read_text())
        entry.update(changes)
        atomic_json(receipt, entry)
        return entry


def lease_paths(root, path):
    key = record_key(path)
    folder = state_dir(root) / 'leases'
    # Bounded stable inodes (4096 shards). Old full-key leases remain readable
    # and authoritative during a mid-run upgrade; never unlink a live lock inode.
    return folder / (key[:3] + '.lease'), folder / (key + '.lock')


@contextlib.contextmanager
def artifact_lock(root, path, blocking=False):
    current, legacy = lease_paths(root, path)
    current.parent.mkdir(parents=True, exist_ok=True)
    # One command may capture several paths hashing to the same shard. Share
    # that open-file description within a thread; other threads/processes still
    # serialize. runpy callers share the state through this process-local module.
    state = sys.modules.setdefault('_wonder_storage_lease_state', types.ModuleType('_wonder_storage_lease_state'))
    state.__dict__.setdefault('holders', {})
    identity = (os.getpid(), threading.get_ident(), str(current))
    holder = state.holders.get(identity)
    if holder is None:
        guard = current.open('a+')
        try:
            fcntl.flock(guard, fcntl.LOCK_EX | (0 if blocking else fcntl.LOCK_NB))
        except BaseException:
            guard.close()
            raise
        holder = dict(guard=guard, references=0)
        state.holders[identity] = holder
    holder['references'] += 1
    try:
        with contextlib.ExitStack() as stack:
            if legacy.exists():
                old = stack.enter_context(legacy.open('r'))
                fcntl.flock(old, fcntl.LOCK_EX | (0 if blocking else fcntl.LOCK_NB))
            yield holder['guard'].fileno()
    finally:
        holder['references'] -= 1
        if not holder['references']:
            holder['guard'].close()
            del state.holders[identity]


def lease_busy(root, path):
    """Read-only probe; planning never creates a lease or storage directory."""
    for lock in lease_paths(root, path):
        if not lock.exists():
            continue
        try:
            with lock.open('r') as guard:
                fcntl.flock(guard, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
    return False


@contextlib.contextmanager
def artifact_use(root, path, kind, unit, revision, store=None):
    """Single writer per output tree. Children inherit the lease through parent death."""
    root = Path(root).resolve()
    path = artifact_path(root, path)
    with artifact_lock(root, path, blocking=True) as descriptor:
        started = time.monotonic()
        register(root, path, kind, unit, revision, store=store)
        try:
            yield descriptor
        finally:
            update_record(root, path, last_used_at=time.time(), completed_at=time.time(),
                          last_use_seconds=time.monotonic() - started)


def policy(root, overrides=None):
    value = dict(DEFAULT_POLICY)
    path = state_dir(root) / 'policy.json'
    if path.exists():
        value.update(json.loads(path.read_text()))
    value.update(overrides or {})
    for key in DEFAULT_POLICY:
        if not isinstance(value[key], (float, int)) or not math.isfinite(value[key]) or value[key] < 0:
            raise ValueError(f'{key} must be a finite nonnegative number')
    return value


def records(root):
    result = []
    for path in sorted((state_dir(root) / 'records').glob('*.json')):
        entry = json.loads(path.read_text())
        if entry.get('schema') != 1:
            raise ValueError(f'Unknown artifact schema: {path}')
        result.append(entry)
    return result


def overlaps(a, b):
    return a == b or a in b.parents or b in a.parents


def owners(root, stores=(), missing=None):
    """Unit rows of every store. With `missing` (a set), a store that no longer exists is recorded there instead of
    failing the whole plan: its own artifacts stay unknown (safety_reason), everyone else's can still be judged."""
    tables = set((root / '.local/orchestrate').glob('*/units.tsv'))
    tables.update(Path(s) / 'units.tsv' for s in stores if s)
    result = []
    for table in sorted(tables):
        if missing is not None and not table.exists():
            missing.add(str(table.parent.resolve()))
            continue
        # Missing external store is uncertainty, not proof of completion.
        with table.open() as stream:
            reader = csv.DictReader(stream, delimiter='\t')
            if not {'id', 'state', 'worktree'} <= set(reader.fieldnames or []):
                raise ValueError(f'Cannot establish owners from {table}')
            result.extend(dict(row, _store=str(table.parent.resolve())) for row in reader)
    return result


def owner_index(rows):
    result = dict(unit={}, branch={}, worktree={}, worktree_ancestors={})
    for row in rows:
        result['unit'].setdefault(row['id'], []).append(row)
        if row.get('branch'):
            result['branch'].setdefault(row['branch'], []).append(row)
        wt = row.get('worktree', '')
        if wt and ':' not in wt:
            path = Path(os.path.abspath(wt))
            result['worktree'].setdefault(path, []).append(row)
            for parent in path.parents:
                result['worktree_ancestors'].setdefault(parent, []).append(row)
    return result


def ownership_reason(root, path, entry=None, stores=(), owner_rows=None, index=None):
    entry = entry or {}
    if index is None:
        index = owner_index(owner_rows if owner_rows is not None else owners(root, [*stores, entry.get('store')]))
    rows = [r for r in index['unit'].get(entry.get('unit'), [])
            if not entry.get('store') or r['_store'] == entry['store']]
    rows.extend(index['branch'].get(entry.get('branch'), []))
    # Registered sibling worktrees can live outside the main checkout. Their
    # explicit branch/worktree owner still applies; root-relative artifact IDs do not.
    relative_parts = path.relative_to(root).parts if path.is_relative_to(root) else ()
    for part in relative_parts:
        rows.extend(index['unit'].get(part, []))
    for parent in (path, *path.parents):
        rows.extend(index['worktree'].get(parent, []))
    rows.extend(index['worktree_ancestors'].get(path, []))
    for row in rows:
        # A concrete path/branch match protects all runs. A unit-only match may
        # be scoped to its store; reused IDs cannot retire another run's data.
        if row['state'] not in {'merged', 'abandoned'}:
            return f'active consumer {row["id"]}: {row["state"]} on {row.get("host") or "local"}'
        if not row.get('host') and str(row.get('pid', '')).isdigit() and int(row['pid']) > 1:
            try:
                os.kill(int(row['pid']), 0)
                return f'live owner process for {row["id"]}'
            except PermissionError:
                return f'owner process cannot be inspected for {row["id"]}'
            except ProcessLookupError:
                pass
        if row.get('pending_dispatch'):
            return f'pending dispatch for {row["id"]}'
        if entry.get('dispatch') and row.get('dispatch') != entry['dispatch']:
            return f'dispatch changed or worker rehosted: {row["id"]}'
    return None


def usage_snapshot(ignore=()):
    """Inspect all this user's open paths, not just a working directory or PID."""
    result = subprocess.run(['lsof', '-nP', '-a', '-u', str(os.getuid()), '-Fpn'],
                            text=True, capture_output=True, timeout=20)
    if result.returncode not in (0, 1) or not result.stdout.strip():
        raise ValueError('Cannot establish host open-file state (lsof unavailable)')
    # Git's read-only directory watcher can outlive its checkout. It is stopped
    # explicitly when pruning; it is not a build, editor or evidence consumer.
    processes = subprocess.run(['ps', '-axo', 'pid=,args='], text=True, capture_output=True, check=True)
    observers = {int(match.group(1)) for line in processes.stdout.splitlines()
                 if (match := re.match(r'\s*(\d+)\s+(?:\S*/)?git fsmonitor--daemon run(?:\s|$)', line))}
    ignore = set(ignore) | observers
    paths, pid = [], None
    for line in result.stdout.splitlines():
        if line.startswith('p') and line[1:].isdigit():
            pid = int(line[1:])
        elif line.startswith('n/') and pid not in ignore:
            paths.append(Path(line[1:]))
    return paths


def safety_context(root, stores=(), entries=None):
    entries = records(root) if entries is None else entries
    tables = [*stores, *(entry.get('store') for entry in entries)]
    missing = set()
    owner_rows = owners(root, tables, missing)
    tracked = subprocess.run(['git', '-C', str(root), 'ls-files', '-z'],
                             capture_output=True, check=True).stdout.split(b'\0')
    worktrees = subprocess.run(['git', '-C', str(root), 'worktree', 'list', '--porcelain'],
                               text=True, capture_output=True, check=True).stdout.splitlines()
    tracked_paths = {root / os.fsdecode(p) for p in tracked if p}
    worktree_paths = {Path(line[9:]).resolve() for line in worktrees if line.startswith('worktree ')}
    descendants = {}
    for entry in entries:
        for parent in (root / entry['path']).parents:
            if parent == root:
                break
            descendants.setdefault(parent, []).append(entry)
    return dict(entries=entries, owners=owner_rows, owner_index=owner_index(owner_rows), missing_stores=missing, tracked=tracked_paths,
                tracked_ancestors={parent for p in tracked_paths for parent in (p, *p.parents)},
                worktree_ancestors={parent for p in worktree_paths for parent in (p, *p.parents)},
                descendants=descendants)



def safety_reason(root, path, entry=None, stores=(), opened=None, context=None):
    try:
        artifact_path(root, path)
        if entry and entry.get('type') in {'evidence', 'symbols'} and path.is_dir() and any(p.is_symlink() for p in path.rglob('*')):
            return 'symlink within evidence: ownership uncertain'
        context = safety_context(root, stores) if context is None else context
        store = (entry or {}).get('store')
        if store and str(Path(store).resolve()) in context.get('missing_stores', ()):
            return f'ownership/host state unknown: its run store {store} no longer exists'
        reason = ownership_reason(root, path, entry, stores, index=context['owner_index'])
        if reason:
            return reason
        if path in context['tracked_ancestors'] or any(p in context['tracked'] for p in path.parents):
            return 'tracked source under candidate'
        if path in context['worktree_ancestors']:
            return 'registered worktree under candidate'
        for child in context['descendants'].get(path, []):
            child_path = root / child['path']
            if lease_busy(root, child_path):
                return 'active descendant artifact/build lease'
        if 'opened_ancestors' not in context:
            used_paths = opened if opened is not None else usage_snapshot()
            context['opened_ancestors'] = {parent for p in used_paths if p == root or root in p.parents
                                           for parent in (p, *p.parents)}
        if path in context['opened_ancestors']:
            return 'open file or live process uses candidate'
        if entry and entry.get('host') != socket.gethostname():
            return 'owner host differs; inactivity not verified'
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        return f'ownership/host state unknown: {error}'
    return None


def allocation(path):
    """One lstat per entry; no symlink following; hard links counted once.

    These are allocated/logical bytes, not APFS clone-exclusive reclaimability.
    scandir avoids constructing/resolving a Path for every compiler output file.
    """
    logical = allocated = 0
    seen = set()
    items = [(os.fspath(path), os.lstat(path))]
    while items:
        name, info = items.pop()
        identity = (info.st_dev, info.st_ino)
        if identity in seen:
            continue
        seen.add(identity)
        logical += info.st_size
        allocated += info.st_blocks * 512
        if stat.S_ISDIR(info.st_mode):
            with os.scandir(name) as entries:
                items.extend((entry.path, entry.stat(follow_symlinks=False)) for entry in entries)
    return {'logical_bytes': logical, 'allocated_bytes': allocated}


def storage_plan(root, overrides=None, stores=(), now=None, opened=None, inventory=True, selected=None):
    root = Path(root).resolve()
    clock = time.time() if now is None else now
    limits = policy(root, overrides)
    entries = records(root)
    opened = usage_snapshot() if opened is None else opened
    try:
        context, context_error = safety_context(root, stores, entries), None
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        context, context_error = None, f'ownership/host state unknown: {error}'
    rows = []
    for entry in entries:
        path = artifact_path(root, root / entry['path'])
        row = dict(path=entry['path'], type=entry['type'], owner=entry['unit'],
                   classification='reusable cache' if entry['type'].startswith('cache-') else 'temporary evidence'
                       if entry['type'] in {'evidence', 'symbols'} else 'durable outcome' if entry['type'] == 'outcome' else 'active work',
                   revision=entry['revision'], last_used_at=entry['last_used_at'],
                   retention_days=entry.get('ttl_days'), status='retained', reason='',
                   uncertainty='APFS exclusive/reclaimable bytes unknown', logical_bytes=0, allocated_bytes=0)
        if entry.get('removed_at') and not path.exists():
            row.update(status='absent', reason='previously removed; outcome retained')
        elif not path.exists():
            row.update(status='absent', reason='artifact is not present')
        else:
            try:
                row.update(allocation(path))
            except OSError as error:
                row.update(status='skipped', reason=f'allocation changed or unreadable: {error}')
                rows.append(row)
                continue
            reason = context_error or safety_reason(root, path, entry, stores, opened, context)
            if reason:
                row.update(status='skipped', reason=reason)
            elif entry.get('consumers'):
                row.update(reason='active consumers: ' + ', '.join(entry['consumers']))
            elif any(until is None or until > clock for until in entry.get('pins', {}).values()):
                row.update(reason='pinned: ' + ', '.join(entry['pins']))
            elif entry['type'] in {'outcome', 'worktree'}:
                row.update(reason='compact outcome or worktree lifecycle owns removal')
            elif not entry.get('completed_at'):
                row.update(reason='owner has not released artifact')
            else:
                cache = entry['type'].startswith('cache-')
                ttl = entry.get('ttl_days')
                ttl = (limits['cache_days'] if cache else limits['evidence_days']) if ttl is None else ttl
                origin = entry['last_used_at'] if cache else entry['completed_at']
                row['retention_days'] = ttl
                if clock >= origin + ttl * 86400:
                    row.update(status='expired', reason='last-use TTL' if cache else 'completed-evidence TTL')
                else:
                    row.update(reason='useful cache within budget' if cache else 'within completed-evidence TTL')
                if lease_busy(root, path):
                    row.update(status='skipped', reason='active artifact lease (including surviving children)')
        rows.append(row)
    # Pins on small results/symbols must not require retaining a whole build tree.
    # Only released descendants can be handed off; active readers/leases stay put.
    by_path = {entry['path']: entry for entry in entries}
    row_by_path = {row['path']: row for row in rows}
    descendant_index = context['descendants'] if context else {}
    for row in sorted(rows, key=lambda r: len(Path(r['path']).parts), reverse=True):
        children = [row_by_path[child['path']] for child in descendant_index.get(root / row['path'], [])
                    if row_by_path[child['path']]['status'] not in {'expired', 'absent'}]
        if children and row['status'] in {'expired', 'retained'}:
            movable = (row['type'].startswith('cache-') or row['type'] == 'evidence') and all(
                child['type'] in {'symbols', 'evidence'} and child['status'] == 'retained'
                and by_path[child['path']].get('completed_at')
                and not by_path[child['path']].get('consumers')
                and not lease_busy(root, root / child['path']) for child in children)
            if movable:
                row['handoff'] = [child['path'] for child in children]
            else:
                row.update(status='retained', reason='nested owned artifact is retained or in use')
        if selected is not None and not any(overlaps(root / row['path'], p) for p in selected):
            row.update(status='retained', reason='outside requested cleanup scope')
    # Budget eviction is LRU, and only explicitly released caches are eligible.
    caches = [r for r in rows if r['type'].startswith('cache-') and r['status'] != 'absent']
    free = shutil.disk_usage(root).free
    used = sum(r['allocated_bytes'] for r in caches if r['status'] != 'expired')
    per_kind = {kind: sum(r['allocated_bytes'] for r in caches if r['type'] == kind and r['status'] != 'expired')
                for kind in ('cache-rust', 'cache-ios')}
    projected_free = free + sum(r['allocated_bytes'] for r in rows if r['status'] == 'expired')
    for row in sorted(caches, key=lambda r: r['last_used_at']):
        if row['status'] != 'retained' or row['reason'] != 'useful cache within budget':
            continue
        key = row['type'].replace('-', '_') + '_bytes'
        if used > limits['cache_bytes'] or per_kind[row['type']] > limits[key] or projected_free < limits['min_free_bytes']:
            row.update(status='expired', reason='inactive cache LRU: size/free-space budget')
            used -= row['allocated_bytes']
            per_kind[row['type']] -= row['allocated_bytes']
            projected_free += row['allocated_bytes']
    if inventory:
        known = {root / entry['path'] for entry in entries}
        known_ancestors = {parent for p in known for parent in p.parents}
        legacy = list(discover(root))
        for folder in INVENTORY:
            base = root / folder
            if base.is_dir() and not base.is_symlink():
                legacy.extend(base.iterdir())
        legacy = set(legacy)
        for path in sorted(legacy):
            if path in known_ancestors or any(p in known for p in (path, *path.parents)):
                continue
            # Do not count descendants of another candidate twice.
            if any(parent in legacy for parent in path.parents):
                continue
            try:
                size = allocation(path)
                # Unknown artifacts are never deletion candidates. Do not run
                # per-file Git/open-file scans for thousands of legacy captures.
                reason = context_error or ownership_reason(root, path, stores=stores, index=context['owner_index'])
                try:
                    artifact_path(root, path)
                except ValueError as error:
                    reason = str(error)
            except OSError as error:
                size, reason = {}, str(error)
            rows.append(dict(path=str(path.relative_to(root)), type='unknown', owner='user' if '.local/backups' in str(path) else None,
                             classification='durable user data' if '.local/backups' in str(path) else
                             'reusable/reproducible; ownership unknown' if '.local/build' in str(path) else
                             'temporary evidence; ownership unknown' if any(part in str(path) for part in EXPIRING) else 'unknown',
                             status='skipped', reason=reason or 'legacy artifact: ownership/retention not registered',
                             uncertainty='requires explicit classification; no age-based deletion', **size))
    # An ancestor and descendant cannot both be deleted or double counted.
    expiring_paths = {Path(row['path']) for row in rows if row['status'] == 'expired'}
    for row in rows:
        if row['status'] == 'expired' and any(parent in expiring_paths for parent in Path(row['path']).parents):
            row.update(status='skipped', reason='covered by ancestor candidate')
    return dict(schema=1, policy=limits, free_bytes=free, rows=rows,
                candidate_allocated_bytes=sum(r.get('allocated_bytes', 0) for r in rows if r['status'] == 'expired'),
                reclaimable_bytes=None)


def sha256_file(path):
    value = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            value.update(chunk)
    return value.hexdigest()


def copy_verified(source, destination):
    """Restartable selective handoff. Never follow source or destination symlinks."""
    files = [source] if source.is_file() else list(source.rglob('*'))
    if source.is_symlink() or any(p.is_symlink() for p in files):
        raise ValueError(f'Handoff symlink ownership unknown: {source}')
    copied = 0
    for path in files:
        if not path.is_file():
            continue
        target = destination if source.is_file() else destination / path.relative_to(source)
        cursor = target
        while cursor != cursor.parent:
            if cursor.is_symlink():
                raise ValueError(f'Handoff target is a symlink: {cursor}')
            cursor = cursor.parent
        target.parent.mkdir(parents=True, exist_ok=True)
        expected = sha256_file(path)
        if target.exists():
            if not target.is_file() or sha256_file(target) != expected:
                raise ValueError(f'Handoff differs at {target}; source retained')
        else:
            # Atomic individual copies permit recovery after a killed copy process.
            temporary = target.parent / ('.copy-' + record_key(target))
            if temporary.is_symlink():
                raise ValueError(f'Handoff temporary path is a symlink: {temporary}')
            try:
                shutil.copy2(path, temporary)
                if sha256_file(temporary) != expected:
                    raise ValueError(f'Handoff integrity failure: {target}')
                os.replace(temporary, target)
            finally:
                Path(temporary).unlink(missing_ok=True)
            copied += path.stat().st_size
    return copied


def handoff_cache(root, entry, row, limits):
    """Detach useful small results and current symbols before cache eviction."""
    import runpy
    copied, released = 0, []
    for relative in row.get('handoff', []):
        source = root / relative
        old_receipt = record_path(root, source)
        child = json.loads(old_receipt.read_text())
        destination = root / '.local' / ('symbols' if child['type'] == 'symbols' else 'evidence') / 'handoff' / record_key(source) / source.name
        artifact_path(root, destination)
        copied += copy_verified(source, destination)
        retained = _register(root, destination, child['type'], child['unit'], child['revision'],
                             store=child.get('store'), completed=True, ttl_days=child.get('ttl_days'),
                             timestamp=child['completed_at'], dispatch=child.get('dispatch', ''))
        retained.update(pins=child.get('pins', {}), created_at=child['created_at'],
                        last_used_at=child['last_used_at'], original_path=relative)
        atomic_json(record_path(root, destination), retained)
        child.update(removed_at=time.time(), relocated_to=str(destination.relative_to(root)))
        atomic_json(old_receipt, child)
        released.append(source)
    if entry['type'].startswith('cache-') and time.time() < entry['last_used_at'] + limits['evidence_days'] * 86400:
        # Logs/results generated by the last build retain their original age, not
        # the time this copy was made. No full DerivedData/target is kept for them.
        destination = root / '.local/evidence/cache-handoff' / record_key(root / entry['path']) / str(int(entry['last_used_at']))
        artifact_path(root, destination)
        preserve = runpy.run_path(str(Path(__file__).with_name('orch')))['preserve_build_evidence']
        existing = set(destination.rglob('*')) if destination.exists() else set()
        manifest = preserve(root / entry['path'], destination, apply=True, excluded=released)
        if manifest:
            copied += sum(p.stat().st_size for p in destination.rglob('*') if p.is_file() and p not in existing)
            _register(root, destination, 'evidence', entry['unit'], entry['revision'],
                      store=entry.get('store'), completed=True, timestamp=entry['last_used_at'],
                      dispatch=entry.get('dispatch', ''))
    return copied


def apply_plan(root, overrides=None, stores=(), now=None, opened=None, inventory=True, selected=None):
    root = Path(root).resolve()
    with transaction(root):
        opened = usage_snapshot() if opened is None else opened
        result = storage_plan(root, overrides, stores, now, opened, inventory, selected)
        context = safety_context(root, stores) if any(r['status'] == 'expired' for r in result['rows']) else None
        before = shutil.disk_usage(root).free
        removed = copied = 0
        for row in result['rows']:
            if row['status'] != 'expired':
                continue
            path = root / row['path']
            try:
                with artifact_lock(root, path):
                    receipt = record_path(root, path)
                    entry = json.loads(receipt.read_text())
                    # A new dispatch/lease/consumer/pin may have appeared since preview.
                    context['owners'] = owners(root, [*stores, entry.get('store')], context.setdefault('missing_stores', set()))
                    context['owner_index'] = owner_index(context['owners'])
                    reason = safety_reason(root, path, entry, stores, opened, context)
                    if reason:
                        row.update(status='skipped', reason=reason)
                        continue
                    if entry['type'].startswith('cache-') or row.get('handoff'):
                        copied += handoff_cache(root, entry, row, result['policy'])
                    if path.is_dir():
                        shutil.rmtree(path)
                    else:
                        path.unlink()
                    entry.update(removed_at=time.time(), removed_allocated_bytes=row['allocated_bytes'])
                    atomic_json(receipt, entry)
                    removed += row['allocated_bytes']
                    row['status'] = 'removed'
                    for child_row in result['rows']:
                        if child_row['path'] in row.get('handoff', []):
                            child_row.update(status='relocated', reason='verified handoff before cache eviction')
            except (OSError, ValueError) as error:
                row.update(status='skipped', reason=f'cleanup deferred: {error}')
        # Large detail is gone; per-artifact tombstones are temporary too. The
        # per-unit outcome and handoff's original-path index remain compact.
        clock = time.time() if now is None else now
        for row in result['rows']:
            if row['status'] != 'absent':
                continue
            path = root / row['path']
            receipt = record_path(root, path)
            entry = json.loads(receipt.read_text())
            if not entry.get('completed_at') or entry.get('pins') or entry.get('consumers') or lease_busy(root, path):
                continue
            try:
                if ownership_reason(root, path, entry, stores):
                    continue
            except (OSError, ValueError) as error:
                row.update(status='skipped', reason=f'ownership unknown: {error}')
                continue
            absent = entry.get('absent_since') or entry.get('removed_at') or clock
            if clock >= absent + result['policy']['receipt_days'] * 86400:
                receipt.unlink()
                row.update(status='metadata-expired', reason='compact unit outcome remains')
            elif not entry.get('absent_since'):
                entry['absent_since'] = absent
                atomic_json(receipt, entry)
        result.update(removed_allocated_bytes=removed, copied_evidence_bytes=copied, free_bytes_before=before,
                      free_bytes_after=shutil.disk_usage(root).free)
        result['filesystem_free_delta_bytes'] = result['free_bytes_after'] - before
        result['free_floor_satisfied_measured'] = result['free_bytes_after'] >= result['policy']['min_free_bytes']
        result['reclaimed_bytes'] = None  # Concurrent writers/snapshots make df deltas unattributable.
        atomic_json(state_dir(root) / 'last-cleanup.json', result)
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, help='Main checkout (default: Git common checkout)')
    parser.add_argument('--apply', action='store_true')
    parser.add_argument('--expired-only', action='store_true', help='Apply registered TTL and cache budgets only')
    parser.add_argument('--keep-days', type=float, help='Completed evidence TTL (default 2 days)')
    parser.add_argument('--cache-gib', type=float)
    parser.add_argument('--min-free-gib', type=float)
    parser.add_argument('--rust-cache-gib', type=float)
    parser.add_argument('--ios-cache-gib', type=float)
    parser.add_argument('--cache-days', type=float)
    parser.add_argument('--store', action='append', default=[], help='Additional owning run store (read-only)')
    parser.add_argument('--register', type=Path, help='Classify an explicitly owned generated artifact')
    parser.add_argument('--kind', choices=sorted(KINDS))
    parser.add_argument('--unit')
    parser.add_argument('--revision')
    parser.add_argument('--completed', action='store_true')
    parser.add_argument('--pin', type=Path)
    parser.add_argument('--unpin', type=Path)
    parser.add_argument('--reason')
    parser.add_argument('--pin-days', type=float, help='Optional pin expiry; no value means documented ongoing need')
    parser.add_argument('--run', type=Path, help='Lease writable build output and measure its last use')
    parser.add_argument('--xcode-directory', type=Path, help='Inventory a known Wonder DerivedData directory')
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    root = args.root.resolve() if args.root else main_root(ROOT)
    if args.register:
        if not args.kind or not args.unit or not args.revision:
            parser.error('--register requires --kind, --unit and --revision')
        print(json.dumps(register(root, args.register, args.kind, args.unit, args.revision,
                                 store=args.store[0] if args.store else None,
                                 completed=args.completed, ttl_days=args.keep_days, pin=args.reason), indent=2))
        return
    if args.pin or args.unpin:
        if not args.reason:
            parser.error('pin/unpin requires a documented --reason')
        with transaction(root):
            path = artifact_path(root, args.pin or args.unpin)
            receipt = record_path(root, path)
            entry = json.loads(receipt.read_text())
            if args.pin:
                entry['pins'][args.reason] = time.time() + args.pin_days * 86400 if args.pin_days is not None else None
            else:
                entry['pins'].pop(args.reason, None)
            atomic_json(receipt, entry)
        print(json.dumps(entry, indent=2))
        return
    if args.run:
        command = args.command[1:] if args.command[:1] == ['--'] else args.command
        if not command or args.kind not in {'cache-rust', 'cache-ios'} or not args.unit or not args.revision:
            parser.error('--run requires cache --kind, --unit, --revision and -- COMMAND')
        with artifact_use(root, args.run, args.kind, args.unit, args.revision,
                          args.store[0] if args.store else None) as descriptor:
            sys.exit(subprocess.run(command, pass_fds=(descriptor,)).returncode)
    overrides = {}
    if args.keep_days is not None:
        overrides['evidence_days'] = args.keep_days
    if args.cache_gib is not None:
        overrides['cache_bytes'] = args.cache_gib * 1024**3
    for argument, key in ((args.rust_cache_gib, 'cache_rust_bytes'), (args.ios_cache_gib, 'cache_ios_bytes')):
        if argument is not None:
            overrides[key] = argument * 1024**3
    if args.cache_days is not None:
        overrides['cache_days'] = args.cache_days
    if args.min_free_gib is not None:
        overrides['min_free_bytes'] = args.min_free_gib * 1024**3
    if args.xcode_directory:
        directory = contained(args.xcode_directory, root)
        if not xcode_directory(directory):
            parser.error('Expected a Wonder DerivedData directory with info.plist')
    try:
        options = dict(inventory=not args.expired_only, selected=[directory] if args.xcode_directory else None)
        report = apply_plan(root, overrides, args.store, **options) if args.apply else storage_plan(root, overrides, args.store, **options)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, f'Cleanup skipped: {error}\n')
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
