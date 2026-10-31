#!/usr/bin/env python3
"""从本仓库锁文件及已校验的本地 crate 归档创建隔离测试 vendor；不联网、不执行 Cargo。"""
import argparse
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import tarfile
import tomllib

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--out', required=True, type=Path)
parser.add_argument('--cache', type=Path, default=Path.home() / '.cargo/registry/cache')
parser.add_argument('--crate', default='serde_json')
options = parser.parse_args()
root = Path(__file__).resolve().parents[1]
packages = tomllib.loads((root / 'Cargo.lock').read_text())['package']
selected = {}


def choose(dependency):
    parts = dependency.split()
    matches = [p for p in packages if p['name'] == parts[0] and (len(parts) == 1 or p['version'] == parts[1])]
    if len(matches) != 1:
        raise ValueError('锁文件依赖不唯一，必须明确版本')
    package = matches[0]
    identity = package['name'], package['version']
    if identity in selected:
        return
    if not package.get('source', '').startswith('registry+') or not package.get('checksum'):
        raise ValueError('测试只接受锁文件有摘要的 registry 包')
    selected[identity] = package
    for child in package.get('dependencies', []):
        choose(child)


choose(options.crate)
options.out.mkdir(parents=True, exist_ok=False)
files, size = {}, 0
for (name, version), package in sorted(selected.items()):
    basename = f'{name}-{version}'
    matches = list(options.cache.glob(f'*/{basename}.crate'))
    if len(matches) != 1 or matches[0].stat().st_size > 32 * 1024**2:
        raise ValueError('本地 crate 归档缺失、不唯一或超过上限')
    with matches[0].open('rb') as source:
        data = source.read(32 * 1024**2 + 1)
    if len(data) > 32 * 1024**2 or hashlib.sha256(data).hexdigest() != package['checksum']:
        raise ValueError('crate 归档与锁文件摘要不符')
    hashes = {}
    with tarfile.open(fileobj=io.BytesIO(data), mode='r|gz') as archive:
        for index, member in enumerate(archive):
            path = PurePosixPath(member.name)
            if index >= 8192 or len(path.parts) > 16 or any(c in member.name for c in ['\\', ':', '\0']):
                raise ValueError('crate 成员路径、深度或数量不支持')
            if path.is_absolute() or '..' in path.parts or not path.parts or path.parts[0] != basename:
                raise ValueError('crate 路径非法')
            if member.isdir():
                continue
            if not member.isfile() or not (0 <= member.size <= 8 * 1024**2) or len(files) >= 4096:
                raise ValueError('crate 成员类型、大小或数量不支持')
            relative = '/'.join(path.parts[1:])
            if not relative or relative in hashes:
                raise ValueError('crate 文件名为空或重复')
            source = archive.extractfile(member)
            content = source.read(8 * 1024**2 + 1)
            if len(content) != member.size:
                raise ValueError('crate 成员长度不符')
            size += len(content)
            if size > 64 * 1024**2:
                raise ValueError('vendor 总量超过64MiB')
            target = options.out.joinpath(*path.parts)
            target.parent.mkdir(parents=True, exist_ok=True)
            with target.open('xb') as output:
                output.write(content)
            target.chmod(0o644)
            hashes[relative] = hashlib.sha256(content).hexdigest()
            files[str(path)] = hashes[relative]
    checksum = json.dumps({'files': hashes, 'package': package['checksum']}, sort_keys=True, separators=(',', ':')).encode()
    target = options.out / basename / '.cargo-checksum.json'
    with target.open('xb') as output:
        output.write(checksum)
    target.chmod(0o644)
    files[f'{basename}/.cargo-checksum.json'] = hashlib.sha256(checksum).hexdigest()
for path in [options.out, *(p for p in options.out.rglob('*') if p.is_dir())]:
    path.chmod(0o755)
manifest = json.dumps(files, ensure_ascii=False, sort_keys=True, separators=(',', ':')).encode()
print(json.dumps({'bundle_sha256': hashlib.sha256(b'eve.cargo.vendor.v1\0' + manifest).hexdigest(),
    'crates': [f'{name}-{version}' for name, version in sorted(selected)], 'files': len(files), 'bytes': size}, ensure_ascii=False))
