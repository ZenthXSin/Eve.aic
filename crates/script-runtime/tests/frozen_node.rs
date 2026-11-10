#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use eve_llm_api::{ContextScope, SharedToolSteps, ToolExecutionContext, ToolExecutionError};
use eve_script_runtime::{
    FrozenNodeOutcome, FrozenNodeRuntime, FrozenNodeTestBundle, FrozenNodeTestReceipt,
    FrozenNodeTestRunner,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    net::TcpListener,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

const NODE_PATH: &str = "runtime/node";
const OUTPUT_LIMIT: usize = 32 * 1024;

fn owner() -> ContextScope {
    ContextScope {
        user_id: "frozen-node-owner".into(),
        session_id: "frozen-node-session".into(),
    }
}

fn context(call_id: &str, pool: &SharedToolSteps) -> ToolExecutionContext {
    ToolExecutionContext::new()
        .with_scope(Some(owner()))
        .with_call_id(call_id.into())
        .with_shared_steps(pool.clone())
}

// PATH 是本地测试宿主提供的可信工具配置，不接受待测脚本指定的可执行文件。
// Linux 缺少工具或 namespace 时必须验收失败，不跳过，也不降级为宿主 Node。
fn trusted_binary(name: &str) -> PathBuf {
    let path = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("Linux 冻结进程验收缺少可信宿主工具：{name}"))
        .canonicalize()
        .expect("可信工具须能解析为真实绝对路径");
    let metadata = fs::symlink_metadata(&path).unwrap();
    assert!(metadata.is_file() && metadata.permissions().mode() & 0o100 != 0);
    path
}

// 只解析已由宿主信任的 Node ELF，不执行待测 bundle，也不查询动态来源。
// ldd 通常列出解释器，但单独读取 PT_INTERP 避免漏掉实际 exec 所需的绝对路径。
fn elf_interpreter(binary: &Path) -> PathBuf {
    let mut file = File::open(binary).unwrap();
    let mut header = [0u8; 64];
    file.read_exact(&mut header).unwrap();
    assert_eq!(&header[..4], b"\x7fELF", "可信 Node 须为 ELF");
    assert_eq!(header[4], 2, "首版 fixture 只核对 ELF64");
    assert_eq!(header[5], 1, "首版 fixture 只核对小端 ELF64");
    let offset = u64::from_le_bytes(header[32..40].try_into().unwrap());
    let size = u16::from_le_bytes(header[54..56].try_into().unwrap()) as u64;
    let count = u16::from_le_bytes(header[56..58].try_into().unwrap()) as u64;
    assert!(size >= 56 && count > 0 && count <= 1024);
    for index in 0..count {
        file.seek(SeekFrom::Start(offset + index * size)).unwrap();
        let mut program = [0u8; 56];
        file.read_exact(&mut program).unwrap();
        if u32::from_le_bytes(program[..4].try_into().unwrap()) != 3 {
            continue;
        }
        let at = u64::from_le_bytes(program[8..16].try_into().unwrap());
        let bytes = u64::from_le_bytes(program[32..40].try_into().unwrap());
        assert!((2..=4096).contains(&bytes));
        file.seek(SeekFrom::Start(at)).unwrap();
        let mut interpreter = vec![0; bytes as usize];
        file.read_exact(&mut interpreter).unwrap();
        assert_eq!(interpreter.pop(), Some(0));
        let path = PathBuf::from(String::from_utf8(interpreter).unwrap());
        assert!(path.is_absolute());
        return path;
    }
    panic!("可信 Node 缺少 PT_INTERP；此 fixture 不支持静态运行时");
}

