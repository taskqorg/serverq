#!/usr/bin/env python3
"""Read-only Linux server audit and conservative self-updater. Author: Tight <tight@kirs.online>."""
import argparse
import base64
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import urllib.parse
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parent
RELEASE_API_URL = 'https://api.github.com/repos/taskqorg/serverq/releases/latest'
ASSET_PREFIX = 'https://github.com/taskqorg/serverq/releases/download/'
MAX_METADATA = 256 * 1024
MAX_ARCHIVE = 8 * 1024 * 1024
MAX_FILES = 64
MAX_UNPACKED = 16 * 1024 * 1024
KEEP_RELEASES = 3
POSTCHECK_TIMEOUT = 15
VERSION_RE = re.compile(r'^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$')
OWNED_MARKER = '.serverq-owned.json'
STAGING_MARKER = '.serverq-staging-owned.json'
ALLOWED_DOWNLOAD_HOSTS = {'api.github.com', 'github.com', 'release-assets.githubusercontent.com', 'objects.githubusercontent.com'}


class UpdateError(RuntimeError):
    pass


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
    except RuntimeError as exc:
        return {'status': 'unknown', 'detail': str(exc)}
    units = [line.split()[0] for line in output.splitlines() if line.strip()]
    return {'status': 'warn' if units else 'ok',
            'detail': f'{len(units)} failed system units', 'units': units}


def journal_policy(text):
    values = {}
    section = None
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
    except (OSError, json.JSONDecodeError, TypeError) as exc:
        raise ValueError('Invalid state; preserve it and investigate') from exc


def save_json(path, value):
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix='.serverq-', dir=path.parent)
    try:
        with os.fdopen(fd, 'w') as stream:
            json.dump(value, stream, indent=2)
            stream.write('\n')
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary, 0o600)
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
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


def version_key(value):
    if not isinstance(value, str) or not VERSION_RE.fullmatch(value):
        raise UpdateError('invalid version')
    return tuple(int(part) for part in value.split('.'))


def metadata(root=ROOT):
    try:
        value = json.loads((root / 'release.json').read_text())
    except (OSError, json.JSONDecodeError) as exc:
        raise UpdateError('invalid release metadata') from exc
    if (not isinstance(value, dict) or value.get('schema') != 1 or
            not isinstance(value.get('version'), str) or
            not isinstance(value.get('app_schema'), int) or isinstance(value.get('app_schema'), bool) or
            not isinstance(value.get('minimum_updater'), str)):
        raise UpdateError('invalid release metadata')
    version_key(value['version'])
    version_key(value['minimum_updater'])
    return value


def with_lock(home):
    home.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(home, 0o700)
    stream = (home / 'lock').open('a')
    os.chmod(home / 'lock', 0o600)
    try:
        fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError as exc:
        stream.close()
        raise UpdateError('ServerQ is already running; update was not started') from exc
    return stream


def inherited_lock(home, fd):
    try:
        descriptor = os.fstat(fd)
        lock = os.stat(home / 'lock')
        if (descriptor.st_dev, descriptor.st_ino) != (lock.st_dev, lock.st_ino):
            raise OSError('wrong lock file')
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError as exc:
        raise UpdateError('inherited lock is invalid or unavailable') from exc


