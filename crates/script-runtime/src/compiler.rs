use crate::{
    Language, MAX_SOURCE_BYTES, Result, digest, fail, reject,
    runtime::{check_stop, guarded, reserve},
};
use eve_llm_api::ToolExecutionContext;
use std::{
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

const FLAGS: &[&str] = &[
    "--edition=2021",
    "--crate-name=eve_plugin",
    "--crate-type=bin",
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
}
impl CompiledRust {
    pub(crate) fn verify(&self) -> Result<()> {
        let bytes = std::fs::read(&self.binary).map_err(fail)?;
        if digest(&bytes)? != self.hash {
            return Err(reject("Rust插件缓存产物校验失败"));
        }
        Ok(())
    }
}
impl crate::Bubblewrap {
    pub(crate) async fn prepare_rust(
        &self,
        source: &str,
        context: &ToolExecutionContext,
        deadline: Instant,
    ) -> Result<Arc<CompiledRust>> {
        self.preflight(&Language::Rust)?;
        if self.container.is_some() {
            return Err(reject("当前Rust编译只支持可信Linux bubblewrap编译器"));
        }
        if source.is_empty() || source.len() > MAX_SOURCE_BYTES {
            return Err(reject("Rust源码无效或超限"));
        }
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
            return Ok(cached);
        }
        if self.rust_cache.lock().map_err(fail)?.len() >= 32 {
            return Err(reject("Rust插件缓存容量已满"));
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
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| fail("Rust隔离编译启动失败"))?;
        let compile_deadline = deadline.min(Instant::now() + Duration::from_secs(20));
        let status = guarded(
            async { child.wait().await.map_err(fail) },
            context,
            compile_deadline,
        )
        .await?;
        if !status.success() {
            return Err(reject("Rust隔离编译失败；旧版本保持可用"));
        }
        let binary = output.path().join("plugin");
        let metadata = std::fs::symlink_metadata(&binary).map_err(fail)?;
        if !metadata.is_file() || metadata.len() > 16 * 1024 * 1024 {
            return Err(reject("Rust编译产物无效"));
        }
        let hash = digest(&std::fs::read(&binary).map_err(fail)?)?;
        let compiled = Arc::new(CompiledRust {
            _directory: output,
            binary,
            hash,
        });
        check_stop(context, deadline)?;
        let mut cache = self.rust_cache.lock().map_err(fail)?;
        if cache.len() >= 32 {
            return Err(reject("Rust插件缓存容量已满"));
        }
        let entry = cache.entry(key).or_insert(compiled).clone();
        entry.verify()?;
        Ok(entry)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use eve_llm_api::SharedToolSteps;
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