fn ldd_paths(ldd: &Path, trusted_elf: &Path) -> Vec<PathBuf> {
    assert_eq!(fs::canonicalize(trusted_elf).unwrap(), trusted_elf);
    let output = Command::new(ldd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .arg(trusted_elf)
        .output()
        .expect("可信 ELF 依赖查询须能启动");
    assert!(
        output.status.success(),
        "可信 ELF 依赖查询失败：{}，{}",
        trusted_elf.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains("not found"), "可信 Node 依赖不完整：{text}");
    if text.trim() == "statically linked" {
        return vec![];
    }
    let mut paths = Vec::new();
    for line in text.lines() {
        let words: Vec<_> = line.split_whitespace().collect();
        if words.is_empty() || words[0].starts_with("linux-vdso") {
            continue;
        }
        let name = words
            .iter()
            .position(|word| *word == "=>")
            .and_then(|index| words.get(index + 1))
            .copied()
            .unwrap_or(words[0]);
        assert!(name.starts_with('/'), "未核对的 ldd 输出：{line}");
        let path = PathBuf::from(name);
        assert!(path.is_file(), "可信运行时依赖缺失：{}", path.display());
        paths.push(path);
    }
    paths
}

struct RuntimeFixture {
    rootfs: tempfile::TempDir,
    bwrap: PathBuf,
    interpreter: String,
    // 保留 loader 使用的原绝对路径；目标文件本身从 canonical 宿主文件复制。
    libraries: BTreeMap<PathBuf, PathBuf>,
}

impl RuntimeFixture {
    fn new() -> Self {
        let node = trusted_binary("node");
        let bwrap = trusted_binary("bwrap");
        let ldd = trusted_binary("ldd");
        let interpreter = elf_interpreter(&node);
        let mut libraries = BTreeMap::new();
        libraries.insert(interpreter.clone(), interpreter.canonicalize().unwrap());
        let interpreter_canonical = interpreter.canonicalize().unwrap();
        let mut pending = vec![node.clone()];
        let mut seen = BTreeSet::new();
        while let Some(elf) = pending.pop() {
            if !seen.insert(elf.clone()) {
                continue;
            }
            // 只对可信 Node 与其受信系统 ELF 依赖运行 ldd，绝不对输入代码运行。
            for lookup_path in ldd_paths(&ldd, &elf) {
                let canonical = lookup_path.canonicalize().unwrap();
                let metadata = fs::symlink_metadata(&canonical).unwrap();
                assert!(metadata.is_file());
                libraries.insert(lookup_path, canonical.clone());
                // ELF 解释器已明确复制；glibc loader 自身不支持 ldd 查询。
                if canonical != interpreter_canonical {
                    pending.push(canonical);
                }
            }
        }
        assert!(!libraries.is_empty());
        let rootfs = tempfile::tempdir().unwrap();
        assert_eq!(rootfs.path().canonicalize().unwrap(), rootfs.path());
        copy_regular(&node, &rootfs.path().join(NODE_PATH), rootfs.path());
        for (lookup_path, canonical) in &libraries {
            let relative = lookup_path.strip_prefix("/").unwrap();
            copy_regular(canonical, &rootfs.path().join(relative), rootfs.path());
        }
        assert_regular_tree(rootfs.path());
        Self {
            rootfs,
            bwrap,
            interpreter: interpreter
                .strip_prefix("/")
                .unwrap()
                .to_str()
                .unwrap()
                .into(),
            libraries,
        }
    }

    fn digest(&self) -> String {
        let pool = SharedToolSteps::new(1).unwrap();
        FrozenNodeRuntime::bundle_sha256(self.rootfs.path(), NODE_PATH, &context("prepare", &pool))
            .unwrap()
    }

    fn prepare(&self) -> FrozenNodeRuntime {
        let pool = SharedToolSteps::new(1).unwrap();
        FrozenNodeRuntime::prepare(
            self.rootfs.path(),
            NODE_PATH,
            &self.digest(),
            &context("prepare", &pool),
        )
        .unwrap()
    }

    fn runner(&self) -> FrozenNodeTestRunner {
        FrozenNodeTestRunner::new(self.prepare(), self.bwrap.clone()).unwrap()
    }
}

fn copy_regular(source: &Path, destination: &Path, root: &Path) {
    assert_eq!(source.canonicalize().unwrap(), source);
    assert!(fs::symlink_metadata(source).unwrap().is_file());
    let parent = destination.parent().unwrap();
    fs::create_dir_all(parent).unwrap();
    for directory in parent
        .ancestors()
        .take_while(|directory| *directory != root)
    {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::copy(source, destination).unwrap();
    let executable = fs::metadata(source).unwrap().permissions().mode() & 0o111;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o644 | executable)).unwrap();
    assert!(fs::symlink_metadata(destination).unwrap().is_file());
}

fn assert_regular_tree(root: &Path) {
    let mut directories = vec![root.to_owned()];
    while let Some(directory) = directories.pop() {
        for child in fs::read_dir(directory).unwrap() {
            let child = child.unwrap();
            let metadata = fs::symlink_metadata(child.path()).unwrap();
            assert!(!metadata.file_type().is_symlink());
            assert!(metadata.is_file() || metadata.is_dir());
            if metadata.is_dir() {
                directories.push(child.path());
            }
        }
    }
}

fn bundle(entry: &str, source: &str, arguments: &[&str]) -> FrozenNodeTestBundle {
    FrozenNodeTestBundle::new(
        BTreeMap::from([(entry.into(), source.as_bytes().to_vec())]),
        entry.into(),
        arguments.iter().map(|value| (*value).into()).collect(),
    )
    .unwrap()
}

fn assert_exit(receipt: &FrozenNodeTestReceipt, code: i32) {
    assert!(receipt.started(), "Node 未确认启动：{receipt:?}");
    assert_eq!(
        receipt.outcome(),
        &FrozenNodeOutcome::Exited {
            exit_code: Some(code),
            signal: None,
        },
        "{receipt:?}"
    );
}

