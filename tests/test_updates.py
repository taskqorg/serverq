import hashlib
import importlib.util
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
import zipfile
from unittest.mock import patch

ROOT = pathlib.Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('serverq_updates', ROOT / 'serverq.py')
sq = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sq)


@unittest.skipIf(os.environ.get('SERVERQ_PACKAGED_PREFLIGHT') == '1', 'nested updater lifecycle is not part of candidate preflight')
class UpdateTests(unittest.TestCase):
    def build_release(self, directory, version='0.2.1', mutate=None):
        directory.mkdir(parents=True, exist_ok=True)
        source = directory / ('source-' + version + '-' + str(len(list(directory.iterdir()))))
        shutil.copytree(ROOT, source, ignore=shutil.ignore_patterns('.git', '__pycache__'))
        metadata = json.loads((source / 'release.json').read_text())
        metadata['version'] = version
        (source / 'release.json').write_text(json.dumps(metadata) + '\n')
        if mutate:
            mutate(source)
        out = directory / 'release'
        result = subprocess.run([sys.executable, str(source / 'tools' / 'package_release.py'), '--output', str(out)], capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        manifest = next(out.glob('*.manifest.json'))
        archive = next(out.glob('*.zip'))
        return manifest, archive

    def transport_for(self, manifest, archive, release=None):
        manifest_json = json.loads(manifest.read_text())
        version = manifest_json['version']
        manifest_url = sq.expected_asset_url(version, manifest.name)
        archive_url = sq.expected_asset_url(version, archive.name)
        release = release or {'tag_name': 'v' + manifest_json['version'], 'prerelease': False, 'draft': False,
                              'assets': [{'name': manifest.name, 'browser_download_url': manifest_url},
                                         {'name': archive.name, 'browser_download_url': archive_url}]}
        payloads = {sq.RELEASE_API_URL: json.dumps(release).encode(),
                    manifest_url: manifest.read_bytes(), archive_url: archive.read_bytes()}
        return lambda url, limit: payloads[url]

    def test_success_noop_and_legacy_migration(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = pathlib.Path(tmp)
            home = tmp / 'home'
            legacy = home / 'app'
            legacy.mkdir(parents=True)
            (legacy / 'serverq.py').write_text('legacy preserved')
            manifest, archive = self.build_release(tmp)
            first = sq.update(home, transport=self.transport_for(manifest, archive))
            self.assertEqual(first['outcome'], 'updated')
            self.assertTrue((home / 'app').is_symlink())
            self.assertTrue((home / 'legacy-app').is_dir())
            self.assertEqual((home / 'legacy-app' / 'serverq.py').read_text(), 'legacy preserved')
            second = sq.update(home, transport=self.transport_for(manifest, archive))
            self.assertEqual(second['outcome'], 'noop')
            rollback = subprocess.run([sys.executable, str(home / 'app' / 'serverq.py'), 'rollback', '--home', str(home), '--version', '0.2.1'], capture_output=True, text=True, timeout=20)
            self.assertEqual(rollback.returncode, 0, rollback.stderr)
            self.assertEqual(json.loads((home / 'update.json').read_text())['outcome'], 'rolled-back')

    def test_rejects_offline_checksum_archive_and_incompatible_versions(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = pathlib.Path(tmp)
            manifest, archive = self.build_release(tmp)
            home = tmp / 'home'
            for transport, phrase in ((lambda url, limit: (_ for _ in ()).throw(OSError('offline')), 'offline'),):
                with self.assertRaises(sq.UpdateError) as exc:
                    sq.update(home, transport=transport)
                self.assertIn(phrase, str(exc.exception))
            bad = json.loads(manifest.read_text())
            bad['sha256'] = '0' * 64
            manifest.write_text(json.dumps(bad))
            with self.assertRaises(sq.UpdateError):
                sq.update(home, transport=self.transport_for(manifest, archive))
            manifest, archive = self.build_release(tmp, version='0.2.1')
            release = {'tag_name': 'v0.2.1', 'prerelease': True, 'draft': False, 'assets': []}
            with self.assertRaises(sq.UpdateError):
                sq.update(home, transport=lambda url, limit: json.dumps(release).encode())
            sq.update(home, transport=self.transport_for(manifest, archive))
            lower, lower_archive = self.build_release(tmp / 'lower', version='0.1.9')
            with self.assertRaises(sq.UpdateError):
                sq.update(home, transport=self.transport_for(lower, lower_archive))
            corrupt = tmp / 'corrupt.zip'
            corrupt.write_bytes(b'not a zip')
            corrupt_manifest = json.loads(manifest.read_text())
            corrupt_manifest.update(version='0.2.2', archive=corrupt.name, sha256=hashlib.sha256(corrupt.read_bytes()).hexdigest(), size=corrupt.stat().st_size)
            corrupt_meta = tmp / 'corrupt.manifest.json'
            corrupt_meta.write_text(json.dumps(corrupt_manifest))
            with self.assertRaises(sq.UpdateError):
                sq.update(home, transport=self.transport_for(corrupt_meta, corrupt))

    def test_archive_safety_preflight_rollback_retention_and_lock(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = pathlib.Path(tmp)
            manifest, archive = self.build_release(tmp, version='0.2.1')
            home = tmp / 'home'
            sq.update(home, transport=self.transport_for(manifest, archive))
            def break_check(source):
                path = source / 'serverq.py'
                path.write_text(path.read_text().replace("return 2 if 'unknown' in statuses else 1 if 'warn' in statuses else 0", 'return 2'))
            broken_manifest, broken_archive = self.build_release(tmp / 'broken', version='0.2.2', mutate=break_check)
            with self.assertRaises(sq.UpdateError):
                sq.update(home, transport=self.transport_for(broken_manifest, broken_archive))
            self.assertEqual(sq.current_version(home), '0.2.1')
            unsafe = tmp / 'unsafe.zip'
            with zipfile.ZipFile(unsafe, 'w') as bundle:
                bundle.writestr('../escape', 'no')
            data = json.loads(manifest.read_text())
            data.update(version='0.2.3', archive=unsafe.name, sha256=hashlib.sha256(unsafe.read_bytes()).hexdigest())
            unsafe_manifest = tmp / 'unsafe.manifest.json'
            unsafe_manifest.write_text(json.dumps(data))
            with self.assertRaises(sq.UpdateError):
                sq.update(home, transport=self.transport_for(unsafe_manifest, unsafe))
            lock = home / 'lock'
            import fcntl
            with lock.open('a') as stream:
                fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
                with self.assertRaises(sq.UpdateError):
                    sq.update(home, transport=self.transport_for(manifest, archive))

    def test_retention_preserves_unknown_directories(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = pathlib.Path(tmp)
            versions = home / 'versions'
            for version in ('0.1.0', '0.1.1', '0.1.2', '0.1.3'):
                target = versions / version
                shutil.copytree(ROOT, target, ignore=shutil.ignore_patterns('.git', '__pycache__'))
                target.joinpath('release.json').write_text(json.dumps({'schema': 1, 'version': version, 'app_schema': 1, 'minimum_updater': '0.2.0'}))
                sq.mark_owned(target, version, '0' * 64)
            unknown = versions / 'operator-data'
            unknown.mkdir()
            sq.prune_versions(home, versions / '0.1.3')
            self.assertEqual(sorted(path.name for path in versions.iterdir() if path.name != 'operator-data'), ['0.1.1', '0.1.2', '0.1.3'])
            self.assertTrue(unknown.is_dir())
            known_staging = home / 'staging' / '0.2.0-123'
            known_staging.mkdir(parents=True)
            known_staging.joinpath(sq.STAGING_MARKER).write_text(json.dumps({'schema': 1, 'kind': 'staging'}))
            operator_staging = home / 'staging' / 'operator-data'
            operator_staging.mkdir()
            sq.prune_staging(home)
            self.assertFalse(known_staging.exists())
            self.assertTrue(operator_staging.is_dir())

    def test_cli_rollback_and_packaged_contents(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = pathlib.Path(tmp)
            manifest, archive = self.build_release(tmp, version='0.2.1')
            with zipfile.ZipFile(archive) as bundle:
                names = bundle.namelist()
            self.assertTrue(any(name.endswith('/tests/test_serverq.py') for name in names))
            self.assertTrue(any(name.endswith('/README.md') for name in names))
            self.assertTrue(any(name.endswith('/rules.json') for name in names))
            self.assertEqual(json.loads(manifest.read_text())['version'], '0.2.1')


    def test_post_activation_failure_restores_legacy_and_state(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = pathlib.Path(tmp)
            home = tmp / 'home'
            legacy = home / 'app'
            legacy.mkdir(parents=True)
            (legacy / 'serverq.py').write_text('legacy')
            state = home / 'state.json'
            report = home / 'report.json'
            state.write_bytes(b'{"schema": 1, "cache": {}, "incidents": {}}\n')
            report.write_bytes(b'{"old": true}\n')

            def fail_after_audit(source):
                path = source / 'serverq.py'
                path.write_text(path.read_text().replace(
                    'report = audit(args.home, args.filesystem, args.force)',
                    "report = audit(args.home, args.filesystem, args.force)\n                if (args.home / 'post-fail').exists():\n                    return 2"))

            manifest, archive = self.build_release(tmp, mutate=fail_after_audit)
            (home / 'post-fail').write_text('x')
            with self.assertRaises(sq.UpdateError):
                sq.update(home, transport=self.transport_for(manifest, archive))
            self.assertTrue((home / 'app').is_dir())
            self.assertFalse((home / 'app').is_symlink())
            self.assertEqual((home / 'app' / 'serverq.py').read_text(), 'legacy')
            self.assertEqual(state.read_bytes(), b'{"schema": 1, "cache": {}, "incidents": {}}\n')
            self.assertEqual(report.read_bytes(), b'{"old": true}\n')

    def test_release_protocol_and_retention_are_owned_only(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = pathlib.Path(tmp)
            manifest, archive = self.build_release(tmp)
            release = {'tag_name': 'v0.2.1', 'draft': False, 'prerelease': False,
                       'assets': [
                           {'name': archive.name, 'browser_download_url': 'https://evil.example/a.zip'},
                           {'name': manifest.name, 'browser_download_url': 'https://github.com/taskqorg/serverq/releases/download/v0.2.1/' + manifest.name},
                       ]}
            with self.assertRaises(sq.UpdateError):
                sq.release_assets(release)
            versions = tmp / 'versions'
            unknown = versions / '9.9.9'
            unknown.mkdir(parents=True)
            (unknown / 'release.json').write_text(json.dumps({'schema': 1, 'version': '9.9.9', 'app_schema': 1, 'minimum_updater': '0.2.0'}))
            for version in ('0.1.0', '0.1.1', '0.1.2', '0.1.3'):
                candidate = versions / version
                candidate.mkdir()
                candidate.joinpath('release.json').write_text(json.dumps({'schema': 1, 'version': version, 'app_schema': 1, 'minimum_updater': '0.2.0'}))
            sq.prune_versions(tmp, versions / '0.1.3')
            self.assertTrue(unknown.exists())
            self.assertTrue((versions / '0.1.0').exists())

    def test_rollback_rejects_path_before_filesystem_and_pins(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = pathlib.Path(tmp)
            with self.assertRaises(sq.UpdateError):
                sq.rollback(tmp, '../../outside')
            manifest, archive = self.build_release(tmp)
            home = tmp / 'home'
            sq.update(home, transport=self.transport_for(manifest, archive))
            result = sq.rollback(home, '0.2.1')
            self.assertEqual(result['pinned_version'], '0.2.1')
            self.assertEqual(sq.update(home, transport=self.transport_for(manifest, archive))['outcome'], 'pinned')


    def test_timeout_rolls_back_and_fixed_release_retries(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = pathlib.Path(tmp)
            home = tmp / 'home'
            first_manifest, first_archive = self.build_release(tmp, version='0.2.1')
            sq.update(home, transport=self.transport_for(first_manifest, first_archive))
            manifest, archive = self.build_release(tmp / 'next', version='0.2.2')
            with patch.object(sq, 'POSTCHECK_TIMEOUT', 0.01):
                with self.assertRaises(sq.UpdateError):
                    sq.update(home, transport=self.transport_for(manifest, archive))
            self.assertEqual(sq.current_version(home), '0.2.1')
            self.assertFalse((home / 'versions' / '0.2.2').exists())
            self.assertEqual(sq.update(home, transport=self.transport_for(manifest, archive))['outcome'], 'updated')
            self.assertEqual(sq.current_version(home), '0.2.2')

    def test_malformed_state_blocks_before_legacy_migration(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = pathlib.Path(tmp)
            home = tmp / 'home'
            legacy = home / 'app'
            legacy.mkdir(parents=True)
            legacy.joinpath('serverq.py').write_text('legacy')
            home.joinpath('state.json').write_bytes(b'broken state')
            manifest, archive = self.build_release(tmp)
            with self.assertRaises(sq.UpdateError):
                sq.update(home, transport=self.transport_for(manifest, archive))
            self.assertTrue(legacy.is_dir())
            self.assertFalse((home / 'legacy-app').exists())
            self.assertEqual(home.joinpath('state.json').read_bytes(), b'broken state')

    def test_rollback_pin_is_honored_before_offline_fetch_and_corruption_fails_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = pathlib.Path(tmp)
            manifest, archive = self.build_release(tmp)
            home = tmp / 'home'
            sq.update(home, transport=self.transport_for(manifest, archive))
            sq.rollback(home, '0.2.1')
            calls = []
            def offline(url, limit):
                calls.append(url)
                raise OSError('offline')
            self.assertEqual(sq.update(home, transport=offline)['outcome'], 'pinned')
            self.assertEqual(calls, [])
            (home / 'pin.json').write_text('{broken')
            with self.assertRaisesRegex(sq.UpdateError, 'pin'):
                sq.update(home, transport=offline)
            self.assertEqual(calls, [])

    def test_noop_rejects_external_or_tampered_app_pointer(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = pathlib.Path(tmp)
            manifest, archive = self.build_release(tmp)
            home = tmp / 'home'
            transport = self.transport_for(manifest, archive)
            sq.update(home, transport=transport)
            external = tmp / 'external'
            shutil.copytree(home / 'versions' / '0.2.1', external)
            (home / 'app').unlink()
            (home / 'app').symlink_to(external)
            with self.assertRaisesRegex(sq.UpdateError, 'installed'):
                sq.update(home, transport=transport)
            (home / 'app').unlink()
            (home / 'app').symlink_to(home / 'versions' / '0.2.1')
            with (home / 'versions' / '0.2.1' / 'serverq.py').open('a') as stream:
                stream.write('# tampered\n')
            with self.assertRaisesRegex(sq.UpdateError, 'installed'):
                sq.update(home, transport=transport)

    def test_recovery_restores_interrupted_activation_before_check(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = pathlib.Path(tmp)
            manifest, archive = self.build_release(tmp)
            home = tmp / 'home'
            sq.update(home, transport=self.transport_for(manifest, archive))
            old = home / 'versions' / '0.2.1'
            candidate = home / 'versions' / '0.2.2'
            shutil.copytree(old, candidate)
            candidate.joinpath('release.json').write_text(json.dumps({'schema': 1, 'version': '0.2.2', 'app_schema': 1, 'minimum_updater': '0.2.0'}))
            sq.mark_owned(candidate, '0.2.2', '0' * 64)
            (home / 'app').unlink()
            (home / 'app').symlink_to(candidate)
            (home / 'state.json').write_bytes(b'{"schema": 1, "cache": {}, "incidents": {}}\n')
            sq.save_json(home / 'transaction.json', {
                'schema': 1, 'phase': 'activated', 'prior_pointer': {'kind': 'symlink', 'value': str(old)},
                'candidate': str(candidate), 'files': {'state.json': {'exists': False, 'data': ''}, 'report.json': {'exists': False, 'data': ''}},
            })
            lock = sq.with_lock(home)
            try:
                sq.recover_transaction(home)
            finally:
                lock.close()
            self.assertEqual(sq.current_version(home), '0.2.1')
            self.assertFalse((home / 'state.json').exists())
            self.assertFalse((home / 'transaction.json').exists())
    def test_legacy_recovery_all_pointer_boundaries_and_repeat(self):
        for phase in ('prepared', 'activated', 'restored'):
            with self.subTest(phase=phase), tempfile.TemporaryDirectory() as tmp:
                home = pathlib.Path(tmp)
                candidate = home / 'versions' / '0.2.1'
                candidate.mkdir(parents=True)
                app = home / 'app'
                app.mkdir()
                (app / 'original').write_text('keep')
                sq.save_json(home / 'transaction.json', {
                    'schema': 1, 'phase': 'prepared',
                    'prior_pointer': {'kind': 'legacy', 'value': None},
                    'candidate': str(candidate),
                    'files': {'state.json': {'exists': False, 'data': ''},
                              'report.json': {'exists': False, 'data': ''}},
                })
                if phase == 'activated':
                    sq.activate(home, candidate)
                if phase == 'restored':
                    app.rename(home / 'legacy-app')
                    (home / 'legacy-app').rename(app)
                # Simulate death after pointer restoration, before journal removal.
                with patch.object(sq, 'restore_journal_files', side_effect=OSError('interrupted')):
                    with self.assertRaises(sq.UpdateError):
                        sq.recover_transaction(home)
                self.assertTrue((home / 'transaction.json').exists())
                sq.recover_transaction(home)
                self.assertFalse(app.is_symlink())
                self.assertEqual((app / 'original').read_text(), 'keep')
                self.assertFalse(sq.recover_transaction(home))

    def test_archive_rejects_duplicate_paths(self):
        import warnings
        with tempfile.TemporaryDirectory() as tmp:
            staging = pathlib.Path(tmp)
            archive = staging / 'duplicate.zip'
            with warnings.catch_warnings():
                warnings.simplefilter('ignore', UserWarning)
                with zipfile.ZipFile(archive, 'w') as bundle:
                    bundle.writestr('serverq-0.2.1/serverq.py', 'first')
                    bundle.writestr('serverq-0.2.1/serverq.py', 'second')
            data = archive.read_bytes()
            manifest = {'version': '0.2.1', 'sha256': hashlib.sha256(data).hexdigest(), 'size': len(data)}
            with self.assertRaisesRegex(sq.UpdateError, 'duplicate'):
                sq.unpack_archive(data, manifest, staging)
            self.assertFalse((staging / 'unpack').exists())

    def test_checker_report_contract(self):
        result = sq.capacity(100, 5, 10)
        self.assertEqual(result['available'], 5)
        self.assertEqual(result['total'], 100)
        with patch.object(sq, 'command', return_value='broken.service loaded failed failed\n'):
            self.assertEqual(sq.failed_units()['units'], ['broken.service'])

    def test_sigterm_after_activation_recovers_on_bootstrap_check(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = pathlib.Path(tmp)
            home = tmp / 'home'
            first_manifest, first_archive = self.build_release(tmp, version='0.2.1')
            sq.update(home, transport=self.transport_for(first_manifest, first_archive))
            manifest, archive = self.build_release(tmp / 'next', version='0.2.2')
            script = """
import importlib.util, json, pathlib, sys, time
root = pathlib.Path(sys.argv[1]); manifest = pathlib.Path(sys.argv[2]); archive = pathlib.Path(sys.argv[3]); home = pathlib.Path(sys.argv[4])
spec = importlib.util.spec_from_file_location('sq', root / 'serverq.py'); sq = importlib.util.module_from_spec(spec); spec.loader.exec_module(sq)
m = json.loads(manifest.read_text()); release = {'tag_name': 'v' + m['version'], 'draft': False, 'prerelease': False, 'assets': [{'name': archive.name, 'browser_download_url': sq.expected_asset_url(m['version'], archive.name)}, {'name': manifest.name, 'browser_download_url': sq.expected_asset_url(m['version'], manifest.name)}]}
def transport(url, limit):
    if url == sq.RELEASE_API_URL: return json.dumps(release).encode()
    if url.endswith(manifest.name): return manifest.read_bytes()
    if url.endswith(archive.name): return archive.read_bytes()
    raise AssertionError(url)
sq.postcheck = lambda home, lock: time.sleep(30)
sq.update(home, transport=transport)
"""
            process = subprocess.Popen([sys.executable, '-c', script, str(ROOT), str(manifest), str(archive), str(home)])
            deadline = time.time() + 10
            while time.time() < deadline and sq.current_version(home) != '0.2.2':
                time.sleep(0.02)
            self.assertEqual(sq.current_version(home), '0.2.2')
            process.terminate()
            process.wait(timeout=5)
            self.assertTrue((home / 'transaction.json').exists())
            subprocess.run([sys.executable, str(home / 'bootstrap' / 'serverq.py'), 'check', '--home', str(home)],
                           capture_output=True, text=True, timeout=20)
            self.assertEqual(sq.current_version(home), '0.2.1')
            self.assertFalse((home / 'transaction.json').exists())


if __name__ == '__main__':
    unittest.main()
