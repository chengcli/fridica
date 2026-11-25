#!/usr/bin/env python3
"""Package installed portable JS and exact corresponding AGPL source; no credentials."""
import argparse
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tarfile

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--upstream', type=Path, required=True)
p.add_argument('--output', type=Path, required=True)
args = p.parse_args()
root = Path(__file__).resolve().parent
lock = json.loads((root / 'package-lock.json').read_text())
pin = lock['packages']['node_modules/@netique/overleaf-mcp']
revision = pin['resolved'].split('#')[1]
assert subprocess.check_output(['git', '-C', str(args.upstream), 'rev-parse', 'HEAD'], text=True).strip() == revision
assert not subprocess.check_output(['git', '-C', str(args.upstream), 'status', '--porcelain'])
assert not list((root / 'node_modules').rglob('*.node')), 'native dependency is not portable'
source = subprocess.check_output(['git', '-C', str(args.upstream), 'archive', '--format=tar.gz', '--prefix=overleaf-mcp/', revision])
files = ['owner-login.mjs', 'owner-login.test.mjs', 'adapter.py', 'test_adapter.py', 'package.json', 'package-lock.json', 'README.md']
with tarfile.open(args.output, 'w:gz') as archive:
    for name in files + ['node_modules']:
        archive.add(root / name, arcname='overleaf-native/' + name)
    archive.add(root.parents[1] / 'LICENSE', arcname='overleaf-native/FRIDICA-LICENSE')
    item = tarfile.TarInfo('overleaf-native/upstream-source.tar.gz')
    item.mode, item.size = 0o600, len(source)
    archive.addfile(item, io.BytesIO(source))
    manifest = json.dumps({'package': '@netique/overleaf-mcp', **pin,
                           'fridica_revision': subprocess.check_output(['git', '-C', str(root), 'rev-parse', 'HEAD'], text=True).strip(),
                           'corresponding_source_sha256': hashlib.sha256(source).hexdigest()}, indent=2).encode()
    item = tarfile.TarInfo('overleaf-native/upstream-pin.json')
    item.mode, item.size = 0o600, len(manifest)
    archive.addfile(item, io.BytesIO(manifest))
args.output.chmod(0o600)
print(hashlib.sha256(args.output.read_bytes()).hexdigest(), args.output)
