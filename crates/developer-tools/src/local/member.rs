//! 成员项目目录与 Linux bubblewrap 命令隔离；不挂载宿主状态或继承凭据环境。
use super::*;

impl LocalDeveloper {
    pub(super) fn member_operation(
        &self,
        op: &DeveloperOperation,
        context: &ToolExecutionContext,
    ) -> bool {
        !self.isolated
            && self.member_runtime.is_some()
            && self.authorize(context).is_err()
            && context.scope().is_some_and(|scope| {
                self.authorization
                    .as_ref()
                    .is_some_and(|a| a.allows_workspace_development(scope))
            })
            && matches!(
                op,
                DeveloperOperation::List(_)
                    | DeveloperOperation::Read(_)
                    | DeveloperOperation::Search(_)
                    | DeveloperOperation::Write(_)
                    | DeveloperOperation::Edit(_)
                    | DeveloperOperation::Command(_)
            )
            && !context.is_cancel_requested()
    }
    pub(super) fn member_backend(
        &self,
        context: &ToolExecutionContext,
    ) -> Result<Self, ToolExecutionError> {
        let scope = context.scope().ok_or_else(|| failed("缺少成员身份"))?;
        let root = member_workspace(&self.root, scope);
        for dir in [
            root.parent().expect("成员目录有父路径").to_path_buf(),
            root.clone(),
            root.join("exports"),
            root.join(".home"),
            root.join(".eve-bin"),
        ] {
            match fs::create_dir(&dir) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(failed("成员项目目录无法创建")),
            }
            if !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
                || dir.canonicalize().ok().as_ref() != Some(&dir)
            {
                return Err(failed("成员目录必须是工作区内的真实目录，不能使用链接"));
            }
        }
        // 某些 JRE 有编译器模块但缺少 javac/jar 启动入口；只在成员目录补入口。
        #[cfg(unix)]
        for (name, module) in [
            ("javac", "jdk.compiler/com.sun.tools.javac.Main"),
            ("jar", "jdk.jartool/sun.tools.jar.Main"),
        ] {
            use std::os::unix::fs::OpenOptionsExt;
            let path = root.join(".eve-bin").join(name);
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o700)
                .open(path)
            {
                Ok(mut file) => file
                    .write_all(format!("#!/bin/sh\nexec java -m {module} \"$@\"\n").as_bytes())
                    .map_err(|_| failed("成员 Java 工具入口无法保存"))?,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(failed("成员 Java 工具入口无法创建")),
            }
        }
        Ok(Self {
            config: self.config.clone(),
            client: self.client.clone(),
            token: None,
            root,
            journal: self.journal.clone(),
            next: AtomicU64::new(0),
            serial: Mutex::new(()),
            api: self.api.clone(),
            authorization: self.authorization.clone(),
            member_runtime: self.member_runtime.clone(),
            isolated: true,
            command_policy: self.command_policy.clone(),
            action_reviewer: self.action_reviewer.clone(),
        })
    }
    pub(super) fn member_command(
        &self,
        a: &CommandArguments,
        cwd: &Path,
    ) -> Result<Command, ToolExecutionError> {
        if !cfg!(target_os = "linux") || !Path::new("/usr/bin/bwrap").is_file() {
            return Err(failed(
                "成员编译需要 Linux bubblewrap；隔离不可用时不回退宿主命令",
            ));
        }
        let mut command = Command::new("/usr/bin/bwrap");
        command.args([
            "--unshare-all",
            "--share-net",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
        ]);
        for path in [
            "/usr",
            "/bin",
            "/sbin",
            "/lib",
            "/lib64",
            "/etc/ssl/certs",
            "/etc/alternatives",
            "/etc/resolv.conf",
            "/etc/hosts",
            "/etc/nsswitch.conf",
            "/etc/passwd",
            "/etc/group",
            "/etc/java-8-openjdk",
            "/etc/java-11-openjdk",
            "/etc/java-17-openjdk",
            "/etc/java-21-openjdk",
            "/etc/java-25-openjdk",
        ] {
            if Path::new(path).exists() {
                command.args(["--ro-bind", path, path]);
            }
        }
        command
            .args([
                "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--bind",
            ])
            .arg(&self.root)
            .arg("/work");
        let runtime = self.member_runtime.as_ref().expect("隔离后端具有运行资源");
        if let Some(java) = &runtime.java_home {
            command.arg("--ro-bind").arg(java).arg("/opt/eve/jdk");
        }
        if let Some(jar) = &runtime.mindustry_server {
            command
                .arg("--ro-bind")
                .arg(jar)
                .arg("/opt/eve/mindustry-server.jar");
        }
        // 本机代理所需公共 CA 只读挂载；不读取或继承任何凭据变量。
        for name in ["SSL_CERT_FILE", "NODE_EXTRA_CA_CERTS", "GIT_SSL_CAINFO"] {
            if let Some(path) = std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute() && p.is_file())
            {
                command.arg("--ro-bind").arg(&path).arg(&path);
            }
        }
        let relative = cwd
            .strip_prefix(&self.root)
            .map_err(|_| failed("命令目录越过成员工作区"))?;
        command
            .arg("--chdir")
            .arg(Path::new("/work").join(relative))
            .arg("--")
            .arg(&a.program)
            .args(&a.args);
        Ok(command)
    }
    pub(super) fn member_environment(&self, command: &mut Command) {
        command
            .env(
                "PATH",
                "/work/.eve-bin:/opt/eve/jdk/bin:/usr/local/bin:/usr/bin:/bin",
            )
            .env("HOME", "/work/.home")
            .env("TMPDIR", "/tmp")
            .env("LANG", "C.UTF-8")
            .env("GRADLE_USER_HOME", "/work/.home/gradle");
        if self
            .member_runtime
            .as_ref()
            .is_some_and(|r| r.java_home.is_some())
        {
            command.env("JAVA_HOME", "/opt/eve/jdk");
        }
        for name in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
            "SSL_CERT_FILE",
            "NODE_EXTRA_CA_CERTS",
            "GIT_SSL_CAINFO",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
    }
}
