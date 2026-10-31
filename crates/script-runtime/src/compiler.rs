use crate::{
    CompilationFailure, Language, MAX_SOURCE_BYTES, Result, digest, fail, reject,
    runtime::{check_stop, guarded, reserve},
};
use eve_llm_api::ToolExecutionContext;
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

// 编译器的地址空间上限是每进程的；跨会话也只能同时启动一个编译器，
// 避免多个各自合法的编译叠加挤占宿主。排队仍服从原调用取消和期限。
pub(crate) static COMPILE_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

const MAX_RUST_ARTIFACT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_RUST_CACHE: usize = 32;
const MAX_COMPILER_DIAGNOSTIC_BYTES: usize = 8192;
static CACHE_CLOCK: AtomicU64 = AtomicU64::new(1);

const FLAGS: &[&str] = &[
    "--edition=2021",
    "--crate-name=eve_plugin",
    "--crate-type=bin",
    "--color=never",
    "--error-format=short",
    "-Cdebuginfo=0",
    "-Copt-level=0",
    "-Cpanic=abort",
    "-Ccodegen-units=1",
];

#[derive(Debug)]
pub(crate) struct CompiledRust {
    _directory: tempfile::TempDir,
    pub(crate) binary: PathBuf,
    hash: String,
    last_used: AtomicU64,
}
fn artifact_hash(path: &Path) -> Result<String> {
    let metadata = std::fs::symlink_metadata(path).map_err(fail)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_RUST_ARTIFACT_BYTES {
        return Err(reject("Rust编译产物无效或超限"));
    }
    let mut file = std::fs::File::open(path)
        .map_err(fail)?
        .take(MAX_RUST_ARTIFACT_BYTES + 1);
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buffer).map_err(fail)?;
        if read == 0 {
            break;
        }
        size += read as u64;
        if size > MAX_RUST_ARTIFACT_BYTES {
            return Err(reject("Rust编译产物超限"));
        }
        hash.update(&buffer[..read]);
    }
    if size != metadata.len() {
        return Err(reject("Rust编译产物读取时变化"));
    }
    Ok(hash
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
fn has_cache_room(cache: &BTreeMap<String, Arc<CompiledRust>>) -> bool {
    cache.len() < MAX_RUST_CACHE || cache.values().any(|v| Arc::strong_count(v) == 1)
}
fn make_cache_room(cache: &mut BTreeMap<String, Arc<CompiledRust>>) -> Result<()> {
    if cache.len() < MAX_RUST_CACHE {
        return Ok(());
    }
    let oldest = cache
        .iter()
        .filter(|(_, v)| Arc::strong_count(v) == 1)
        .min_by_key(|(_, v)| v.last_used.load(Ordering::Relaxed))
        .map(|(key, _)| key.clone())
        .ok_or_else(|| reject("Rust编译缓存均被在途调用持有，请等待调用结束"))?;
    cache.remove(&oldest);
    Ok(())
}
impl CompiledRust {
    pub(crate) fn verify(&self) -> Result<()> {
        if artifact_hash(&self.binary)? != self.hash {
            return Err(reject("Rust插件缓存产物校验失败"));
        }
        Ok(())
    }
    fn touch(&self) {
        self.last_used.store(
            CACHE_CLOCK.fetch_add(1, Ordering::Relaxed),
            Ordering::Relaxed,
        );
    }
}
pub(crate) enum RustPreparation {
    Ready(Arc<CompiledRust>),
    CompilationFailed(CompilationFailure),
}
async fn read_diagnostics(mut stream: tokio::process::ChildStderr) -> Result<(String, bool)> {
    use tokio::io::AsyncReadExt;
    let mut retained = Vec::new();
    let mut truncated = false;
    let mut buffer = [0u8; 4096];
    loop {
        let count = stream.read(&mut buffer).await.map_err(fail)?;
        if count == 0 {
            break;
        }
        let keep = count.min(MAX_COMPILER_DIAGNOSTIC_BYTES.saturating_sub(retained.len()));
        retained.extend_from_slice(&buffer[..keep]);
        truncated |= keep < count;
        // 达到保留上限仍继续排空管道，避免编译器被自己的 stderr 阻塞。
    }
    let mut text = String::from_utf8_lossy(&retained).into_owned();
    if text.len() > MAX_COMPILER_DIAGNOSTIC_BYTES {
        let mut end = MAX_COMPILER_DIAGNOSTIC_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        truncated = true;
    }
    Ok((text, truncated))
}
impl crate::Bubblewrap {
    pub(crate) async fn prepare_rust(
        &self,
        source: &str,
        context: &ToolExecutionContext,
        deadline: Instant,
    ) -> Result<Arc<CompiledRust>> {
        match self.prepare_rust_report(source, context, deadline).await? {
            RustPreparation::Ready(compiled) => Ok(compiled),
            RustPreparation::CompilationFailed(report) => Err(reject(format!(
                "Rust隔离编译失败；旧版本保持可用；退出码 {:?}，信号 {:?}；诊断{}：{}",
                report.exit_code,
                report.signal,
                if report.truncated {
                    "（已截断）"
                } else {
                    ""
                },
                report.diagnostics
            ))),
        }
    }
    pub(crate) async fn prepare_rust_report(
        &self,
        source: &str,
        context: &ToolExecutionContext,
        deadline: Instant,
    ) -> Result<RustPreparation> {
        self.preflight(&Language::Rust)?;
        if self.container.is_some() {
            return Err(reject("当前Rust编译只支持可信Linux bubblewrap编译器"));
        }
        if source.is_empty() || source.len() > MAX_SOURCE_BYTES {
            return Err(reject("Rust源码无效或超限"));
        }
        let _compile_slot = guarded(
            async { COMPILE_SLOTS.acquire().await.map_err(fail) },
            context,
            deadline,
        )
        .await?;
        check_stop(context, deadline)?;
        let rustc = self
            .rustc
            .as_ref()
            .ok_or_else(|| reject("未配置可信rustc"))?;
        // 元数据命令只运行可信编译器，不接受源码或模型参数；无编译器环境覆盖。
        let metadata = tokio::process::Command::new(rustc)
            .arg("--version")
            .arg("--verbose")
            .env_clear()
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output();
        let metadata = guarded(async { metadata.await.map_err(fail) }, context, deadline).await?;
        if !metadata.status.success() || metadata.stdout.len() > 4096 {
            return Err(reject("无法核验Rust编译器版本和目标"));
        }
        let linker = std::fs::canonicalize("/usr/bin/cc")
            .map_err(|_| reject("可信编译环境缺少系统linker"))?;
        if !linker.starts_with("/usr/bin") {
            return Err(reject("系统linker不在公开编译工具目录"));
        }
        let key = digest(&(source, &metadata.stdout, FLAGS, rustc, &linker))?;
        if let Some(cached) = self.rust_cache.lock().map_err(fail)?.get(&key).cloned() {
            cached.verify()?;
            cached.touch();
            return Ok(RustPreparation::Ready(cached));
        }
        if !has_cache_room(&*self.rust_cache.lock().map_err(fail)?) {
            return Err(reject("Rust编译缓存均被在途调用持有，请等待调用结束"));
        }
        reserve(context)?;
        check_stop(context, deadline)?;
        let directory = tempfile::tempdir().map_err(fail)?;
        let source_path = directory.path().join("source.rs");
        std::fs::write(&source_path, source).map_err(fail)?;
        let output = tempfile::tempdir().map_err(fail)?;
        let library = rustc
            .parent()
            .and_then(std::path::Path::parent)
            .map(|p| p.join("lib"))
            .filter(|p| p.is_dir())
            .ok_or_else(|| reject("rustc缺少公开sysroot"))?;
        let mut command = tokio::process::Command::new(&self.bwrap);
        command.env_clear().args([
            "--unshare-all",
            "--unshare-user",
            "--disable-userns",
            "--assert-userns-disabled",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
            "--clearenv",
            "--ro-bind",
            "/usr/lib",
            "/usr/lib",
            "--ro-bind",
            "/lib",
            "/lib",
            "--ro-bind",
            "/usr/bin",
            "/usr/bin",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--size",
            "16777216",
            "--tmpfs",
            "/tmp",
            "--setenv",
            "HOME",
            "/tmp",
            "--setenv",
            "PATH",
            "/usr/bin",
            "--setenv",
            "LANG",
            "C.UTF-8",
        ]);
        if std::path::Path::new("/usr/libexec").exists() {
            command.args(["--ro-bind", "/usr/libexec", "/usr/libexec"]);
        }
        if std::path::Path::new("/lib64").exists() {
            command.args(["--ro-bind", "/lib64", "/lib64"]);
        }
        command
            .arg("--ro-bind")
            .arg(&library)
            .arg("/compiler/lib")
            .arg("--ro-bind")
            .arg(rustc)
            .arg("/compiler/bin/rustc")
            .arg("--ro-bind")
            .arg(&source_path)
            .arg("/source.rs")
            .arg("--bind")
            .arg(output.path())
            .arg("/out")
            .args([
                "--chdir",
                "/out",
                "--",
                "/compiler/bin/rustc",
                "--sysroot=/compiler",
            ])
            .args(FLAGS)
            .arg(format!("-Clinker={}", linker.display()))
            .args(["/source.rs", "-o", "/out/plugin"]);
        #[cfg(target_os = "linux")]
        unsafe {
            command.pre_exec(|| {
                #[repr(C)]
                struct Limit {
                    current: u64,
                    maximum: u64,
                }
                unsafe extern "C" {
                    fn setrlimit(resource: u32, limit: *const Limit) -> i32;
                }
                for (resource, value) in [
                    (0, 10),
                    (1, 16 * 1024 * 1024),
                    (4, 0),
                    (7, 64),
                    (9, 2 * 1024 * 1024 * 1024),
                ] {
                    if setrlimit(
                        resource,
                        &Limit {
                            current: value,
                            maximum: value,
                        },
                    ) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| fail("Rust隔离编译启动失败"))?;
        let compile_deadline = deadline.min(Instant::now() + Duration::from_secs(20));
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| fail("编译诊断管道不可用"))?;
        let (status, (diagnostics, truncated)) = guarded(
            async {
                tokio::try_join!(
                    async { child.wait().await.map_err(fail) },
                    read_diagnostics(stderr)
                )
            },
            context,
            compile_deadline,
        )
        .await?;
        if !status.success() {
            #[cfg(unix)]
            let signal = {
                use std::os::unix::process::ExitStatusExt;
                status.signal()
            };
            #[cfg(not(unix))]
            let signal = None;
            return Ok(RustPreparation::CompilationFailed(CompilationFailure {
                language: Language::Rust,
                exit_code: status.code(),
                signal,
                diagnostics,
                truncated,
            }));
        }
        let binary = output.path().join("plugin");
        let hash = artifact_hash(&binary)?;
        let compiled = Arc::new(CompiledRust {
            _directory: output,
            binary,
            hash,
            last_used: AtomicU64::new(CACHE_CLOCK.fetch_add(1, Ordering::Relaxed)),
        });
        check_stop(context, deadline)?;
        let mut cache = self.rust_cache.lock().map_err(fail)?;
        // 只在新产物已成功验证后回收未被调用持有的缓存；失败编译保留旧产物。
        make_cache_room(&mut cache)?;
        let entry = cache.entry(key).or_insert(compiled).clone();
        entry.verify()?;
        entry.touch();
        Ok(RustPreparation::Ready(entry))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use eve_llm_api::SharedToolSteps;
    #[tokio::test]
    async fn global_compiler_queue_obeys_cancellation_and_deadline_without_spending_steps() {
        let _occupied = COMPILE_SLOTS.acquire().await.unwrap();
        // 只测试准入；即使配置的可执行文件存在，也不得越过已占用的全局编译槽。
        let mut sandbox =
            crate::Bubblewrap::new("/usr/bin/true", "/usr/bin/true").with_rustc("/usr/bin/true");
        sandbox.bwrap = "/usr/bin/true".into();
        let steps = SharedToolSteps::new(1).unwrap();
        let context = ToolExecutionContext::new().with_shared_steps(steps.clone());
        let result = sandbox
            .prepare_rust(
                "fn main() {}",
                &context,
                Instant::now() + Duration::from_millis(25),
            )
            .await;
        assert!(matches!(
            result,
            Err(eve_llm_api::ToolExecutionError::Cancelled(_))
        ));
        assert_eq!(steps.remaining().unwrap(), 1);
        let cancellation = context.cancellation_handle();
        let cancel = async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            cancellation.cancel();
        };
        let (result, ()) = tokio::join!(
            sandbox.prepare_rust(
                "fn main() {}",
                &context,
                Instant::now() + Duration::from_secs(1)
            ),
            cancel
        );
        assert!(matches!(
            result,
            Err(eve_llm_api::ToolExecutionError::Cancelled(_))
        ));
        assert_eq!(steps.remaining().unwrap(), 1);
        assert!(sandbox.rust_cache.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn fully_pinned_cache_refuses_compilation_before_spending_the_original_budget() {
        let output = std::process::Command::new("rustc")
            .args(["--print", "sysroot"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let root = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
        let sandbox = crate::Bubblewrap::new("/usr/bin/python3", "/usr/bin/node")
            .with_rustc(root.join("bin/rustc"));
        let mut pinned = Vec::new();
        {
            let mut cache = sandbox.rust_cache.lock().unwrap();
            for index in 0..MAX_RUST_CACHE {
                let entry = super::cache_tests::artifact(&format!("pinned-{index}"));
                pinned.push(entry.clone());
                cache.insert(format!("{index:02}"), entry);
            }
        }
        let steps = SharedToolSteps::new(1).unwrap();
        let context = ToolExecutionContext::new().with_shared_steps(steps.clone());
        let error = sandbox
            .prepare_rust(
                "fn main() {}",
                &context,
                Instant::now() + Duration::from_secs(10),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            eve_llm_api::ToolExecutionError::Rejected(_)
        ));
        assert_eq!(steps.remaining().unwrap(), 1);
        assert_eq!(sandbox.rust_cache.lock().unwrap().len(), MAX_RUST_CACHE);
        assert!(pinned.iter().all(|entry| entry.binary.exists()));
    }
    #[tokio::test]
    async fn compiled_rust_cache_rejects_modified_artifact_without_recompiling() {
        let output = std::process::Command::new("rustc")
            .args(["--print", "sysroot"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let root = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
        let sandbox = crate::Bubblewrap::new("/usr/bin/python3", "/usr/bin/node")
            .with_rustc(root.join("bin/rustc"));
        let context =
            ToolExecutionContext::new().with_shared_steps(SharedToolSteps::new(1).unwrap());
        let compiled = sandbox
            .prepare_rust(
                include_str!("../examples/plugins/hello.rs"),
                &context,
                Instant::now() + Duration::from_secs(20),
            )
            .await
            .unwrap();
        std::fs::write(&compiled.binary, b"modified binary").unwrap();
        assert!(
            sandbox
                .prepare_rust(
                    include_str!("../examples/plugins/hello.rs"),
                    &context,
                    Instant::now() + Duration::from_secs(20)
                )
                .await
                .is_err()
        );
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    pub(super) fn artifact(text: &str) -> Arc<CompiledRust> {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("plugin");
        std::fs::write(&binary, text).unwrap();
        let hash = artifact_hash(&binary).unwrap();
        Arc::new(CompiledRust {
            _directory: directory,
            binary,
            hash,
            last_used: AtomicU64::new(CACHE_CLOCK.fetch_add(1, Ordering::Relaxed)),
        })
    }
    #[test]
    fn least_recent_unused_artifact_is_deleted_but_pinned_artifact_survives_until_release() {
        let mut cache = BTreeMap::new();
        for index in 0..MAX_RUST_CACHE {
            cache.insert(format!("{index:02}"), artifact(&format!("binary-{index}")));
        }
        let pinned = cache["00"].clone();
        let first = pinned.binary.clone();
        let second = cache["01"].binary.clone();
        make_cache_room(&mut cache).unwrap();
        assert_eq!(cache.len(), MAX_RUST_CACHE - 1);
        assert!(!second.exists());
        assert!(first.exists());
        pinned.verify().unwrap();
        cache.insert("32".into(), artifact("binary-32"));
        cache["02"].touch();
        let third = cache["03"].binary.clone();
        make_cache_room(&mut cache).unwrap();
        assert!(!third.exists());
        assert!(cache.contains_key("02"));
        assert!(first.exists());
        cache.insert("33".into(), artifact("binary-33"));
        drop(pinned);
        make_cache_room(&mut cache).unwrap();
        assert!(!first.exists());
        assert!(!cache.contains_key("00"));
    }
    #[test]
    fn fully_pinned_cache_is_refused_without_dropping_any_live_file() {
        let mut cache = BTreeMap::new();
        let mut pinned = Vec::new();
        for index in 0..MAX_RUST_CACHE {
            let entry = artifact(&format!("live-{index}"));
            pinned.push(entry.clone());
            cache.insert(format!("{index:02}"), entry);
        }
        assert!(!has_cache_room(&cache));
        assert!(make_cache_room(&mut cache).is_err());
        assert_eq!(cache.len(), MAX_RUST_CACHE);
        assert!(pinned.iter().all(|artifact| artifact.binary.exists()));
        drop(pinned);
        assert!(has_cache_room(&cache));
        make_cache_room(&mut cache).unwrap();
        assert_eq!(cache.len(), MAX_RUST_CACHE - 1);
    }
    #[test]
    fn artifact_hash_is_bounded_and_rejects_changed_files_without_erasing_them() {
        let entry = artifact("abc");
        assert_eq!(
            entry.hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        std::fs::write(&entry.binary, b"changed").unwrap();
        assert!(entry.verify().is_err());
        assert_eq!(std::fs::read(&entry.binary).unwrap(), b"changed");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&entry.binary)
            .unwrap()
            .set_len(MAX_RUST_ARTIFACT_BYTES + 1)
            .unwrap();
        assert!(entry.verify().is_err());
        assert_eq!(
            std::fs::metadata(&entry.binary).unwrap().len(),
            MAX_RUST_ARTIFACT_BYTES + 1
        );
    }
    #[cfg(unix)]
    #[test]
    fn cached_artifact_symlink_is_not_executed_or_followed() {
        let entry = artifact("original");
        let target = entry._directory.path().join("target");
        std::fs::write(&target, b"original").unwrap();
        std::fs::remove_file(&entry.binary).unwrap();
        std::os::unix::fs::symlink(&target, &entry.binary).unwrap();
        assert!(entry.verify().is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
    }
}
