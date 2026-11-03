//! 会话私有检出与不可变升级快照；共享工作树、索引、分支及未提交内容只读。
use super::*;
use std::collections::BTreeSet;

pub(super) fn uses_workspace(operation: &DeveloperOperation) -> bool {
    matches!(
        operation,
        DeveloperOperation::List(_)
            | DeveloperOperation::Read(_)
            | DeveloperOperation::Search(_)
            | DeveloperOperation::Write(_)
            | DeveloperOperation::Edit(_)
            | DeveloperOperation::Patch(_)
            | DeveloperOperation::Command(_)
            | DeveloperOperation::PrepareUpgrade
    )
}

async fn git(
    root: &Path,
    args: &[&str],
    input: Option<&[u8]>,
) -> Result<String, ToolExecutionError> {
    let mut command = Command::new("git");
    command
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(root)
        .kill_on_drop(true)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Eve candidate snapshot")
        .env("GIT_AUTHOR_EMAIL", "eve-snapshot@localhost")
        .env("GIT_COMMITTER_NAME", "Eve candidate snapshot")
        .env("GIT_COMMITTER_EMAIL", "eve-snapshot@localhost")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|_| failed("会话源码 Git 无法启动"))?;
    if let Some(input) = input {
        use tokio::io::AsyncWriteExt;
        child
            .stdin
            .take()
            .ok_or_else(|| failed("Git 输入不可用"))?
            .write_all(input)
            .await
            .map_err(|_| failed("Git 输入失败"))?;
    }
    let output = child
        .stdout
        .take()
        .ok_or_else(|| failed("Git 输出不可用"))?;
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let (status, output) = tokio::join!(child.wait(), capture(output, 4 * 1024 * 1024));
        Ok::<_, ToolExecutionError>((status.map_err(|_| failed("会话源码 Git 失败"))?, output?))
    })
    .await
    .map_err(|_| failed("会话源码 Git 超时；原工作区保留"))??;
    if !result.0.success() || result.1.1 {
        return Err(failed("会话源码 Git 拒绝操作或输出超限；原工作区保留"));
    }
    String::from_utf8(result.1.0).map_err(|_| failed("源码路径不是完整 UTF-8"))
}

fn plain_directory(path: &Path) -> Result<(), ToolExecutionError> {
    fs::create_dir_all(path).map_err(|_| failed("会话源码目录无法创建"))?;
    if path.canonicalize().ok().as_deref() != Some(path)
        || !fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
    {
        return Err(failed("会话源码目录不能经符号链接跳转"));
    }
    Ok(())
}

impl LocalDeveloper {
    fn at_root(&self, root: PathBuf) -> Self {
        let mut config = self.config.clone();
        config.workspace = root.clone();
        Self {
            config,
            root,
            client: self.client.clone(),
            token: self.token.clone(),
            journal: self.journal.clone(),
            ledger: self.ledger.clone(),
            api: self.api.clone(),
            authorization: self.authorization.clone(),
            member_runtime: self.member_runtime.clone(),
            isolated: self.isolated,
            command_policy: self.command_policy.clone(),
            action_reviewer: self.action_reviewer.clone(),
            session_workspaces: false,
        }
    }

