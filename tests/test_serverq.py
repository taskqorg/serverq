import importlib.util
import json
import pathlib
import tempfile
import unittest
from unittest.mock import patch

ROOT = pathlib.Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('serverq', ROOT / 'serverq.py')
assert spec is not None and spec.loader is not None
sq = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sq)


class ServerQTests(unittest.TestCase):
    def test_limits(self):
        self.assertEqual(sq.capacity(100, 5, 10)['status'], 'warn')
        self.assertEqual(sq.capacity(100, 15, 10)['status'], 'ok')
        self.assertEqual(sq.capacity(0, 0, 10)['status'], 'unknown')

    def test_effective_journal_policy(self):
        self.assertEqual(sq.journal_policy('[Journal]\nSystemMaxUse=256M\nSystemKeepFree=2G\nRuntimeMaxUse=64M\n')['status'], 'ok')
        self.assertEqual(sq.journal_policy('[Journal]\nSystemMaxUse=256M\nSystemMaxUse=\nSystemKeepFree=2G\n')['status'], 'warn')
        self.assertEqual(sq.journal_policy('[Journal]\nSystemMaxUse=0\nSystemKeepFree=2G\n')['status'], 'warn')

    def test_journal_size_syntax(self):
        for value in ('1K', '1M', '1G', '1T', '1P', '1E', '1024'):
            with self.subTest(value=value):
                config = '[Journal]\n' + ''.join(f'{key}={value}\n' for key in ('SystemMaxUse', 'SystemKeepFree', 'RuntimeMaxUse'))
                self.assertEqual(sq.journal_policy(config)['status'], 'ok')
        for value in ('1KB', '1GB', '1EB', '0', '-1', 'unlimited'):
            with self.subTest(value=value):
                config = '[Journal]\n' + ''.join(f'{key}={value}\n' for key in ('SystemMaxUse', 'SystemKeepFree', 'RuntimeMaxUse'))
                self.assertEqual(sq.journal_policy(config)['status'], 'warn')

    def test_cache_invalidation_and_expiry(self):
        state = {}
        calls = []
        probe = lambda: calls.append(1) or {'status': 'ok'}
        sq.cached(state, 'x', 'a', 100, 10, probe)
        sq.cached(state, 'x', 'a', 105, 10, probe)
        self.assertEqual(len(calls), 1)
        sq.cached(state, 'x', 'b', 106, 10, probe)
        sq.cached(state, 'x', 'b', 117, 10, probe)
        sq.cached(state, 'x', 'b', 90, 10, probe)
        self.assertEqual(len(calls), 4)

    def test_unknown_not_cached(self):
        state = {}
        probe = lambda: {'status': 'unknown'}
        sq.cached(state, 'x', 'a', 100, 10, probe)
        self.assertNotIn('x', state)

    def test_atomic_state_and_corruption(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = pathlib.Path(tmp) / 'state.json'
            sq.save_json(path, {'schema': 1})
            self.assertEqual(sq.load_state(path)['schema'], 1)
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            path.write_text('broken')
            with self.assertRaises(ValueError):
                sq.load_state(path)

    def test_command_error_not_healthy(self):
        with patch.object(sq.subprocess, 'run', side_effect=FileNotFoundError):
            self.assertEqual(sq.failed_units()['status'], 'unknown')

    def test_docker_rotation_checks_existing_containers_on_local_daemon(self):
        ids = ['a' * 64, 'b' * 64]
        valid = {'Type': 'json-file', 'Config': {'max-size': '10m', 'max-file': '3'}}
        with patch.object(sq.shutil, 'which', return_value='/usr/bin/docker'), patch.object(sq, 'command') as probe:
            probe.side_effect = ['\n'.join(ids), '\n'.join(map(json.dumps, [valid, {'Type': 'json-file', 'Config': {}}]))]
            result = sq.docker_logs()
            self.assertEqual(result['status'], 'warn')
            self.assertEqual(result['missing_rotation'], [ids[1]])
            self.assertEqual(probe.call_args_list[0].args[0][:3], ['docker', '--host', 'unix:///var/run/docker.sock'])
            self.assertIn('--all', probe.call_args_list[0].args[0])
            self.assertEqual(probe.call_args_list[1].args[0][-2:], ids)

    def test_docker_rotation_bounds_and_supported_drivers(self):
        for driver, options, status in (
            ('json-file', {'max-size': '10m', 'max-file': '3'}, 'ok'),
            ('local', {'max-size': '1.5MB', 'max-file': '1'}, 'ok'),
            ('local', {}, 'warn'),
            ('json-file', {'max-size': '-1', 'max-file': '3'}, 'warn'),
            ('json-file', {'max-size': '0m', 'max-file': '3'}, 'warn'),
            ('json-file', {'max-size': '10m', 'max-file': '0'}, 'warn'),
            ('journald', {}, 'ok'), ('none', {}, 'ok'), ('fluentd', {}, 'unknown'),
        ):
            with self.subTest(driver=driver, options=options), patch.object(sq.shutil, 'which', return_value='docker'), patch.object(sq, 'command', side_effect=['a' * 64, json.dumps({'Type': driver, 'Config': options})]):
                self.assertEqual(sq.docker_logs()['status'], status)

    def test_docker_collection_fails_closed(self):
        cases = [RuntimeError('denied'), 'invalid-id', 'a' * 64 + '\n' + 'a' * 64]
        for output in cases:
            with self.subTest(output=output), patch.object(sq.shutil, 'which', return_value='docker'), patch.object(sq, 'command', side_effect=[output]):
                self.assertEqual(sq.docker_logs()['status'], 'unknown')
        for output in ('', '{}', 'null', '{broken', json.dumps({'Type': 'json-file', 'Config': None})):
            with self.subTest(output=output), patch.object(sq.shutil, 'which', return_value='docker'), patch.object(sq, 'command', side_effect=['a' * 64, output]):
                self.assertEqual(sq.docker_logs()['status'], 'unknown')
        with patch.object(sq.shutil, 'which', return_value='docker'), patch.object(sq, 'command', return_value=''):
            self.assertEqual(sq.docker_logs()['status'], 'ok')
        with patch.object(sq.shutil, 'which', return_value=None), patch.object(sq.Path, 'exists', return_value=False):
            self.assertEqual(sq.docker_logs()['status'], 'ok')
        with patch.object(sq.shutil, 'which', return_value=None), patch.object(sq.Path, 'exists', return_value=True):
            self.assertEqual(sq.docker_logs()['status'], 'unknown')

    def test_real_cli_isolated_state_and_failure(self):
        import subprocess
        import sys
        with tempfile.TemporaryDirectory() as tmp:
            argv = [sys.executable, str(ROOT / 'serverq.py'), 'check', '--home', tmp]
            result = subprocess.run(argv, capture_output=True, text=True, timeout=20)
            self.assertIn(result.returncode, (0, 1, 2))
            report = json.loads(result.stdout)
            self.assertEqual(set(report['results']), {'disk-headroom', 'inode-headroom', 'failed-services', 'bounded-journal', 'bounded-docker-logs'})
            again = subprocess.run(argv, capture_output=True, text=True, timeout=20)
            second = json.loads(again.stdout)
            if report['results']['bounded-journal']['status'] != 'unknown':
                self.assertTrue(second['results']['bounded-journal']['cached'])
            missing = subprocess.run(argv + ['--filesystem', str(pathlib.Path(tmp) / 'absent')], capture_output=True, text=True, timeout=20)
            self.assertEqual(missing.returncode, 2)
            self.assertEqual(json.loads(missing.stdout)['results']['disk-headroom']['status'], 'unknown')
            pathlib.Path(tmp, 'state.json').write_text('broken')
            broken = subprocess.run(argv, capture_output=True, text=True, timeout=20)
            self.assertEqual(broken.returncode, 2)
            self.assertEqual(broken.stdout, '')
            self.assertIn('no healthy result claimed', broken.stderr)

    def test_exclusive_lock(self):
        import fcntl
        import subprocess
        import sys
        with tempfile.TemporaryDirectory() as tmp:
            with pathlib.Path(tmp, 'lock').open('a') as lock:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                result = subprocess.run([sys.executable, str(ROOT / 'serverq.py'), 'check', '--home', tmp], capture_output=True, text=True, timeout=20)
                self.assertEqual(result.returncode, 2)
                self.assertFalse(pathlib.Path(tmp, 'state.json').exists())

    def test_incident_recovery_and_recurrence(self):
        state = {'incidents': {}}
        bad = {'disk-headroom': {'status': 'warn', 'detail': 'Low space'}}
        sq.incidents(state, bad, 100)
        sq.incidents(state, bad, 101)
        self.assertEqual(state['incidents']['disk-headroom']['occurrences'], 2)
        sq.incidents(state, {'disk-headroom': {'status': 'unknown'}}, 102)
        self.assertEqual(state['incidents']['disk-headroom']['state'], 'observed')
        sq.incidents(state, {'disk-headroom': {'status': 'ok'}}, 103)
        self.assertEqual(state['incidents']['disk-headroom']['state'], 'verified')
        sq.incidents(state, bad, 104)
        self.assertEqual(state['incidents']['disk-headroom']['first_seen'], 104)


if __name__ == '__main__':
    unittest.main()