#[tokio::test]
async fn real_cjs_mjs_and_nonzero_exits_have_bound_receipts() {
    let fixture = RuntimeFixture::new();
    let runtime_digest = fixture.digest();
    let runner = fixture.runner();
    let pool = SharedToolSteps::new(3).unwrap();
    let cjs = bundle(
        "entry.cjs",
        "const assert=require('node:assert/strict'); assert.equal(2+2,4); assert.deepEqual(process.argv.slice(2),['one','two words']); console.log('cjs-ok');",
        &["one", "two words"],
    );
    let command_digest = runner.command_sha256(&cjs).unwrap();
    let receipt = runner
        .run_independent(&cjs, "actual-execution", "cjs", context("cjs", &pool))
        .await
        .unwrap();
    assert_exit(&receipt, 0);
    assert_eq!(receipt.stdout(), "cjs-ok\n");
    assert_eq!(receipt.owner(), &owner());
    assert_eq!(receipt.execution_id(), "actual-execution");
    assert_eq!(receipt.call_id(), "cjs");
    assert_eq!(receipt.input_sha256(), cjs.sha256());
    assert_eq!(receipt.runtime_sha256(), runtime_digest);
    assert_eq!(receipt.command_sha256(), command_digest);
    assert!(!receipt.stdout_truncated() && !receipt.stderr_truncated());

    let mjs = FrozenNodeTestBundle::new(
        BTreeMap::from([
            ("entry.mjs".into(), b"import assert from 'node:assert/strict'; import {sum} from './lib/math.mjs'; assert.equal(sum(3,4),7); console.log('mjs-ok');".to_vec()),
            ("lib/math.mjs".into(), b"export const sum=(a,b)=>a+b;".to_vec()),
        ]),
        "entry.mjs".into(),
        vec![],
    ).unwrap();
    let receipt = runner
        .run_independent(&mjs, "actual-execution", "mjs", context("mjs", &pool))
        .await
        .unwrap();
    assert_exit(&receipt, 0);
    assert_eq!(receipt.stdout(), "mjs-ok\n");

    let failing = bundle(
        "failure.cjs",
        "process.stderr.write('diagnostic-seven'); process.exit(7);",
        &[],
    );
    let receipt = runner
        .run_independent(
            &failing,
            "actual-execution",
            "exit-seven",
            context("exit-seven", &pool),
        )
        .await
        .unwrap();
    assert_exit(&receipt, 7);
    assert!(receipt.stderr().contains("diagnostic-seven"));
    assert_eq!(pool.usage().unwrap().tool_calls, 3);
    assert_eq!(pool.usage().unwrap().model_requests, 0);
}

#[test]
fn digests_separate_input_command_and_exact_runtime_executable_bits() {
    let fixture = RuntimeFixture::new();
    let node = fixture.rootfs.path().join(NODE_PATH);
    fs::set_permissions(&node, fs::Permissions::from_mode(0o755)).unwrap();
    let original = fixture.digest();
    let runner = fixture.runner();
    let files = BTreeMap::from([
        ("a.cjs".into(), b"console.log('a');".to_vec()),
        ("b.cjs".into(), b"console.log('b');".to_vec()),
    ]);
    let first =
        FrozenNodeTestBundle::new(files.clone(), "a.cjs".into(), vec!["one".into()]).unwrap();
    let equal =
        FrozenNodeTestBundle::new(files.clone(), "a.cjs".into(), vec!["one".into()]).unwrap();
    let arguments =
        FrozenNodeTestBundle::new(files.clone(), "a.cjs".into(), vec!["two".into()]).unwrap();
    let entry =
        FrozenNodeTestBundle::new(files.clone(), "b.cjs".into(), vec!["one".into()]).unwrap();
    let mut changed_files = files;
    changed_files.insert("b.cjs".into(), b"console.log('new bytes');".to_vec());
    let changed =
        FrozenNodeTestBundle::new(changed_files, "a.cjs".into(), vec!["one".into()]).unwrap();
    assert_eq!(first.sha256(), equal.sha256());
    assert_eq!(first.sha256(), arguments.sha256());
    assert_eq!(first.sha256(), entry.sha256());
    assert_ne!(first.sha256(), changed.sha256());
    assert_eq!(
        runner.command_sha256(&first).unwrap(),
        runner.command_sha256(&equal).unwrap()
    );
    assert_ne!(
        runner.command_sha256(&first).unwrap(),
        runner.command_sha256(&arguments).unwrap()
    );
    assert_ne!(
        runner.command_sha256(&first).unwrap(),
        runner.command_sha256(&entry).unwrap()
    );

    let directory = fixture.rootfs.path().join("runtime");
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o750)).unwrap();
    assert_ne!(fixture.digest(), original, "目录执行位应参与结构摘要");
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&node, fs::Permissions::from_mode(0o754)).unwrap();
    assert_ne!(
        fixture.digest(),
        original,
        "文件执行位应精确绑定，而非布尔值"
    );
    fs::set_permissions(&node, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(fixture.digest(), original);
    assert_eq!(fixture.prepare().sha256(), original);

    fs::write(
        fixture.rootfs.path().join("runtime/host-note"),
        b"first version",
    )
    .unwrap();
    let first_runtime = fixture.digest();
    assert_ne!(first_runtime, original);
    fs::write(
        fixture.rootfs.path().join("runtime/host-note"),
        b"second version",
    )
    .unwrap();
    assert_ne!(fixture.digest(), first_runtime);

    let pool = SharedToolSteps::new(1).unwrap();
    fs::set_permissions(&node, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        FrozenNodeRuntime::prepare(
            fixture.rootfs.path(),
            NODE_PATH,
            &fixture.digest(),
            &context("no-exec", &pool)
        ),
        Err(ToolExecutionError::Rejected(_))
    ));
    fs::set_permissions(&node, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        FrozenNodeRuntime::prepare(
            fixture.rootfs.path(),
            NODE_PATH,
            &original,
            &context("no-traverse", &pool)
        ),
        Err(ToolExecutionError::Rejected(_))
    ));
    // 保证测试临时目录在非 root 测试用户下同样可清理。
    fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
}

