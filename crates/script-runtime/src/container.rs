//! 可信组合层选择的 Linux 容器后端；容器内绝不暴露 daemon socket。
use crate::{Language, PublishedVersion, Result, fail, reject};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::Command;

/// image 必须为宿主预装的不可变 sha256 镜像ID，不能来自模型参数。
#[derive(Clone, Debug)]
pub struct ContainerSandbox {
    pub cli: PathBuf,
    pub image: String,
}
impl ContainerSandbox {
    pub(crate) fn preflight(&self, language: &Language) -> Result<()> {
        if language.is_rust() {
            return Err(reject("当前容器后端没有可信Rust编译器，拒绝降级"));
        }
        if !self.cli.is_absolute()
            || !self.cli.is_file()
            || !self.image.strip_prefix("sha256:").is_some_and(|digest| {
                digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
            })
        {
            return Err(reject(
                "容器隔离需要可信绝对CLI路径与本地不可变sha256镜像ID",
            ));
        }
        Ok(())
    }
    pub(crate) fn spawn(
        &self,
        version: &PublishedVersion,
        source: &Path,
        work: &Path,
    ) -> Result<crate::sandbox::SandboxChild> {
        self.preflight(&version.spec.language)?;
        let mut nonce = [0u8; 16];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut nonce)
            .map_err(|_| fail("容器名称随机源不可用"))?;
        let id = format!(
            "eve-script-{}",
            nonce.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let mut command = Command::new(&self.cli);
        command.args([
            "run",
            "--rm",
            "--pull=never",
            "--name",
            &id,
            "-i",
            "--network=none",
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--pids-limit=32",
            "--memory=256m",
            "--memory-swap=256m",
            "--cpus=1",
            "--ulimit=cpu=3:3",
            "--ulimit=nofile=64:64",
            "--ulimit=fsize=1048576:1048576",
            "--ulimit=core=0:0",
            "--tmpfs=/tmp:rw,noexec,nosuid,size=16m",
            "--workdir=/work",
        ]);
        command.arg("--user=65534:65534");
        let staging = tempfile::tempdir().map_err(fail)?;
        let staged_source = staging.path().join("entry");
        std::fs::copy(source, &staged_source).map_err(fail)?;
        readable(&staged_source, false)?;
        let staged_work = staging.path().join("work");
        std::fs::create_dir(&staged_work).map_err(fail)?;
        readable(&staged_work, true)?;
        copy_work(work, &staged_work)?;
        for (host, guest) in [
            (staged_source.as_path(), "/entry"),
            (staged_work.as_path(), "/work"),
        ] {
            let host = host
                .to_str()
                .ok_or_else(|| reject("容器挂载路径不是UTF-8"))?;
            if host.contains(',') {
                return Err(reject("容器挂载路径包含分隔符"));
            }
            command
                .arg("--mount")
                .arg(format!("type=bind,source={host},target={guest},readonly"));
        }
        // 固定入口不允许镜像自定义ENTRYPOINT。timeout保证宿主掉线时进程也有独立硬期限。
        command.args([
            "--entrypoint=/usr/bin/env",
            &self.image,
            "-i",
            "HOME=/tmp",
            "LANG=C.UTF-8",
            "PATH=/usr/bin",
            "/usr/bin/timeout",
            "--signal=KILL",
            "30",
        ]);
        match version.spec.language {
            Language::Rust | Language::RustCargo(_) => {
                return Err(reject("容器Rust运行后端尚未实现，拒绝降级"));
            }
            Language::Python => {
                command.args(["/usr/bin/python3", "-I", "-B", "-u", "/entry"]);
            }
            Language::Javascript => {
                command.args([
                    "/usr/bin/node",
                    "--no-addons",
                    "--jitless",
                    "--max-old-space-size=64",
                    "/entry",
                ]);
            }
        }
        let child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| fail("容器隔离启动失败，未降级exec"))?;
        Ok(crate::sandbox::SandboxChild {
            child,
            container: Some((self.cli.clone(), id)),
            staging: Some(staging),
        })
    }
}

fn readable(path: &Path, directory: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            path,
            std::fs::Permissions::from_mode(if directory { 0o755 } else { 0o644 }),
        )
        .map_err(fail)?;
    }
    #[cfg(not(unix))]
    let _ = (path, directory);
    Ok(())
}
pub(crate) fn copy_work(source: &Path, target: &Path) -> Result<()> {
    let mut count = 0usize;
    let mut total = 0u64;
    for entry in std::fs::read_dir(source).map_err(fail)? {
        let entry = entry.map_err(fail)?;
        let metadata = std::fs::symlink_metadata(entry.path()).map_err(fail)?;
        count += 1;
        total = total.saturating_add(metadata.len());
        if !metadata.is_file() || count > 64 || total > 8 * 1024 * 1024 {
            return Err(reject("容器共享目录不是有界普通文件"));
        }
        let path = target.join(entry.file_name());
        std::fs::copy(entry.path(), &path).map_err(fail)?;
        readable(&path, false)?;
    }
    Ok(())
}
