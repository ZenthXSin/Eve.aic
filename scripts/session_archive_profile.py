#!/usr/bin/env python3
"""Linux 冷归档幂等写入独立进程峰值对照；只用合成文件，无模型、网络或真实会话。"""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--deps', required=True, type=Path, help='已构建的 debug/deps；请勿并行编译')
parser.add_argument('--old-ref', default='4a54924')
parser.add_argument('--rustc', default=shutil.which('rustc'))
options = parser.parse_args()
if not sys.platform.startswith('linux') or not options.rustc:
    parser.error('需要 Linux 和 rustc')
root = Path(__file__).resolve().parents[1]
old = subprocess.check_output(['git', 'show', f'{options.old_ref}:crates/session-plugin/src/archive.rs'], cwd=root, text=True)
new = (root / 'crates/session-plugin/src/archive.rs').read_text()
deps = options.deps.resolve()
libraries = []
for name in ['eve_session_api', 'ring', 'tempfile']:
    candidates = list(deps.glob(f'lib{name}-*.rlib'))
    if not candidates:
        parser.error(f'请先构建依赖：缺少 {name}')
    libraries += ['--extern', f'{name}={max(candidates, key=lambda p:p.stat().st_mtime_ns)}']
source = r'''
extern crate eve_session_api;
extern crate ring;
extern crate tempfile;
mod archive { SOURCE }
use eve_session_api::{SessionArchiveStore,MAX_SESSION_ARCHIVE_RECORD_BYTES};
fn main() {
    let directory=tempfile::tempdir().unwrap();
    let archive=archive::FileSessionArchive::open(directory.path()).unwrap();
    let mut body=vec![b'x';MAX_SESSION_ARCHIVE_RECORD_BYTES];
    body[0]=b'"';let last=body.len()-1;body[last]=b'"';
    let hash:String=ring::digest::digest(&ring::digest::SHA256,&body).as_ref().iter().map(|b|format!("{b:02x}")).collect();
    archive.put(&hash,&body).unwrap();
    for _ in 0..3 {archive.put(&hash,&body).unwrap();}
    assert_eq!(std::fs::metadata(directory.path().join(format!("{hash}.json"))).unwrap().len(),body.len() as u64);
    println!("bytes={} sha256={hash} repeated_puts=3",body.len());
}
'''
wrapper = ('import subprocess,resource,time,sys\nstart=time.monotonic()\n'
           'subprocess.run([sys.argv[1]],check=True)\n'
           'print(f"max_rss_kib={resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss} '
           'elapsed_seconds={time.monotonic()-start:.3f}",file=sys.stderr)')
results = {}
with tempfile.TemporaryDirectory(prefix='eve-session-archive-profile-') as directory:
    for name, module in [('old', old), ('new', new)]:
        file = Path(directory) / f'{name}.rs'
        binary = Path(directory) / name
        module = '\n'.join(line for line in module.splitlines() if not line.startswith('//!'))
        file.write_text(source.replace('SOURCE', module), encoding='utf-8')
        subprocess.run([options.rustc, '--edition=2024', '--crate-name', f'archive_{name}',
            '-L', f'dependency={deps}', *libraries, '-C', 'debuginfo=0', str(file), '-o', str(binary)], check=True)
        result = subprocess.run([sys.executable, '-c', wrapper, str(binary)], capture_output=True, text=True, check=True)
        results[name] = {'manifest': result.stdout.strip(), 'measurement': result.stderr.strip()}
if results['old']['manifest'] != results['new']['manifest']:
    raise RuntimeError('同样本输出不一致')
print(json.dumps({'old_ref': options.old_ref, 'sample': '32MiB JSON string; one initial and three idempotent puts',
    'scope': 'isolated FileSessionArchive process, not whole Eve RSS', 'results': results}, ensure_ascii=False, indent=2))