#[tokio::test]
async fn prepared_runtime_uses_private_bytes_after_source_changes_and_removal() {
    let fixture = RuntimeFixture::new();
    let runtime_digest = fixture.digest();
    let runner = fixture.runner();
    let source_path = fixture.rootfs.path().to_str().unwrap().to_owned();
    fs::write(
        fixture.rootfs.path().join(NODE_PATH),
        b"source changed after prepare",
    )
    .unwrap();
    fs::remove_dir_all(fixture.rootfs.path().join("runtime")).unwrap();
    let test = bundle(
        "private.cjs",
        "const assert=require('node:assert/strict'); const fs=require('node:fs'); assert.equal(fs.existsSync(process.argv[2]),false); assert.ok(fs.readFileSync('/runtime/node').subarray(0,4).equals(Buffer.from([127,69,76,70]))); console.log('private-snapshot-ok');",
        &[&source_path],
    );
    let pool = SharedToolSteps::new(1).unwrap();
    let receipt = runner
        .run_independent(
            &test,
            "private-snapshot",
            "private",
            context("private", &pool),
        )
        .await
        .unwrap();
    assert_exit(&receipt, 0);
    assert_eq!(receipt.runtime_sha256(), runtime_digest);
    assert_eq!(receipt.stdout(), "private-snapshot-ok\n");
}

#[tokio::test]
async fn host_files_processes_and_rpc_are_absent_work_is_readonly_and_tmp_is_writable() {
    let fixture = RuntimeFixture::new();
    let runner = fixture.runner();
    let host = tempfile::tempdir().unwrap();
    let secret = host.path().join("parent-secret");
    fs::write(&secret, b"must remain on the host").unwrap();
    let host_secret = secret.to_str().unwrap();
    let test = bundle(
        "isolation.cjs",
        r#"const assert=require('node:assert/strict');
const fs=require('node:fs');
assert.equal(fs.existsSync(process.argv[2]),false);
assert.equal(fs.existsSync('/etc/passwd'),false);
assert.equal(fs.existsSync('/proc/self'),false);
assert.equal(typeof globalThis.tools,'undefined');
assert.equal(typeof globalThis.write,'undefined');
assert.equal(process.env.PATH,'/runtime');
assert.equal(process.env.HOME,'/tmp');
assert.equal(process.env.CARGO_MANIFEST_DIR,undefined);
assert.equal(fs.readFileSync(0).length,0);
assert.throws(()=>fs.writeFileSync('/work/isolation.cjs','changed'),e=>e.code==='ERR_ACCESS_DENIED'&&e.permission==='FileSystemWrite');
assert.throws(()=>fs.writeFileSync('/runtime/node','changed'),e=>e.code==='ERR_ACCESS_DENIED'&&e.permission==='FileSystemWrite');
fs.writeFileSync('/tmp/local-note','temporary bytes');
assert.equal(fs.readFileSync('/tmp/local-note','utf8'),'temporary bytes');
assert.throws(()=>require('node:child_process').spawnSync(process.execPath,['-e',"process.stdout.write('forbidden-child')"]),e=>e.code==='ERR_ACCESS_DENIED'&&e.permission==='ChildProcess');
assert.equal(typeof process.execve,'function');
assert.throws(()=>process.execve(process.execPath,[process.execPath,'-e','process.exit(17)'],{}),e=>e.code==='ERR_ACCESS_DENIED'&&e.permission==='ChildProcess');
assert.throws(()=>new (require('node:worker_threads').Worker)('process.exit(18)',{eval:true}),e=>e.code==='ERR_ACCESS_DENIED');
console.log('isolation-ok');"#,
        &[host_secret],
    );
    let pool = SharedToolSteps::new(1).unwrap();
    let receipt = runner
        .run_independent(&test, "isolation", "isolation", context("isolation", &pool))
        .await
        .unwrap();
    assert_exit(&receipt, 0);
    assert_eq!(receipt.stdout(), "isolation-ok\n");
    assert_eq!(fs::read(secret).unwrap(), b"must remain on the host");
}