class GithubRedirectHandler(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        parsed = urllib.parse.urlparse(newurl)
        if parsed.scheme != 'https' or parsed.hostname not in ALLOWED_DOWNLOAD_HOSTS:
            raise UpdateError('release redirect is unsafe')
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def fetch(url, limit, timeout=8):
    parsed = urllib.parse.urlparse(url)
    if parsed.scheme != 'https' or parsed.hostname not in ALLOWED_DOWNLOAD_HOSTS:
        raise UpdateError('release URL is unsafe')
    opener = urllib.request.build_opener(GithubRedirectHandler())
    try:
        with opener.open(url, timeout=timeout) as response:
            final = urllib.parse.urlparse(response.geturl())
            if final.scheme != 'https' or final.hostname not in ALLOWED_DOWNLOAD_HOSTS:
                raise UpdateError('release redirect is unsafe')
            data = response.read(limit + 1)
    except UpdateError:
        raise
    except OSError as exc:
        raise UpdateError('release download failed') from exc
    if len(data) > limit:
        raise UpdateError('release download exceeds bounded size')
    return data


def expected_asset_url(version, name):
    return ASSET_PREFIX + 'v' + version + '/' + name


def release_assets(release):
    if not isinstance(release, dict) or release.get('draft') is not False or release.get('prerelease') is not False:
        raise UpdateError('latest release is not a stable public release')
    tag = release.get('tag_name')
    if not isinstance(tag, str) or not tag.startswith('v'):
        raise UpdateError('release tag is malformed')
    version = tag[1:]
    version_key(version)
    expected_archive = 'serverq-' + version + '.zip'
    expected_manifest = 'serverq-' + version + '.manifest.json'
    assets = release.get('assets')
    if not isinstance(assets, list):
        raise UpdateError('release metadata has no assets')
    matched = {expected_archive: [], expected_manifest: []}
    for asset in assets:
        if isinstance(asset, dict) and asset.get('name') in matched:
            matched[asset['name']].append(asset.get('browser_download_url'))
    if any(len(urls) != 1 for urls in matched.values()):
        raise UpdateError('release assets are missing or duplicated')
    for name, urls in matched.items():
        if urls[0] != expected_asset_url(version, name):
            raise UpdateError('release asset URL is unsafe or noncanonical')
    return version, matched[expected_manifest][0], matched[expected_archive][0]


def validate_manifest(value, release_version):
    needed = ('schema', 'version', 'app_schema', 'minimum_updater', 'archive', 'sha256', 'size')
    if not isinstance(value, dict) or any(key not in value for key in needed):
        raise UpdateError('release manifest is malformed or incompatible')
    if (value['schema'] != 1 or value['version'] != release_version or
            value['archive'] != 'serverq-' + release_version + '.zip' or
            not isinstance(value['app_schema'], int) or isinstance(value['app_schema'], bool) or
            not isinstance(value['size'], int) or isinstance(value['size'], bool) or
            not isinstance(value['minimum_updater'], str) or
            not isinstance(value['sha256'], str) or not re.fullmatch(r'[0-9a-f]{64}', value['sha256'])):
        raise UpdateError('release manifest is malformed or incompatible')
    version_key(value['version'])
    version_key(value['minimum_updater'])
    if value['size'] < 1 or value['size'] > MAX_ARCHIVE:
        raise UpdateError('release archive size is unsafe')
    return value


def unpack_archive(data, manifest, staging):
    if hashlib.sha256(data).hexdigest() != manifest['sha256']:
        raise UpdateError('release checksum mismatch')
    if len(data) != manifest['size']:
        raise UpdateError('release archive size mismatch')
    archive_path = staging / 'candidate.zip'
    archive_path.write_bytes(data)
    try:
        with zipfile.ZipFile(archive_path) as bundle:
            entries = bundle.infolist()
            if not entries or len(entries) > MAX_FILES or sum(item.file_size for item in entries) > MAX_UNPACKED:
                raise UpdateError('release archive exceeds safety limits')
            prefix = 'serverq-' + manifest['version'] + '/'
            seen = set()
            for item in entries:
                path = Path(item.filename)
                normalized = path.as_posix()
                if normalized in seen:
                    raise UpdateError('release archive has duplicate paths')
                seen.add(normalized)
                if (not item.filename.startswith(prefix) or path.is_absolute() or '..' in path.parts or
                        item.is_dir() or stat.S_ISLNK(item.external_attr >> 16)):
                    raise UpdateError('release archive has unsafe paths or links')
            bundle.extractall(staging / 'unpack')
    except zipfile.BadZipFile as exc:
        raise UpdateError('release archive is invalid') from exc
    candidate = staging / 'unpack' / ('serverq-' + manifest['version'])
    required = ('serverq.py', 'rules.json', 'release.json', 'README.md', 'AGENTS.md', 'tests/test_serverq.py')
    if not candidate.is_dir() or any(not (candidate / name).is_file() for name in required):
        raise UpdateError('release archive is incomplete')
    candidate_meta = metadata(candidate)
    if candidate_meta['version'] != manifest['version'] or candidate_meta['app_schema'] != manifest['app_schema']:
        raise UpdateError('release metadata does not match manifest')
    return candidate


def candidate_check(candidate, home):
    env = dict(os.environ, SERVERQ_PACKAGED_PREFLIGHT='1')
    tests = subprocess.run([sys.executable, '-m', 'unittest', 'discover', '-s', str(candidate / 'tests'), '-v'],
                           cwd=candidate, env=env, capture_output=True, text=True, timeout=15)
    if tests.returncode:
        raise UpdateError('candidate packaged tests failed')
    isolated = home / 'preflight-state'
    result = subprocess.run([sys.executable, str(candidate / 'serverq.py'), 'check', '--home', str(isolated)],
                            capture_output=True, text=True, timeout=15)
    if result.returncode not in (0, 1):
        raise UpdateError('candidate read-only check failed')


PACKAGE_FILES = ('serverq.py', 'rules.json', 'release.json', 'README.md', 'AGENTS.md',
                 'tests/test_serverq.py', 'tests/test_updates.py', 'systemd/serverq-check.service',
                 'systemd/serverq-check.timer', 'systemd/serverq-update.service', 'systemd/serverq-update.timer')
PIN_FILE = 'pin.json'
TRANSACTION_FILE = 'transaction.json'
TRANSACTION_MAX_BYTES = 512 * 1024


def file_hashes(path):
    hashes = {}
    for relative in PACKAGE_FILES:
        target = path / relative
        if not target.is_file() or target.is_symlink():
            raise UpdateError('installed package is incomplete')
        hashes[relative] = hashlib.sha256(target.read_bytes()).hexdigest()
    return hashes


def valid_owned(path, home=None, version=None, digest=None):
    if path.is_symlink() or not path.is_dir():
        return False
    try:
        resolved = path.resolve(strict=True)
        if home is not None and (resolved.parent != home / 'versions' or not VERSION_RE.fullmatch(resolved.name)):
            return False
        marker = json.loads((resolved / OWNED_MARKER).read_text())
        meta = metadata(resolved)
        files = file_hashes(resolved)
    except (OSError, json.JSONDecodeError, UpdateError):
        return False
    return (isinstance(marker, dict) and marker.get('schema') == 1 and
            marker.get('version') == meta['version'] == resolved.name and
            isinstance(marker.get('sha256'), str) and re.fullmatch(r'[0-9a-f]{64}', marker['sha256']) and
            marker.get('files') == files and (version is None or meta['version'] == version) and
            (digest is None or marker['sha256'] == digest))


def current_version(home, reject_invalid=False):
    home = Path(home)
    app = home / 'app'
    if not app.is_symlink():
        return None
    if not valid_owned(app.resolve(), home):
        if reject_invalid:
            raise UpdateError('installed app pointer is unowned, incomplete, or tampered')
        return None
    return metadata(app.resolve())['version']


def capture_pointer(home):
    app = home / 'app'
    if app.is_symlink():
        return ('symlink', os.readlink(app))
    if app.exists() or app.is_symlink():
        return ('legacy', None)
    return ('absent', None)


def activate(home, target):
    app = home / 'app'
    temporary = home / '.app-next'
    temporary.unlink(missing_ok=True)
    temporary.symlink_to(target)
    if app.exists() and not app.is_symlink():
        legacy = home / 'legacy-app'
        if legacy.exists() or legacy.is_symlink():
            temporary.unlink(missing_ok=True)
            raise UpdateError('legacy app is already preserved; manual migration required')
        try:
            os.replace(app, legacy)
            os.replace(temporary, app)
        except OSError:
            if not app.exists() and legacy.exists():
                os.replace(legacy, app)
            temporary.unlink(missing_ok=True)
            raise
    else:
        os.replace(temporary, app)


def restore_pointer(home, pointer):
    app = home / 'app'
    kind, value = pointer
    if kind == 'legacy':
        legacy = home / 'legacy-app'
        if legacy.is_dir() and not legacy.is_symlink():
            # A directory cannot atomically replace a symlink on Linux.
            # If interrupted after unlink, the durable journal retries this rename.
            if app.is_symlink():
                app.unlink()
            elif app.exists():
                raise UpdateError('rollback target conflicts with an existing app')
            os.replace(legacy, app)
        elif app.is_dir() and not app.is_symlink():
            # Activation never moved the legacy directory, or recovery already
            # restored it before an interruption. Both states are idempotent.
            return
        else:
            raise UpdateError('rollback target is unavailable')
    elif kind == 'symlink':
        temporary = home / '.app-restore'
        temporary.unlink(missing_ok=True)
        temporary.symlink_to(value)
        os.replace(temporary, app)
    else:
        app.unlink(missing_ok=True)


def snapshot_files(home):
    snapshot = {}
    for name in ('state.json', 'report.json'):
        path = home / name
        if path.is_symlink():
            raise UpdateError('state file is not a regular private file')
        snapshot[name] = (path.exists(), path.read_bytes() if path.exists() else b'')
    return snapshot


def restore_files(home, snapshot):
    for name, (existed, data) in snapshot.items():
        path = home / name
        if path.is_symlink():
            raise UpdateError('state file is not a regular private file')
        if existed:
            path.write_bytes(data)
        else:
            path.unlink(missing_ok=True)


def ensure_bootstrap(home):
    root = Path(home) / 'bootstrap'
    if (root / 'serverq.py').is_file():
        return
    temporary = Path(home) / '.bootstrap-next'
    shutil.rmtree(temporary, ignore_errors=True)
    for relative in PACKAGE_FILES:
        source = ROOT / relative
        if not source.is_file():
            raise UpdateError('bootstrap source is incomplete')
        target = temporary / relative
        target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        shutil.copyfile(source, target)
        os.chmod(target, 0o600)
    os.replace(temporary, root)
    directory = os.open(Path(home), os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def journal_snapshot(home):
    files = {}
    for name, (exists, data) in snapshot_files(home).items():
        if len(data) > TRANSACTION_MAX_BYTES:
            raise UpdateError('activation state is too large to journal safely')
        files[name] = {'exists': exists, 'data': base64.b64encode(data).decode('ascii')}
    return files


def restore_journal_files(home, files):
    if not isinstance(files, dict) or set(files) != {'state.json', 'report.json'}:
        raise UpdateError('activation journal is malformed')
    snapshot = {}
    total = 0
    for name in ('state.json', 'report.json'):
        value = files.get(name)
        if not isinstance(value, dict) or not isinstance(value.get('exists'), bool) or not isinstance(value.get('data'), str):
            raise UpdateError('activation journal is malformed')
        try:
            data = base64.b64decode(value['data'], validate=True)
        except ValueError as exc:
            raise UpdateError('activation journal is malformed') from exc
        total += len(data)
        if total > TRANSACTION_MAX_BYTES:
            raise UpdateError('activation journal is too large')
        snapshot[name] = (value['exists'], data)
    restore_files(home, snapshot)


def journal_pointer(pointer):
    kind, value = pointer
    return {'kind': kind, 'value': value}


def recover_transaction(home):
    home = Path(home)
    path = home / TRANSACTION_FILE
    if not path.exists():
        return False
    try:
        if path.stat().st_size > TRANSACTION_MAX_BYTES:
            raise UpdateError('activation journal is too large')
        journal = json.loads(path.read_text())
        prior = journal['prior_pointer']
        candidate = Path(journal['candidate'])
        if (journal.get('schema') != 1 or journal.get('phase') not in ('prepared', 'activated') or
                not isinstance(prior, dict) or prior.get('kind') not in ('symlink', 'legacy', 'absent') or
                not isinstance(prior.get('value'), (str, type(None))) or
                candidate.parent != home / 'versions' or not VERSION_RE.fullmatch(candidate.name)):
            raise UpdateError('activation journal is malformed')
        restore_pointer(home, (prior['kind'], prior['value']))
        restore_journal_files(home, journal['files'])
    except (OSError, json.JSONDecodeError, KeyError, TypeError) as exc:
        raise UpdateError('activation journal is malformed; refusing unsafe recovery') from exc
    path.unlink()
    directory = os.open(home, os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)
    return True


def begin_transaction(home, pointer, candidate):
    candidate = Path(candidate).resolve()
    if candidate.parent != Path(home) / 'versions' or not VERSION_RE.fullmatch(candidate.name) or not owned(candidate, candidate.name):
        raise UpdateError('activation candidate is not an owned version')
    journal = {'schema': 1, 'phase': 'prepared', 'prior_pointer': journal_pointer(pointer),
               'candidate': str(candidate), 'files': journal_snapshot(home)}
    save_json(Path(home) / TRANSACTION_FILE, journal)
    return journal


def set_transaction_phase(home, journal, phase):
    journal = dict(journal, phase=phase)
    save_json(Path(home) / TRANSACTION_FILE, journal)
    return journal


def commit_transaction(home):
    path = Path(home) / TRANSACTION_FILE
    path.unlink(missing_ok=True)
    directory = os.open(Path(home), os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def owned(path, version=None, digest=None):
    return valid_owned(path, version=version, digest=digest)


def mark_owned(path, version, digest):
    save_json(path / OWNED_MARKER, {'schema': 1, 'version': version, 'sha256': digest,
                                    'files': file_hashes(path)})


def prune_versions(home, active):
    versions = home / 'versions'
    known = []
    for path in versions.iterdir() if versions.exists() else ():
        if not VERSION_RE.fullmatch(path.name) or not owned(path, path.name):
            continue
        try:
            known.append((version_key(path.name), path))
        except UpdateError:
            continue
    for _, path in sorted(known, reverse=True)[KEEP_RELEASES:]:
        if path.resolve() != active.resolve():
            shutil.rmtree(path)


def prune_staging(home):
    staging = home / 'staging'
    if not staging.is_dir():
        return
    for path in staging.iterdir():
        if not path.is_symlink() and path.is_dir() and VERSION_RE.fullmatch(path.name.rsplit('-', 1)[0] if '-' in path.name else ''):
            try:
                marker = json.loads((path / STAGING_MARKER).read_text())
            except (OSError, json.JSONDecodeError):
                continue
            if marker == {'schema': 1, 'kind': 'staging'}:
                shutil.rmtree(path)


def postcheck(home, lock):
    app = home / 'app' / 'serverq.py'
    try:
        result = subprocess.run([sys.executable, str(app), 'check', '--home', str(home), '--lock-fd', str(lock.fileno()), '--in-transaction'],
                                capture_output=True, text=True, timeout=POSTCHECK_TIMEOUT, pass_fds=(lock.fileno(),))
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise UpdateError('post-activation check failed') from exc
    if result.returncode not in (0, 1):
        raise UpdateError('post-activation check failed')


def pinned_version(home):
    path = Path(home) / PIN_FILE
    if not path.exists():
        return None
    try:
        value = json.loads(path.read_text())
        pinned = value['version']
        if value != {'schema': 1, 'version': pinned}:
            raise ValueError
        version_key(pinned)
        return pinned
    except (OSError, json.JSONDecodeError, KeyError, TypeError, ValueError, UpdateError) as exc:
        raise UpdateError('rollback pin is corrupted; refusing network update') from exc


def set_pin(home, version):
    version_key(version)
    save_json(Path(home) / PIN_FILE, {'schema': 1, 'version': version})


def clear_pin(home):
    path = Path(home) / PIN_FILE
    path.unlink(missing_ok=True)
    directory = os.open(Path(home), os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)


def update(home, transport=fetch, unpin=False):
    home = Path(home)
    lock = with_lock(home)
    try:
        recover_transaction(home)
        ensure_bootstrap(home)
        if unpin:
            clear_pin(home)
        else:
            pin = pinned_version(home)
            if pin:
                outcome = {'outcome': 'pinned', 'version': current_version(home), 'pinned_version': pin, 'at': time.time()}
                save_json(home / 'update.json', outcome)
                return outcome
        prune_staging(home)
        try:
            release = json.loads(transport(RELEASE_API_URL, MAX_METADATA).decode('utf-8'))
            release_version, manifest_url, archive_url = release_assets(release)
            manifest = validate_manifest(json.loads(transport(manifest_url, MAX_METADATA).decode('utf-8')), release_version)
            local = metadata()
            installed = current_version(home, reject_invalid=True)
            if installed and version_key(manifest['version']) < version_key(installed):
                raise UpdateError('release downgrade rejected')
            if installed == manifest['version']:
                outcome = {'outcome': 'noop', 'version': installed, 'at': time.time()}
                save_json(home / 'update.json', outcome)
                return outcome
            if manifest['app_schema'] != local['app_schema'] or version_key(manifest['minimum_updater']) > version_key(local['version']):
                raise UpdateError('release is incompatible with this updater')
            load_state(home / 'state.json')
            data = transport(archive_url, MAX_ARCHIVE)
            staging = home / 'staging' / (manifest['version'] + '-' + str(os.getpid()))
            staging.mkdir(parents=True)
            save_json(staging / STAGING_MARKER, {'schema': 1, 'kind': 'staging'})
            destination = None
            activated = False
            pointer = None
            state_snapshot = None
            try:
                candidate = unpack_archive(data, manifest, staging)
                candidate_check(candidate, home)
                versions = home / 'versions'
                versions.mkdir(exist_ok=True)
                destination = versions / manifest['version']
                if destination.exists() or destination.is_symlink():
                    if owned(destination, manifest['version'], manifest['sha256']):
                        shutil.rmtree(destination)
                    else:
                        raise UpdateError('candidate version directory already exists')
                shutil.move(str(candidate), str(destination))
                mark_owned(destination, manifest['version'], manifest['sha256'])
                pointer = capture_pointer(home)
                journal = begin_transaction(home, pointer, destination)
                activate(home, destination)
                activated = True
                journal = set_transaction_phase(home, journal, 'activated')
                try:
                    postcheck(home, lock)
                except UpdateError:
                    recover_transaction(home)
                    activated = False
                    raise
                commit_transaction(home)
                prune_versions(home, destination)
                outcome = {'outcome': 'updated', 'version': manifest['version'], 'previous_version': installed, 'at': time.time()}
                save_json(home / 'update.json', outcome)
                return outcome
            except Exception:
                if (home / TRANSACTION_FILE).exists():
                    recover_transaction(home)
                if destination is not None and owned(destination, manifest['version'], manifest['sha256']) and current_version(home) != manifest['version']:
                    shutil.rmtree(destination)
                raise
            finally:
                shutil.rmtree(staging, ignore_errors=True)
        except (OSError, json.JSONDecodeError, UnicodeDecodeError, subprocess.TimeoutExpired, UpdateError, ValueError) as exc:
            failure = {'outcome': 'failed', 'at': time.time(), 'error': 'update rejected: ' + type(exc).__name__}
            save_json(home / 'update.json', failure)
            raise UpdateError(str(exc)) from exc
    finally:
        lock.close()


def rollback(home, version):
    version_key(version)
    home = Path(home)
    lock = with_lock(home)
    try:
        recover_transaction(home)
        target = home / 'versions' / version
        local = metadata()
        if (not owned(target, version) or metadata(target)['version'] != version or
                metadata(target)['app_schema'] != local['app_schema']):
            raise UpdateError('requested retained version is unavailable or incompatible')
        load_state(home / 'state.json')
        pointer = capture_pointer(home)
        journal = begin_transaction(home, pointer, target)
        activate(home, target)
        journal = set_transaction_phase(home, journal, 'activated')
        try:
            postcheck(home, lock)
        except UpdateError:
            recover_transaction(home)
            raise
        commit_transaction(home)
        set_pin(home, version)
        result = {'outcome': 'rolled-back', 'version': version, 'pinned_version': version, 'at': time.time()}
        save_json(home / 'update.json', result)
        return result
    finally:
        lock.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['check', 'update', 'rollback'])
    parser.add_argument('--home', type=Path, default=Path.home() / '.serverq')
    parser.add_argument('--filesystem', default='/')
    parser.add_argument('--force', action='store_true', help='Bypass configuration cache')
    parser.add_argument('--version')
    parser.add_argument('--unpin', action='store_true', help='Allow update after a deliberate rollback')
    parser.add_argument('--lock-fd', type=int, help=argparse.SUPPRESS)
    parser.add_argument('--in-transaction', action='store_true', help=argparse.SUPPRESS)
    args = parser.parse_args()
    if ROOT == args.home / 'bootstrap' and args.lock_fd is None:
        try:
            lock = with_lock(args.home)
            try:
                recover_transaction(args.home)
                version = current_version(args.home)
            finally:
                lock.close()
            target = args.home / 'app' / 'serverq.py'
            if version and target.is_file():
                return subprocess.run([sys.executable, str(target), *sys.argv[1:]], check=False).returncode
        except (OSError, ValueError, KeyError, TypeError, UpdateError) as exc:
            print(f'ServerQ {args.action} failed; no healthy result claimed: {exc}', file=sys.stderr)
            return 2
    try:
        if args.action == 'check':
            if args.lock_fd is None:
                lock = with_lock(args.home)
                try:
                    if not args.in_transaction:
                        recover_transaction(args.home)
                    report = audit(args.home, args.filesystem, args.force)
                finally:
                    lock.close()
            else:
                inherited_lock(args.home, args.lock_fd)
                if not args.in_transaction:
                    recover_transaction(args.home)
                report = audit(args.home, args.filesystem, args.force)
            print(json.dumps(report, indent=2))
            statuses = {value['status'] for value in report['results'].values()}
            return 2 if 'unknown' in statuses else 1 if 'warn' in statuses else 0
        if args.action == 'update':
            print(json.dumps(update(args.home, unpin=args.unpin), indent=2))
            return 0
        if not args.version:
            raise UpdateError('--version is required for rollback')
        print(json.dumps(rollback(args.home, args.version), indent=2))
        return 0
    except (OSError, ValueError, KeyError, TypeError, UpdateError) as exc:
        print(f'ServerQ {args.action} failed; no healthy result claimed: {exc}', file=sys.stderr)
        return 2


if __name__ == '__main__':
    sys.exit(main())