    pub(super) async fn session_backend(
        &self,
        context: &ToolExecutionContext,
    ) -> Result<Self, ToolExecutionError> {
        self.authorize(context)?;
        let scope = context
            .scope()
            .ok_or_else(|| failed("会话工作区缺少可信身份"))?;
        let owner = session_workspace_key(scope);
        let parent = self.journal.join("workspaces");
        plain_directory(&parent)?;
        let directory = parent.join(owner);
        let binding = json!({"version":1,"scope":scope,"source":self.root});
        if directory.exists() {
            plain_directory(&directory)?;
            let stored = live_upgrade::read_bounded(&directory.join("binding.json"), 8192)?;
            if serde_json::from_slice::<Value>(&stored).ok().as_ref() != Some(&binding) {
                return Err(failed("会话工作区绑定不匹配；原目录保留"));
            }
        } else {
            if fs::read_dir(&parent)
                .map_err(|_| failed("会话源码目录不可读"))?
                .count()
                >= 128
            {
                return Err(failed(
                    "会话源码工作区容量已满；保留已有会话，不自动清理未完成工作",
                ));
            }
            let stage =
                tempfile::tempdir_in(&parent).map_err(|_| failed("会话源码暂存目录无法创建"))?;
            let source = self.root.to_str().ok_or_else(|| failed("源码路径无效"))?;
            git(
                stage.path(),
                &[
                    "clone",
                    "--local",
                    "--no-hardlinks",
                    "--no-checkout",
                    "--template=",
                    "--",
                    source,
                    "source",
                ],
                None,
            )
            .await?;
            let root = stage.path().join("source");
            self.import_online_revision(&root).await?;
            // 新会话从当前在线提交起步；共享仓库未提交内容不复制。
            let head = match self.supervisor()?.1.active_source_revision {
                Some(revision) => revision,
                None => git(&root, &["rev-parse", "HEAD"], None).await?,
            };
            git(
                &root,
                &[
                    "checkout",
                    "-b",
                    &format!(
                        "codex/eve-session-{}",
                        &directory.file_name().unwrap().to_string_lossy()[..16]
                    ),
                    head.trim(),
                    "--",
                ],
                None,
            )
            .await?;
            fs::write(stage.path().join("binding.json"), binding.to_string())
                .map_err(|_| failed("会话源码绑定无法保存"))?;
            fs::rename(stage.path(), &directory)
                .map_err(|_| failed("会话源码发布失败；已有目录不覆盖"))?;
        }
        let root = directory.join("source");
        plain_directory(&root)?;
        self.import_online_revision(&root).await?;
        let actual = git(&root, &["rev-parse", "--show-toplevel"], None).await?;
        if Path::new(actual.trim()) != root {
            return Err(failed("会话 Git 根目录已改变"));
        }
        Ok(self.at_root(root))
    }

    /// 在线源码可能是上次升级的宿主快照；仅补齐该受信对象，不移动当前会话分支。
    async fn import_online_revision(&self, root: &Path) -> Result<(), ToolExecutionError> {
        let Some(revision) = self.supervisor()?.1.active_source_revision else {
            return Ok(());
        };
        if !valid_source_revision(&revision) {
            return Err(failed("在线源码提交无效"));
        }
        if git(
            root,
            &["cat-file", "-e", &format!("{revision}^{{commit}}")],
            None,
        )
        .await
        .is_ok()
        {
            return Ok(());
        }
        if root != self.root
            && git(
                &self.root,
                &["cat-file", "-e", &format!("{revision}^{{commit}}")],
                None,
            )
            .await
            .is_ok()
        {
            git(
                root,
                &[
                    "fetch",
                    "--no-tags",
                    "--",
                    self.root.to_str().ok_or_else(|| failed("源码路径无效"))?,
                    &revision,
                ],
                None,
            )
            .await?;
            return Ok(());
        }
        let directory = self.journal.join("source-snapshots");
        if directory.exists() {
            plain_directory(&directory)?;
            for entry in fs::read_dir(&directory)
                .map_err(|_| failed("源码快照目录无法读取"))?
                .take(256)
            {
                let entry = entry.map_err(|_| failed("源码快照目录无法读取"))?;
                if !entry
                    .file_type()
                    .map_err(|_| failed("源码快照类型不可读"))?
                    .is_dir()
                {
                    continue;
                }
                let path = entry.path().join("snapshot.json");
                let Some(binding) = live_upgrade::read_bounded(&path, 8192)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                else {
                    continue;
                };
                if binding["head"] != revision {
                    continue;
                }
                let source = entry.path().join("source");
                plain_directory(&source)?;
                git(
                    root,
                    &[
                        "fetch",
                        "--no-tags",
                        "--",
                        source.to_str().ok_or_else(|| failed("快照路径无效"))?,
                        &revision,
                    ],
                    None,
                )
                .await?;
                return Ok(());
            }
        }
        Err(failed(
            "在线源码对象不在可信本地仓库或快照中；保留当前会话工作区",
        ))
    }

