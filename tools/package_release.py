#!/usr/bin/env python3
"""Build reproducible ServerQ release assets using only the standard library."""
import argparse
import hashlib
import json
from pathlib import Path
import zipfile

ROOT = Path(__file__).resolve().parents[1]
INCLUDE = ('serverq.py', 'rules.json', 'release.json', 'README.md', 'AGENTS.md', 'tests/test_serverq.py', 'tests/test_updates.py', 'systemd/serverq-check.service', 'systemd/serverq-check.timer', 'systemd/serverq-update.service', 'systemd/serverq-update.timer')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    metadata = json.loads((ROOT / 'release.json').read_text())
    version = metadata['version']
    if not isinstance(version, str) or not version:
        raise SystemExit('release.json has no valid version')
    args.output.mkdir(parents=True, exist_ok=True)
    archive = args.output / ('serverq-' + version + '.zip')
    prefix = 'serverq-' + version + '/'
    with zipfile.ZipFile(archive, 'w', compression=zipfile.ZIP_DEFLATED, compresslevel=9) as bundle:
        for relative in INCLUDE:
            data = (ROOT / relative).read_bytes()
            info = zipfile.ZipInfo(prefix + relative, (1980, 1, 1, 0, 0, 0))
            info.external_attr = 0o600 << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            bundle.writestr(info, data)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    manifest = {'schema': 1, 'version': version, 'app_schema': metadata['app_schema'],
                'minimum_updater': metadata['minimum_updater'], 'archive': archive.name,
                'sha256': digest, 'size': archive.stat().st_size}
    output = args.output / ('serverq-' + version + '.manifest.json')
    output.write_text(json.dumps(manifest, indent=2, sort_keys=True) + '\n')
    print(json.dumps({'archive': str(archive), 'manifest': str(output), 'sha256': digest}, sort_keys=True))


if __name__ == '__main__':
    main()