#[tokio::test]
async fn network_namespace_cannot_reach_a_real_parent_listener() {
    let fixture = RuntimeFixture::new();
    let runner = fixture.runner();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let test = bundle(
        "network.cjs",
        r#"const net=require('node:net');
const socket=net.connect({host:'127.0.0.1',port:Number(process.argv[2])});
socket.once('connect',()=>{console.error('reached parent network');socket.destroy();process.exit(9);});
socket.once('error',()=>{console.log('network-isolated');socket.destroy();});
setTimeout(()=>{console.error('network check timed out');process.exit(8);},2000).unref();"#,
        &[&port],
    );
    let pool = SharedToolSteps::new(1).unwrap();
    let receipt = runner
        .run_independent(&test, "network", "network", context("network", &pool))
        .await
        .unwrap();
    assert_exit(&receipt, 0);
    assert_eq!(receipt.stdout(), "network-isolated\n");
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
}

#[tokio::test]
async fn real_stdout_stderr_and_lossy_utf8_are_bounded_without_blocking_exit() {
    let fixture = RuntimeFixture::new();
    let runner = fixture.runner();
    let pool = SharedToolSteps::new(3).unwrap();
    let ascii = bundle(
        "output.cjs",
        "const fs=require('node:fs'); fs.writeSync(1,'o'.repeat(100000)); fs.writeSync(2,'e'.repeat(100000));",
        &[],
    );
    let receipt = runner
        .run_independent(&ascii, "output", "ascii", context("ascii", &pool))
        .await
        .unwrap();
    assert_exit(&receipt, 0);
    assert_eq!(receipt.stdout().len(), OUTPUT_LIMIT);
    assert_eq!(receipt.stderr().len(), OUTPUT_LIMIT);
    assert!(receipt.stdout_truncated() && receipt.stderr_truncated());
    assert!(receipt.stdout().bytes().all(|byte| byte == b'o'));

    let invalid = bundle(
        "invalid.cjs",
        "const fs=require('node:fs'); fs.writeSync(1,Buffer.alloc(100000,255)); fs.writeSync(2,Buffer.alloc(100000,255));",
        &[],
    );
    let receipt = runner
        .run_independent(&invalid, "output", "invalid", context("invalid", &pool))
        .await
        .unwrap();
    assert_exit(&receipt, 0);
    assert!(receipt.stdout().len() <= OUTPUT_LIMIT && receipt.stderr().len() <= OUTPUT_LIMIT);
    assert!(receipt.stdout_truncated() && receipt.stderr_truncated());
    assert!(receipt.stdout().contains('\u{fffd}'));
    assert!(!receipt.stdout().contains("eve-frozen-node-started"));

    let marker = bundle(
        "marker.cjs",
        "process.stdout.write('eve-frozen-node-started:v1\\n');",
        &[],
    );
    let receipt = runner
        .run_independent(&marker, "output", "marker", context("marker", &pool))
        .await
        .unwrap();
    assert_exit(&receipt, 0);
    assert_eq!(
        receipt.stdout(),
        "eve-frozen-node-started:v1\n",
        "只移除宿主 bootstrap 的一次前缀，保留脚本真实输出"
    );
    assert!(!receipt.stdout_truncated());
}

#[tokio::test]
async fn launcher_bytes_are_in_the_command_digest_and_changes_reject_before_charge() {
    let fixture = RuntimeFixture::new();
    let copy = tempfile::tempdir().unwrap();
    let launcher = copy.path().join("bwrap");
    copy_regular(&fixture.bwrap, &launcher, copy.path());
    let runner = FrozenNodeTestRunner::new(fixture.prepare(), launcher.clone()).unwrap();
    let same_bytes = fixture.runner();
    let test = bundle(
        "launcher.cjs",
        "throw new Error('changed launcher must not run');",
        &[],
    );
    let original = runner.command_sha256(&test).unwrap();
    assert_eq!(original, same_bytes.command_sha256(&test).unwrap());
    let mut bytes = fs::read(&launcher).unwrap();
    let last = bytes.last_mut().unwrap();
    *last ^= 1;
    fs::write(&launcher, bytes).unwrap();
    let changed = FrozenNodeTestRunner::new(fixture.prepare(), launcher.clone()).unwrap();
    assert_ne!(original, changed.command_sha256(&test).unwrap());
    let pool = SharedToolSteps::new(1).unwrap();
    assert!(matches!(
        runner
            .run_independent(&test, "launcher", "changed", context("changed", &pool))
            .await,
        Err(ToolExecutionError::Rejected(_))
    ));
    assert_eq!(pool.usage().unwrap().tool_calls, 0);
}