    /// 只冻结此会话的源码；在独立候选仓库提交快照，不提交或修改任何会话的 HEAD/索引。
    pub(super) async fn upgrade_snapshot(
        &self,
        context: &ToolExecutionContext,
    ) -> Result<Self, ToolExecutionError> {
        self.authorize(context)?;
        self.import_online_revision(&self.root).await?;
        let online = self.supervisor()?.1.active_source_revision;
        let head = git(&self.root, &["rev-parse", "HEAD"], None).await?;
        let tracked = git(&self.root, &["ls-files", "-v", "-z"], None).await?;
        if tracked
            .split('\0')
            .filter(|s| !s.is_empty())
            .any(|s| !s.starts_with("H "))
        {
            return Err(failed("会话索引冲突或隐藏改动；保留原状态"));
        }
        let dirty = git(
            &self.root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--name-only",
                "-z",
                "HEAD",
                "--",
            ],
            None,
        )
        .await?;
        let untracked = git(
            &self.root,
            &["ls-files", "--others", "--exclude-standard", "-z"],
            None,
        )
        .await?;
        let paths: BTreeSet<_> = dirty
            .split('\0')
            .chain(untracked.split('\0'))
            .filter(|p| !p.is_empty())
            .collect();
        if paths.len() > 4096 {
            return Err(failed("会话候选文件过多；未裁剪或发布"));
        }
        let mut files = Vec::new();
        let mut total = 0usize;
        for relative in paths {
            if review::credential_path(relative) {
                return Err(failed("会话候选涉及凭据或身份账本；未读取正文"));
            }
            let path = self.path(relative, true)?;
            let (bytes, executable) = match fs::symlink_metadata(&path) {
                Ok(meta)
                    if meta.is_file()
                        && !meta.file_type().is_symlink()
                        && meta.len() <= MAX_FILE as u64 =>
                {
                    let bytes = live_upgrade::read_bounded_scoped(&path, MAX_FILE, Some(context))?;
                    self.reject_evidence_credentials(
                        std::str::from_utf8(&bytes)
                            .map_err(|_| failed("源码变更含不可审核的二进制"))?,
                    )?;
                    #[cfg(unix)]
                    let executable = {
                        use std::os::unix::fs::PermissionsExt;
                        meta.permissions().mode() & 0o111 != 0
                    };
                    #[cfg(not(unix))]
                    let executable = false;
                    (Some(bytes), executable)
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, false),
                _ => return Err(failed("会话候选含链接、特殊文件或超限文件；原状态保留")),
            };
            total += bytes.as_ref().map_or(0, Vec::len);
            if total > 16 * 1024 * 1024 {
                return Err(failed("会话候选改动超过容量"));
            }
            files.push((relative.to_owned(), bytes, executable));
        }
        let key = hash(
            &serde_json::to_vec(&(head.trim(), &online, &files))
                .map_err(|_| failed("候选快照编码失败"))?,
        );
        let parent = self.journal.join("source-snapshots");
        plain_directory(&parent)?;
        let directory = parent.join(&key);
        if !directory.exists() {
            if fs::read_dir(&parent)
                .map_err(|_| failed("源码快照目录不可读"))?
                .count()
                >= 256
            {
                return Err(failed("源码快照容量已满；保留候选和会话，未删除其他工作"));
            }
            let stage = tempfile::tempdir_in(&parent).map_err(|_| failed("源码快照暂存失败"))?;
            git(
                stage.path(),
                &[
                    "clone",
                    "--local",
                    "--no-hardlinks",
                    "--no-checkout",
                    "--template=",
                    "--",
                    self.root.to_str().ok_or_else(|| failed("源码路径无效"))?,
                    "source",
                ],
                None,
            )
            .await?;
            let root = stage.path().join("source");
            git(&root, &["checkout", "--detach", head.trim(), "--"], None).await?;
            let backend = self.at_root(root.clone());
            for (relative, bytes, _executable) in &files {
                let path = backend.path(relative, true)?;
                match bytes {
                    Some(bytes) => {
                        fs::create_dir_all(
                            path.parent().ok_or_else(|| failed("候选路径无父目录"))?,
                        )
                        .map_err(|_| failed("候选子目录无法创建"))?;
                        fs::write(&path, bytes).map_err(|_| failed("候选源码写入失败"))?;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            fs::set_permissions(
                                &path,
                                fs::Permissions::from_mode(if *_executable { 0o755 } else { 0o644 }),
                            )
                            .map_err(|_| failed("候选源码权限无法保存"))?;
                        }
                    }
                    None => {
                        if path.exists() {
                            fs::remove_file(path).map_err(|_| failed("候选文件删除失败"))?;
                        }
                    }
                }
            }
            git(&root, &["add", "--all", "--"], None).await?;
            if !git(&root, &["diff", "--cached", "--name-only"], None)
                .await?
                .is_empty()
            {
                git(&root, &["commit", "-m", "冻结当前会话的升级源码快照"], None).await?;
            }
            // 旧会话不覆盖其他会话已上线的修复。在候选私有目录做真实三方合并，
            // 冲突返回具体文件供本会话处理，原会话目录和索引保持原状。
            if let Some(online) = &online {
                match git(&root, &["merge", "--no-edit", "--no-ff", online], None).await {
                    Ok(_) => {}
                    Err(_) => {
                        let paths = git(
                            &root,
                            &["diff", "--name-only", "--diff-filter=U", "-z"],
                            None,
                        )
                        .await?;
                        return Err(failed(format!(
                            "当前会话与在线修复存在候选合并冲突；未覆盖任何工作区。冲突文件：{}",
                            paths.replace('\0', "，")
                        )));
                    }
                }
            }
            let revision = git(&root, &["rev-parse", "HEAD"], None).await?;
            fs::write(stage.path().join("snapshot.json"), json!({"version":1,"key":key,"base":head.trim(),"online":online,"head":revision.trim()}).to_string()).map_err(|_| failed("候选源码绑定无法保存"))?;
            fs::rename(stage.path(), &directory).map_err(|_| failed("源码快照发布失败"))?;
        }
        // 复制期间的变更不能被误说成已冻结；不锁住或覆盖用户继续编辑的内容。
        if git(&self.root, &["rev-parse", "HEAD"], None).await? != head
            || git(
                &self.root,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--name-only",
                    "-z",
                    "HEAD",
                    "--",
                ],
                None,
            )
            .await?
                != dirty
            || git(
                &self.root,
                &["ls-files", "--others", "--exclude-standard", "-z"],
                None,
            )
            .await?
                != untracked
            || self.supervisor()?.1.active_source_revision != online
            || files.iter().any(|(relative, bytes, executable)| {
                !unchanged_file(&self.root.join(relative), bytes.as_deref(), *executable)
            })
        {
            return Err(failed("冻结时当前会话源码继续变化；原文件保留，请重试准备"));
        }
        plain_directory(&directory)?;
        let backend = self.at_root(directory.join("source"));
        plain_directory(&backend.root)?;
        let binding: Value = serde_json::from_slice(&live_upgrade::read_bounded(
            &directory.join("snapshot.json"),
            8192,
        )?)
        .map_err(|_| failed("源码快照绑定损坏"))?;
        let revision = git(&backend.root, &["rev-parse", "HEAD"], None).await?;
        if binding
            != json!({"version":1,"key":key,"base":head.trim(),"online":online,"head":revision.trim()})
        {
            return Err(failed("源码快照提交或绑定已改变；原内容保留"));
        }
        if !git(
            &backend.root,
            &["status", "--porcelain=v1", "--untracked-files=all"],
            None,
        )
        .await?
        .is_empty()
        {
            return Err(failed("既有源码快照已被修改；不覆盖"));
        }
        Ok(backend)
    }
}

fn unchanged_file(path: &Path, expected: Option<&[u8]>, executable: bool) -> bool {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return expected.is_none(),
        _ => return false,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if (metadata.permissions().mode() & 0o111 != 0) != executable {
            return false;
        }
    }
    #[cfg(not(unix))]
    let _ = (metadata, executable);
    fs::read(path).ok().as_deref() == expected
}
