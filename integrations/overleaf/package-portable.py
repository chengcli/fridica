#!/usr/bin/env python3
"""Package installed portable JS and exact corresponding AGPL source; no credentials."""
import argparse
import hashlib
import io
import json
import shutil
from pathlib import Path
import subprocess
import tarfile
import tempfile

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--upstream', type=Path, help='clean checkout at the locked revision; otherwise fetch it')
output = p.add_mutually_exclusive_group(required=True)
output.add_argument('--output', type=Path, help='portable tar.gz destination')
output.add_argument('--wheel-data', action='store_true', help='stage standard Maturin fridica.data resources')
args = p.parse_args()
root = Path(__file__).resolve().parent
lock = json.loads((root / 'package-lock.json').read_text())
pin = lock['packages']['node_modules/@netique/overleaf-mcp']
repository, revision = pin['resolved'].removeprefix('git+').split('#')
# Source archives are public; npm's SSH alias must not require SSH credentials.
repository = repository.replace('ssh://git@github.com/', 'https://github.com/', 1)
# npm may normalize the same Git HTTPS source to SSH in its installation receipt.
installed = json.loads((root / 'node_modules/.package-lock.json').read_text())['packages']['node_modules/@netique/overleaf-mcp']
assert installed['resolved'].split('#')[1] == revision
subprocess.run(['node', '--input-type=module', '-e',
                'import { checkResources } from "./owner-login.mjs"; await checkResources();'],
               cwd=root, check=True)
assert (root / 'node_modules/@netique/overleaf-mcp/dist/auth/sessionFd.js').is_file()
# All scratch/staging stays under the checkout and out of source control.
scratch = root.parents[1] / 'target'
scratch.mkdir(exist_ok=True)
workspace = tempfile.TemporaryDirectory(prefix='overleaf-package-', dir=scratch)
if args.upstream is None:
    args.upstream = Path(workspace.name) / 'upstream'
    subprocess.run(['git', 'init', '--quiet', str(args.upstream)], check=True)
    subprocess.run(['git', '-C', str(args.upstream), 'fetch', '--quiet', '--depth=1', repository, revision], check=True)
    subprocess.run(['git', '-C', str(args.upstream), 'checkout', '--quiet', '--detach', 'FETCH_HEAD'], check=True)
assert subprocess.check_output(['git', '-C', str(args.upstream), 'rev-parse', 'HEAD'], text=True).strip() == revision
assert not subprocess.check_output(['git', '-C', str(args.upstream), 'status', '--porcelain'])
assert not list((root / 'node_modules').rglob('*.node')), 'native dependency is not portable'
source = subprocess.check_output(['git', '-C', str(args.upstream), 'archive', '--format=tar.gz', '--prefix=overleaf-mcp/', revision])
files = ['owner-login.mjs', 'owner-login.test.mjs', 'adapter.py', 'test_adapter.py', 'package.json', 'package-lock.json', 'README.md', 'UPSTREAM-NOTICE']
destination = args.output or Path(workspace.name) / 'portable.tar.gz'
with tarfile.open(destination, 'w:gz') as archive:
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
if args.wheel_data:
    data = root.parents[1] / 'fridica.data/data/share/fridica'
    staged = data / 'overleaf-native'
    if staged.exists():
        shutil.rmtree(staged)
    data.mkdir(parents=True, exist_ok=True)
    with tarfile.open(destination) as archive:
        archive.extractall(data, filter='data')
    print(f'Staged {pin["version"]} at {revision} in {staged}')
else:
    destination.chmod(0o600)
    print(hashlib.sha256(destination.read_bytes()).hexdigest(), destination)
workspace.cleanup()