#[tokio::test]
async fn cancellation_and_expiry_before_launch_do_not_charge_or_consume_call_identity() {
    let fixture = RuntimeFixture::new();
    let runner = fixture.runner();
    let test = bundle("stop.cjs", "console.log('ran-once');", &[]);
    let pool = SharedToolSteps::new(2).unwrap();
    let cancelled = context("cancelled", &pool);
    cancelled.cancellation_handle().cancel();
    assert!(matches!(
        runner
            .run_independent(&test, "pre-stop", "cancelled", cancelled)
            .await,
        Err(ToolExecutionError::Cancelled(_))
    ));
    let expired =
        context("expired", &pool).with_deadline(Some(Instant::now() - Duration::from_secs(1)));
    assert!(matches!(
        runner
            .run_independent(&test, "pre-stop", "expired", expired)
            .await,
        Err(ToolExecutionError::Cancelled(_))
    ));
    assert_eq!(pool.usage().unwrap().tool_calls, 0);
    for call_id in ["cancelled", "expired"] {
        let receipt = runner
            .run_independent(&test, "pre-stop", call_id, context(call_id, &pool))
            .await
            .unwrap();
        assert_exit(&receipt, 0);
        assert_eq!(receipt.stdout(), "ran-once\n");
    }
    assert_eq!(pool.usage().unwrap().tool_calls, 2);
}

#[tokio::test]
async fn running_process_is_cancelled_and_original_deadline_is_enforced() {
    let fixture = RuntimeFixture::new();
    let runner = fixture.runner();
    let test = bundle("wait.cjs", "setInterval(()=>{},1000);", &[]);
    let pool = SharedToolSteps::new(2).unwrap();
    let ctx = context("cancel-running", &pool);
    let cancel = ctx.cancellation_handle();
    let began = Instant::now();
    let (result, ()) = tokio::join!(
        runner.run_independent(&test, "inflight-stop", "cancel-running", ctx),
        async {
            // 当前线程直到运行器开始等待真实进程后才会轮询这个 future。
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel.cancel();
        }
    );
    let receipt = result.unwrap();
    assert_eq!(receipt.outcome(), &FrozenNodeOutcome::Cancelled);
    assert!(began.elapsed() < Duration::from_secs(10));
    assert_eq!(pool.usage().unwrap().tool_calls, 1);
    assert!(matches!(
        runner
            .run_independent(
                &test,
                "inflight-stop",
                "cancel-running",
                context("cancel-running", &pool)
            )
            .await,
        Err(ToolExecutionError::Rejected(_))
    ));

    let began = Instant::now();
    let deadline = context("deadline", &pool).with_deadline(Some(began + Duration::from_secs(2)));
    let result = runner
        .run_independent(&test, "inflight-stop", "deadline", deadline)
        .await;
    match result {
        Ok(receipt) => assert_eq!(receipt.outcome(), &FrozenNodeOutcome::Cancelled),
        // 很慢的受信宿主可能在读取私有快照时已到原期限；同样不得继续启动。
        Err(error) => assert!(matches!(error, ToolExecutionError::Cancelled(_))),
    }
    assert!(began.elapsed() < Duration::from_secs(5));
    assert_eq!(pool.usage().unwrap().tool_calls, 2);
}

#[tokio::test]
async fn precharged_and_independent_runs_share_budget_and_duplicate_calls_never_replay() {
    let fixture = RuntimeFixture::new();
    let runner = fixture.runner();
    let test = bundle("budget.cjs", "console.log('one-execution');", &[]);
    let pool = SharedToolSteps::new(3).unwrap();
    assert!(pool.reserve(1).unwrap());
    let receipt = runner
        .run_precharged(&test, "budget", "precharged", context("precharged", &pool))
        .await
        .unwrap();
    assert_exit(&receipt, 0);
    assert_eq!(pool.usage().unwrap().tool_calls, 1, "叶子不得重复收费");
    assert!(matches!(
        runner
            .run_precharged(&test, "budget", "precharged", context("precharged", &pool))
            .await,
        Err(ToolExecutionError::Rejected(_))
    ));
    assert_eq!(pool.usage().unwrap().tool_calls, 1);

    let receipt = runner
        .run_independent(
            &test,
            "budget",
            "independent",
            context("independent", &pool),
        )
        .await
        .unwrap();
    assert_exit(&receipt, 0);
    assert_eq!(pool.usage().unwrap().tool_calls, 2);
    let reserved_parent = pool.with_parent_reserve(1);
    assert!(matches!(
        runner
            .run_independent(
                &test,
                "budget",
                "no-child-budget",
                context("no-child-budget", &reserved_parent)
            )
            .await,
        Err(ToolExecutionError::Rejected(_))
    ));
    assert_eq!(pool.usage().unwrap().tool_calls, 2);
    assert_eq!(pool.remaining().unwrap(), 1);
}

