#!/usr/bin/env python3
"""Linux 同样本语义布局峰值对照；只读本地源码/构建依赖，无模型或网络请求。"""
from pathlib import Path
import argparse
import hashlib
import json
import shutil
import subprocess
import sys
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--deps', required=True, type=Path, help='已构建的 debug/deps 目录，请勿并行编译')
parser.add_argument('--old-ref', default='aa9b742')
parser.add_argument('--rustc', default=shutil.which('rustc'))
options = parser.parse_args()
if not sys.platform.startswith('linux') or not options.rustc:
    parser.error('需要 Linux 和可用 rustc')
root = Path(__file__).resolve().parents[1]
prior = subprocess.check_output(['git', 'show', f'{options.old_ref}:crates/semantic-plugin/src/items.rs'], cwd=root, text=True)
current = (root/'crates/semantic-plugin/src/items.rs').read_text().split('#[cfg(test)]')[0]
base = r'''
extern crate eve_memory_api;
extern crate eve_semantic_api;
extern crate ring;
mod segments { pub const SEGMENT_EVIDENCE: usize = 64; }
mod items {
SOURCE
pub fn profile(reader: &dyn eve_memory_api::MemoryService) {
    let result = layout(reader, true).unwrap();
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    let mut retained = 0usize;
    let mut count = 0usize;
    for item in result.ledger.iter().chain(result.blocks.iter().flat_map(|b| &b.items)) {
        retained += item.text.len(); count += 1;
        let input = embed_input(item).unwrap_or_default();
        let excerpt = if item.text.len() > eve_memory_api::MAX_RECALL_EXCERPT_BYTES {
            format!("{}…", prefix(&item.text, eve_memory_api::MAX_RECALL_EXCERPT_BYTES - "…".len()))
        } else { item.text.clone() };
        digest.update(format!("{}\n{}\n{}\n{}\n{}\n", item.key, item.evidence_revision, source_key(&item.source), content_hash(&input), excerpt).as_bytes());
    }
    println!("items={count} retained_text_bytes={retained} manifest={}", digest.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect::<String>());
}
}
use eve_memory_api::*;
struct Paged;
const COUNT: u64 = 4097;
fn evidence(revision: u64) -> InteractionEvidence {
    let text = "旧原文中文".repeat(2000);
    InteractionEvidence {
        id: format!("e-{revision}"), revision, at_ms: revision,
        source: EvidenceSource::CompletedInteraction {
            message_id: format!("m-{revision}"), session_revision: revision * 2,
            turn_id: revision, user_text: text.clone(), assistant_text: text,
        },
    }
}
impl MemoryService for Paged {
    fn snapshot(&self) -> MemoryResult<MemorySnapshot> {
        Ok(MemorySnapshot {
            scope: MemoryScope { channel: "test".into(), session_id: "profile".into(), user_id: "synthetic".into() },
            revision: COUNT + 200, evidence: vec![evidence(COUNT + 100)], preferences: vec![], archived_evidence: COUNT,
        })
    }
    fn archived(&self, after: u64, limit: usize) -> MemoryResult<ArchivedEvidencePage> {
        let end = (after + limit as u64).min(COUNT);
        Ok(ArchivedEvidencePage { evidence: (after+1..=end).map(evidence).collect(), next_after: (end < COUNT).then_some(end) })
    }
}
fn main() { items::profile(&Paged); }
'''

deps = options.deps.resolve()
args = []
for crate in ['eve_memory_api', 'eve_semantic_api', 'ring']:
    libraries = list(deps.glob(f'lib{crate}-*.rlib'))
    if not libraries:
        parser.error(f'请先构建依赖：缺少 {crate}')
    library = max(libraries, key=lambda p: p.stat().st_mtime_ns)
    args += ['--extern', f'{crate}={library}']
wrapper = ('import subprocess,resource,time,sys\nstart=time.monotonic()\n'
           'subprocess.run([sys.argv[1]],check=True)\n'
           'print(f"max_rss_kib={resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss} '
           'elapsed_seconds={time.monotonic()-start:.3f}",file=sys.stderr)')
results = {}
with tempfile.TemporaryDirectory(prefix='eve-semantic-layout-') as directory:
    for name, source in [('old', prior), ('new', current)]:
        source = '\n'.join(line for line in source.splitlines() if not line.startswith('//!'))
        file = Path(directory) / f'{name}.rs'
        binary = Path(directory) / name
        file.write_text(base.replace('SOURCE', source), encoding='utf-8')
        subprocess.run([options.rustc, '--edition=2024', '--crate-name', f'layout_{name}',
                        '-L', f'dependency={deps}', *args, '-C', 'debuginfo=0',
                        '-A', 'dead_code', str(file), '-o', str(binary)], check=True, timeout=120)
        result = subprocess.run([sys.executable, '-c', wrapper, str(binary)],
                                capture_output=True, text=True, check=True, timeout=60)
        results[name] = {'output': result.stdout.strip(), 'measurement': result.stderr.strip()}
if results['old']['output'].split('manifest=')[1] != results['new']['output'].split('manifest=')[1]:
    raise SystemExit('新旧向量输入和片段摘要不一致，不能计为性能通过')
report = {
    'baseline_ref': options.old_ref,
    'baseline_source_sha256': hashlib.sha256(prior.encode()).hexdigest(),
    'current_source_sha256': hashlib.sha256(current.encode()).hexdigest(),
    'compiler': subprocess.check_output([options.rustc, '--version'], text=True).strip(),
    'source_head': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip(),
    'working_tree_dirty': bool(subprocess.check_output(['git', 'status', '--porcelain', '--', 'crates/semantic-plugin/src/items.rs'], cwd=root, text=True).strip()),
    'fixture': '4097 条合成归档，每个用户/助手字段 30000 字节，另有一条热证据',
    'manifest_equal': True, 'results': results,
}
print(json.dumps(report, ensure_ascii=False, indent=2))
