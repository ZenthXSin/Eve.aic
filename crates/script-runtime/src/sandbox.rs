use crate::{Language, PublishedVersion, Result, reject};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::{Child, Command};

/// Linux bubblewrap 后端。可替换运行时路径由可信装配提供，不能来自脚本。
/// 无宿主根目录、网络、凭据环境；/work 只读，写共享文件须用有界宿主 RPC。
#[derive(Clone, Debug)]
pub struct Bubblewrap {
    pub bwrap: PathBuf,
    pub python: PathBuf,
    pub node: PathBuf,
    pub(crate) container: Option<crate::ContainerSandbox>,
    pub(crate) rustc: Option<PathBuf>,
    pub(crate) rust_cache: std::sync::Arc<
        std::sync::Mutex<
            std::collections::BTreeMap<String, std::sync::Arc<crate::compiler::CompiledRust>>,
        >,
    >,
}
impl Bubblewrap {
    pub fn new(python: impl Into<PathBuf>, node: impl Into<PathBuf>) -> Self {
        Self {
            bwrap: "/usr/bin/bwrap".into(),
            python: python.into(),
            node: node.into(),
            container: None,
            rustc: None,
            rust_cache: Default::default(),
        }
    }
    pub fn with_rustc(mut self, rustc: impl Into<PathBuf>) -> Self {
        self.rustc = Some(rustc.into());
        self.rust_cache = Default::default();
        self
    }
    /// 只能由可信宿主配置；参数不经过插件/模型输入。
    pub fn with_container(mut self, container: crate::ContainerSandbox) -> Self {
        self.container = Some(container);
        self
    }
    pub(crate) fn description(&self) -> &'static str {
        if self.container.is_some() {
            "linux-container-required; daemon and immutable image checked at launch"
        } else {
            "bubblewrap-required; namespace availability checked at launch"
        }
    }
    pub(crate) fn preflight(&self, language: &Language) -> Result<()> {
        if let Some(container) = &self.container {
            return container.preflight(language);
        }
        if !cfg!(target_os = "linux") {
            return Err(reject("缺少 Linux namespace 隔离，拒绝原生 exec 降级"));
        }
        if !cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
            return Err(reject("当前架构缺少已核对的 seccomp 规则，拒绝启动"));
        }
        let executable = match language {
            Language::Python => &self.python,
            Language::Javascript => &self.node,
            Language::Rust => self
                .rustc
                .as_ref()
                .ok_or_else(|| reject("未配置可信Rust编译器"))?,
        };
        for path in [&self.bwrap, executable] {
            if !path.is_absolute() || !path.is_file() {
                return Err(reject("隔离器或语言运行时不可用，未启动脚本"));
            }
        }
        Ok(())
    }
    pub(crate) fn spawn(
        &self,
        version: &PublishedVersion,
        source: &Path,
        work: &Path,
    ) -> Result<SandboxChild> {
        if let Some(container) = &self.container {
            return container.spawn(version, source, work);
        }
        self.preflight(&version.spec.language)?;
        let mut command = Command::new(&self.bwrap);
        #[cfg(target_os = "linux")]
        let filter = syscall_filter()?;
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let filter_fd = filter.as_file().as_raw_fd();
            command.arg("--seccomp").arg(filter_fd.to_string());
            // pre_exec 只调用 async-signal-safe libc 函数，不分配、不加锁。
            unsafe {
                command.pre_exec(move || {
                    #[repr(C)]
                    struct Limit {
                        current: u64,
                        maximum: u64,
                    }
                    unsafe extern "C" {
                        fn fcntl(fd: i32, command: i32, value: i32) -> i32;
                        fn setrlimit(resource: u32, limit: *const Limit) -> i32;
                    }
                    // 仅 seccomp fd 需跨 exec 交给 bwrap，其余宿主 fd 默认 CLOEXEC。
                    if fcntl(filter_fd, 2, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    for (resource, value) in [
                        (0, 3),
                        (1, 1024 * 1024),
                        (4, 0),
                        (7, 64),
                        (9, 1024 * 1024 * 1024),
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
        }
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
        ]);
        if Path::new("/lib64").exists() {
            command.args(["--ro-bind", "/lib64", "/lib64"]);
        }
        command
            .args([
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
                "--ro-bind",
            ])
            .arg(work)
            .arg("/work")
            .arg("--ro-bind")
            .arg(source)
            .arg("/entry")
            .args(["--chdir", "/work"]);
        match version.spec.language {
            Language::Rust => {
                command.args(["--", "/entry"]);
            }
            Language::Python => {
                // 解释器来自可信PATH/配置；同时只读挂载其安装前缀的lib，支持非/usr的CI Python。
                let library = self
                    .python
                    .parent()
                    .and_then(Path::parent)
                    .map(|prefix| prefix.join("lib"))
                    .filter(|path| path.is_dir())
                    .ok_or_else(|| reject("Python安装前缀缺少lib，拒绝不完整运行时"))?;
                command
                    .arg("--ro-bind")
                    .arg(library)
                    .arg("/runtime/lib")
                    .arg("--ro-bind")
                    .arg(&self.python)
                    .arg("/runtime/bin/python")
                    .args(["--", "/runtime/bin/python", "-I", "-B", "-u", "/entry"]);
            }
            Language::Javascript => {
                command
                    .arg("--ro-bind")
                    .arg(&self.node)
                    .arg("/runtime/node")
                    .args([
                        "--",
                        "/runtime/node",
                        "--no-addons",
                        "--jitless",
                        "--max-old-space-size=64",
                        "--max-semi-space-size=4",
                        "/entry",
                    ]);
            }
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        // 子进程只继承 stdin/stdout；bwrap 的 PID namespace 在退出时杀死全部后代。
        command
            .spawn()
            .map(|child| SandboxChild {
                child,
                container: None,
                staging: None,
            })
            .map_err(|_| reject("隔离进程启动失败，未降级到宿主 exec"))
    }
}

/// 拒绝新进程、额外 namespace 和常见跨进程/内核访问；允许 Node 运行时线程。
/// 这是 namespace 之上的附加规则，不是通用 syscall allowlist。
#[cfg(target_os = "linux")]
fn syscall_filter() -> Result<tempfile::NamedTempFile> {
    let (arch, clone, denied): (u32, u32, &[u32]) = if cfg!(target_arch = "x86_64") {
        (
            0xc000003e,
            56,
            &[
                57, 58, 435, 101, 165, 166, 155, 248, 249, 250, 272, 298, 304, 308, 310, 311, 321,
                323,
            ],
        )
    } else if cfg!(target_arch = "aarch64") {
        (
            0xc00000b7,
            220,
            &[
                435, 117, 40, 39, 41, 217, 218, 219, 97, 241, 265, 268, 270, 271, 280, 282,
            ],
        )
    } else {
        return Err(reject("不支持当前 syscall 架构"));
    };
    let mut instructions = vec![
        (0x20u16, 0u8, 0u8, 4u32), // seccomp_data.arch
        (0x15, 1, 0, arch),
        (0x06, 0, 0, 0x80000000), // 错误架构终止
        (0x20, 0, 0, 0),
        (0x45, 0, 1, 0x40000000), // 拒绝 x32 ABI
        (0x06, 0, 0, 0x80000000),
        (0x15, 0, 3, clone),
        (0x20, 0, 0, 16),         // clone args[0] flags
        (0x45, 1, 0, 0x10000),    // CLONE_THREAD
        (0x06, 0, 0, 0x00050001), // errno EPERM
        (0x20, 0, 0, 0),
    ];
    for syscall in denied {
        instructions.push((0x15, 0, 1, *syscall));
        // glibc 在 ENOSYS 时回退到可检查 flags 的 clone；EPERM 会阻止 Node 创建线程。
        instructions.push((
            0x06,
            0,
            0,
            if *syscall == 435 {
                0x00050026
            } else {
                0x00050001
            },
        ));
    }
    instructions.push((0x06, 0, 0, 0x7fff0000));
    let mut file = tempfile::NamedTempFile::new().map_err(reject)?;
    use std::io::Write;
    for (code, jt, jf, k) in instructions {
        file.write_all(&code.to_ne_bytes()).map_err(reject)?;
        file.write_all(&[jt, jf]).map_err(reject)?;
        file.write_all(&k.to_ne_bytes()).map_err(reject)?;
    }
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0)).map_err(reject)?;
    Ok(file)
}