#[tokio::test]
async fn execution_requires_real_owner_matching_call_id_and_original_shared_budget() {
    let fixture = RuntimeFixture::new();
    let runner = fixture.runner();
    let test = bundle("identity.cjs", "throw new Error('must not launch');", &[]);
    let pool = SharedToolSteps::new(3).unwrap();
    let invalid = [
        ToolExecutionContext::new()
            .with_call_id("bound".into())
            .with_shared_steps(pool.clone()),
        ToolExecutionContext::new()
            .with_scope(Some(owner()))
            .with_call_id("bound".into()),
        context("different-call", &pool),
        context("bound", &pool).with_scope(Some(ContextScope {
            user_id: "".into(),
            session_id: "session".into(),
        })),
    ];
    for ctx in invalid {
        assert!(matches!(
            runner
                .run_independent(&test, "identity", "bound", ctx)
                .await,
            Err(ToolExecutionError::Rejected(_))
        ));
    }
    assert_eq!(pool.usage().unwrap().tool_calls, 0);
    for execution_id in ["", "control\ncharacter"] {
        assert!(matches!(
            runner
                .run_independent(&test, execution_id, "bound", context("bound", &pool))
                .await,
            Err(ToolExecutionError::Rejected(_))
        ));
    }
    assert_eq!(pool.usage().unwrap().tool_calls, 0);
}

#[tokio::test]
async fn missing_runtime_library_or_interpreter_never_becomes_a_successful_test() {
    let fixture = RuntimeFixture::new();
    let test = bundle("missing.cjs", "console.log('must not run');", &[]);
    let pool = SharedToolSteps::new(2).unwrap();
    let libc_paths: Vec<_> = fixture
        .libraries
        .iter()
        .filter(|(_, canonical)| {
            canonical
                .file_name()
                .is_some_and(|name| name == "libc.so.6")
        })
        .map(|(path, _)| fixture.rootfs.path().join(path.strip_prefix("/").unwrap()))
        .collect();
    assert!(!libc_paths.is_empty(), "可信 Node fixture 须明确冻结 libc");
    for path in &libc_paths {
        fs::remove_file(path).unwrap();
    }
    let missing_library = fixture.runner();
    let receipt = missing_library
        .run_independent(
            &test,
            "broken-runtime",
            "missing-library",
            context("missing-library", &pool),
        )
        .await
        .unwrap();
    assert!(!receipt.started(), "缺库时不能确认 Node 启动：{receipt:?}");
    assert_eq!(receipt.outcome(), &FrozenNodeOutcome::Unknown);
    assert!(!receipt.stdout().contains("must not run"));

    fs::remove_file(fixture.rootfs.path().join(&fixture.interpreter)).unwrap();
    let missing_interpreter = fixture.runner();
    let receipt = missing_interpreter
        .run_independent(
            &test,
            "broken-runtime",
            "missing-interpreter",
            context("missing-interpreter", &pool),
        )
        .await
        .unwrap();
    assert!(!receipt.started());
    assert_eq!(receipt.outcome(), &FrozenNodeOutcome::Unknown);
    assert_eq!(pool.usage().unwrap().tool_calls, 2);
}

#[tokio::test]
async fn zero_exit_without_the_real_node_start_marker_remains_unknown() {
    let fixture = RuntimeFixture::new();
    // 模拟可信 launcher 在启动 Node 前直接退出 0；真实退出码本身不能代替启动证明。
    let runner = FrozenNodeTestRunner::new(fixture.prepare(), trusted_binary("true")).unwrap();
    let test = bundle("never.cjs", "throw new Error('Node must not start');", &[]);
    let pool = SharedToolSteps::new(1).unwrap();
    let receipt = runner
        .run_independent(&test, "no-marker", "no-marker", context("no-marker", &pool))
        .await
        .unwrap();
    assert!(!receipt.started());
    assert_eq!(receipt.outcome(), &FrozenNodeOutcome::Unknown);
    assert!(receipt.stdout().is_empty());
    assert_eq!(pool.usage().unwrap().tool_calls, 1);
    assert!(matches!(
        runner
            .run_independent(&test, "no-marker", "no-marker", context("no-marker", &pool))
            .await,
        Err(ToolExecutionError::Rejected(_))
    ));
    assert_eq!(pool.usage().unwrap().tool_calls, 1);
}

