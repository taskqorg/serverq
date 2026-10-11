#!/usr/bin/env python3
"""Read-only Linux server audit. Author: Tight <tight@kirs.online>."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parent


def capacity(total, available, minimum):
    if total <= 0:
        return {'status': 'unknown', 'detail': 'Capacity unavailable'}
    percent = available * 100 / total
    return {'status': 'warn' if percent < minimum else 'ok',
            'detail': f'{percent:.1f}% available; minimum {minimum}%',
            'available': available, 'total': total}


def command(argv):
    try:
        result = subprocess.run(argv, capture_output=True, text=True, timeout=15)
    except (OSError, subprocess.SubprocessError) as exc:
        raise RuntimeError(f'{argv[0]} collection failed: {type(exc).__name__}') from exc
    if result.returncode:
        raise RuntimeError(f'{argv[0]} collection failed: exit {result.returncode}')
    return result.stdout


def failed_units():
    try:
        output = command(['systemctl', '--failed', '--no-legend', '--plain', '--no-pager'])
        units = [line.split()[0] for line in output.splitlines() if line.strip()]
        return {'status': 'warn' if units else 'ok', 'detail': f'{len(units)} failed system units', 'units': units}
    except RuntimeError as exc:
        return {'status': 'unknown', 'detail': str(exc)}


def journal_policy(text):
    values = {}
    section = ''
    for line in text.splitlines():
        line = line.strip()
        if line.startswith('['):
            section = line
        elif section == '[Journal]' and '=' in line and not line.startswith(('#', ';')):
            key, value = line.split('=', 1)
            values[key.strip()] = value.strip()
    def positive(value):
        return bool(re.fullmatch(r'[1-9][0-9]*[KMGTPE]?', value or ''))
    keys = ('SystemMaxUse', 'SystemKeepFree', 'RuntimeMaxUse')
    missing = [key for key in keys if not positive(values.get(key))]
    return {'status': 'warn' if missing else 'ok',
            'detail': 'Missing explicit positive journal bounds: ' + ', '.join(missing) if missing else 'Persistent and runtime journals have explicit bounds',
            'values': {key: values.get(key) for key in keys}}


def journal():
    try:
        return journal_policy(command(['systemd-analyze', 'cat-config', 'systemd/journald.conf']))
    except RuntimeError as exc:
        return {'status': 'unknown', 'detail': str(exc)}


def journal_fingerprint():
    digest = hashlib.sha256()
    for directory in ('/usr/lib/systemd', '/usr/local/lib/systemd', '/run/systemd', '/etc/systemd'):
        root = Path(directory)
        paths = [root / 'journald.conf'] + sorted((root / 'journald.conf.d').glob('*.conf'))
        for path in paths:
            digest.update(str(path).encode())
            digest.update(b'present' if path.exists() else b'absent')
            if path.is_symlink():
                digest.update(os.readlink(path).encode())
            if path.exists():
                digest.update(path.read_bytes())
    return digest.hexdigest()


def cached(cache, key, fingerprint, now, ttl, probe):
    previous = cache.get(key)
    if previous and previous['fingerprint'] == fingerprint and 0 <= now - previous['at'] < ttl:
        return dict(previous['result'], cached=True, checked_at=previous['at'])
    result = probe()
    if result['status'] != 'unknown':
        cache[key] = {'fingerprint': fingerprint, 'at': now, 'result': result}
    else:
        cache.pop(key, None)
    return dict(result, cached=False, checked_at=now)


def load_state(path):
    if not path.exists():
        return {'schema': 1, 'cache': {}, 'incidents': {}}
    try:
        state = json.loads(path.read_text())
        if not isinstance(state, dict) or state.get('schema') != 1:
            raise ValueError('Unsupported state schema')
        for key in ('cache', 'incidents'):
            if key in state and not isinstance(state[key], dict):
                raise ValueError('Invalid state structure')
        return state
    except (json.JSONDecodeError, TypeError) as exc:
        raise ValueError('Invalid state; preserve it and investigate') from exc


def save_json(path, value):
    fd, temporary = tempfile.mkstemp(prefix='.serverq-', dir=path.parent)
    try:
        with os.fdopen(fd, 'w') as stream:
            json.dump(value, stream, indent=2)
            stream.write('\n')
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def incidents(state, results, now):
    records = state.setdefault('incidents', {})
    for key, result in results.items():
        previous = records.get(key)
        if result['status'] == 'warn':
            if not previous or previous['state'] == 'verified':
                previous = {'first_seen': now, 'occurrences': 0, 'state': 'observed'}
            previous.update(last_seen=now, occurrences=previous['occurrences'] + 1)
            records[key] = previous
        elif result['status'] == 'ok' and previous and previous['state'] != 'verified':
            previous.update(state='verified', verified_at=now)


def audit(home, filesystem, force=False):
    rules_bytes = (ROOT / 'rules.json').read_bytes()
    rules = json.loads(rules_bytes)
    now = time.time()
    state = load_state(home / 'state.json')
    revision = hashlib.sha256(rules_bytes + Path(__file__).read_bytes()).hexdigest()
    if force or state.get('revision') != revision:
        state['cache'] = {}
    cache = state.setdefault('cache', {})
    results = {}
    try:
        disk = shutil.disk_usage(filesystem)
        results['disk-headroom'] = capacity(disk.total, disk.free, rules['disk-headroom']['minimum_percent'])
        inode = os.statvfs(filesystem)
        results['inode-headroom'] = capacity(inode.f_files, inode.f_favail, rules['inode-headroom']['minimum_percent'])
    except OSError as exc:
        for key in ('disk-headroom', 'inode-headroom'):
            results[key] = {'status': 'unknown', 'detail': f'Filesystem collection failed: {type(exc).__name__}'}
    results['failed-services'] = failed_units()
    try:
        results['bounded-journal'] = cached(cache, 'bounded-journal', journal_fingerprint(), now,
                                            rules['bounded-journal']['ttl_seconds'], journal)
    except OSError as exc:
        results['bounded-journal'] = {'status': 'unknown', 'detail': f'Journal config unavailable: {type(exc).__name__}'}
    incidents(state, results, now)
    report = {'schema': 1, 'at': now, 'revision': revision, 'filesystem': filesystem,
              'results': results, 'incidents': state.get('incidents', {}),
              'manual_rules': [key for key, rule in rules.items() if rule.get('manual')]}
    state.update(revision=revision, last_run=now)
    save_json(home / 'state.json', state)
    save_json(home / 'report.json', report)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['check'])
    parser.add_argument('--home', type=Path, default=Path.home() / '.serverq')
    parser.add_argument('--filesystem', default='/')
    parser.add_argument('--force', action='store_true', help='Bypass configuration cache')
    args = parser.parse_args()
    try:
        args.home.mkdir(mode=0o700, parents=True, exist_ok=True)
        os.chmod(args.home, 0o700)
        with (args.home / 'lock').open('a') as lock:
            os.chmod(args.home / 'lock', 0o600)
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            report = audit(args.home, args.filesystem, args.force)
        print(json.dumps(report, indent=2))
        statuses = {value['status'] for value in report['results'].values()}
        return 2 if 'unknown' in statuses else 1 if 'warn' in statuses else 0
    except (OSError, ValueError, KeyError, TypeError) as exc:
        print(f'ServerQ check failed; no healthy result claimed: {exc}', file=sys.stderr)
        return 2


if __name__ == '__main__':
    sys.exit(main())