/// 宿主取消同时清理容器；即使daemon暂不可达，固定容器timeout仍在30秒内终止负载。
pub(crate) struct SandboxChild {
    pub(crate) child: Child,
    pub(crate) container: Option<(PathBuf, String)>,
    pub(crate) staging: Option<tempfile::TempDir>,
}
impl SandboxChild {
    pub(crate) fn sync_work(&self, work: &Path) -> Result<()> {
        if let Some(staging) = &self.staging {
            crate::container::copy_work(work, &staging.path().join("work"))?;
        }
        Ok(())
    }
}
impl std::ops::Deref for SandboxChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.child
    }
}
impl std::ops::DerefMut for SandboxChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}
impl Drop for SandboxChild {
    fn drop(&mut self) {
        if let Some((cli, id)) = self.container.take() {
            // 清理请求不包含用户可控参数；不等待daemon，不阻塞宿主执行器。
            if let Ok(mut cleanup) = std::process::Command::new(cli)
                .args(["rm", "--force", &id])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                std::thread::spawn(move || {
                    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    while std::time::Instant::now() < until {
                        if cleanup.try_wait().is_ok_and(|status| status.is_some()) {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    let _ = cleanup.kill();
                    let _ = cleanup.wait();
                });
            }
        }
    }
}