#[test]
fn complete_input_bundle_rejects_escape_native_addons_and_resource_overflow() {
    for entry in [
        "../entry.cjs",
        "/entry.cjs",
        "./entry.cjs",
        "dir//entry.cjs",
        "dir\\entry.cjs",
        "entry\0.cjs",
    ] {
        assert!(
            FrozenNodeTestBundle::new(
                BTreeMap::from([(entry.into(), vec![0])]),
                entry.into(),
                vec![]
            )
            .is_err()
        );
    }
    assert!(
        FrozenNodeTestBundle::new(
            BTreeMap::from([("entry.txt".into(), vec![])]),
            "entry.txt".into(),
            vec![]
        )
        .is_err()
    );
    assert!(
        FrozenNodeTestBundle::new(
            BTreeMap::from([("entry.cjs".into(), vec![]), ("addon.node".into(), vec![])]),
            "entry.cjs".into(),
            vec![]
        )
        .is_err()
    );
    assert!(
        FrozenNodeTestBundle::new(
            BTreeMap::from([
                ("entry.cjs".into(), vec![]),
                ("dir".into(), vec![]),
                ("dir/part.cjs".into(), vec![])
            ]),
            "entry.cjs".into(),
            vec![]
        )
        .is_err()
    );
    assert!(
        FrozenNodeTestBundle::new(
            BTreeMap::from([("entry.cjs".into(), vec![0; 8 * 1024 * 1024 + 1])]),
            "entry.cjs".into(),
            vec![]
        )
        .is_err()
    );
    assert!(
        FrozenNodeTestBundle::new(
            (0..65)
                .map(|index| (format!("{index}.cjs"), vec![]))
                .collect(),
            "0.cjs".into(),
            vec![]
        )
        .is_err()
    );
    assert!(
        FrozenNodeTestBundle::new(
            BTreeMap::from([("entry.cjs".into(), vec![])]),
            "entry.cjs".into(),
            vec!["arg".into(); 17]
        )
        .is_err()
    );
    assert!(
        FrozenNodeTestBundle::new(
            BTreeMap::from([("entry.cjs".into(), vec![])]),
            "entry.cjs".into(),
            vec!["nul\0arg".into()]
        )
        .is_err()
    );
}

#[test]
fn runtime_source_rejects_links_reserved_mounts_oversized_files_and_manifest_mismatch() {
    let rootfs = tempfile::tempdir().unwrap();
    let node = rootfs.path().join("node");
    // 仅测试输入校验，不启动此普通文件，也不会对它使用 ldd。
    fs::write(&node, b"validation fixture").unwrap();
    fs::set_permissions(&node, fs::Permissions::from_mode(0o755)).unwrap();
    let pool = SharedToolSteps::new(1).unwrap();
    let ctx = context("validate", &pool);
    let digest = FrozenNodeRuntime::bundle_sha256(rootfs.path(), "node", &ctx).unwrap();
    fs::write(&node, b"changed before freezing").unwrap();
    assert!(matches!(
        FrozenNodeRuntime::prepare(rootfs.path(), "node", &digest, &ctx),
        Err(ToolExecutionError::Rejected(_))
    ));
    let link = rootfs.path().join("runtime-link");
    symlink(&node, &link).unwrap();
    assert!(FrozenNodeRuntime::bundle_sha256(rootfs.path(), "node", &ctx).is_err());
    fs::remove_file(link).unwrap();
    for reserved in ["work", "control", "tmp", "dev", "proc"] {
        let directory = rootfs.path().join(reserved);
        fs::create_dir(&directory).unwrap();
        assert!(FrozenNodeRuntime::bundle_sha256(rootfs.path(), "node", &ctx).is_err());
        fs::remove_dir(directory).unwrap();
    }
    let oversized = rootfs.path().join("too-large");
    File::create(&oversized)
        .unwrap()
        .set_len(256 * 1024 * 1024 + 1)
        .unwrap();
    assert!(FrozenNodeRuntime::bundle_sha256(rootfs.path(), "node", &ctx).is_err());
    fs::remove_file(oversized).unwrap();
    for index in 0..512 {
        fs::write(rootfs.path().join(format!("file-{index}")), []).unwrap();
    }
    assert!(FrozenNodeRuntime::bundle_sha256(rootfs.path(), "node", &ctx).is_err());
    assert_eq!(pool.usage().unwrap().tool_calls, 0);
}
