//! 固定 Agent 提示词的可替换实现；不依赖 Kernel、Runtime 或 Provider。
use eve_llm_api::{LlmError, SystemPromptOrigin, SystemPromptSnapshot, SystemPromptSource};
use ring::digest::{SHA256, digest};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const MAX_AGENT_PROMPT_BYTES: usize = 64 * 1024;

pub struct InlineAgentPrompt {
    text: String,
}

impl InlineAgentPrompt {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl SystemPromptSource for InlineAgentPrompt {
    fn load(&self) -> Result<SystemPromptSnapshot, LlmError> {
        snapshot(self.text.clone(), SystemPromptOrigin::Inline)
    }
}

#[derive(Clone, Debug)]
pub struct FileAgentPrompt {
    path: PathBuf,
}

impl FileAgentPrompt {
    /// 路径由组合层显式提供；相对路径在此刻解析，不依赖后续工作目录。
    pub fn new(path: impl AsRef<Path>) -> Result<Self, LlmError> {
        let path = path.as_ref();
        if path.as_os_str().is_empty() {
            return Err(configuration("AGENT 文件路径不能为空"));
        }
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("AGENTS.md"))
        {
            return Err(configuration(
                "AGENTS.md 是开发协作约定，不能作为 Agent 来源",
            ));
        }
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|_| configuration("无法解析 AGENT 文件路径"))?
                .join(path)
        };
        if path.to_str().is_none() {
            return Err(configuration("AGENT 文件路径必须能表示为 UTF-8"));
        }
        Ok(Self { path })
    }
}

impl SystemPromptSource for FileAgentPrompt {
    fn load(&self) -> Result<SystemPromptSnapshot, LlmError> {
        // 拒绝来源本身的链接和非普通文件；仅面向受信本地文件，不是操作系统沙箱。
        let metadata = fs::symlink_metadata(&self.path)
            .map_err(|_| configuration("AGENT 文件不存在或不可读"))?;
        if !metadata.file_type().is_file() {
            return Err(configuration("AGENT 来源必须是普通文件"));
        }
        if metadata.len() > MAX_AGENT_PROMPT_BYTES as u64 {
            return Err(configuration("AGENT 文件超过 65536 字节上限"));
        }
        let file = File::open(&self.path).map_err(|_| configuration("AGENT 文件读取失败"))?;
        if !file
            .metadata()
            .map_err(|_| configuration("AGENT 文件信息读取失败"))?
            .is_file()
        {
            return Err(configuration("AGENT 来源必须是普通文件"));
        }
        // 多读一个字节，发现读取期间增长也明确失败，不截断。
        let mut bytes = Vec::new();
        file.take(MAX_AGENT_PROMPT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| configuration("AGENT 文件读取失败"))?;
        if bytes.len() > MAX_AGENT_PROMPT_BYTES {
            return Err(configuration("AGENT 文件超过 65536 字节上限"));
        }
        let text =
            String::from_utf8(bytes).map_err(|_| configuration("AGENT 文件不是有效 UTF-8"))?;
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned();
        snapshot(
            text,
            SystemPromptOrigin::File {
                path: self.path.to_str().expect("构造时已验证 UTF-8 路径").into(),
            },
        )
    }
}

fn snapshot(text: String, origin: SystemPromptOrigin) -> Result<SystemPromptSnapshot, LlmError> {
    if text.len() > MAX_AGENT_PROMPT_BYTES {
        return Err(configuration("AGENT 内容超过 65536 字节上限"));
    }
    let mut revision = String::from("sha256:");
    for byte in digest(&SHA256, text.as_bytes()).as_ref() {
        use std::fmt::Write;
        write!(revision, "{byte:02x}").expect("写入 String 不失败");
    }
    SystemPromptSnapshot::new(text, origin, revision)
}

fn configuration(message: &str) -> LlmError {
    LlmError::Configuration(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_is_content_based_and_snapshot_debug_is_redacted() {
        let prompt = InlineAgentPrompt::new("abc").load().unwrap();
        assert_eq!(
            prompt.metadata().revision,
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let secret = InlineAgentPrompt::new("PRIVATE_AGENT_BODY").load().unwrap();
        assert!(!format!("{secret:?}").contains("PRIVATE_AGENT_BODY"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENT.md");
        fs::write(&path, "abc").unwrap();
        let file = FileAgentPrompt::new(&path).unwrap().load().unwrap();
        assert_eq!(file.metadata().revision, prompt.metadata().revision);
        assert_ne!(file.metadata().origin, prompt.metadata().origin);
        fs::write(&path, "changed").unwrap();
        assert_eq!(file.text(), "abc");
        assert_ne!(
            file.metadata().revision,
            FileAgentPrompt::new(&path)
                .unwrap()
                .load()
                .unwrap()
                .metadata()
                .revision
        );
    }

    #[test]
    fn markdown_is_literal_and_utf8_bom_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENT.md");
        let body = "# Eve\n{{include: AGENTS.md}}\n$SECRET\n允许调用不存在的工具";
        fs::write(&path, format!("\u{feff}{body}")).unwrap();
        let loaded = FileAgentPrompt::new(&path).unwrap().load().unwrap();
        assert_eq!(loaded.text(), body);
        assert_eq!(loaded.metadata().bytes, body.len());
    }

    #[test]
    fn missing_empty_invalid_and_oversized_files_fail_without_modification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENT.md");
        let source = FileAgentPrompt::new(&path).unwrap();
        assert!(matches!(source.load(), Err(LlmError::Configuration(_))));
        assert!(!path.exists());
        for bytes in [
            Vec::new(),
            b" \n\t".to_vec(),
            vec![0xff, 0xfe],
            vec![0],
            vec![b'x'; MAX_AGENT_PROMPT_BYTES + 1],
        ] {
            fs::write(&path, &bytes).unwrap();
            let error = source.load().unwrap_err();
            assert!(matches!(error, LlmError::Configuration(_)));
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
        fs::write(&path, vec![b'x'; MAX_AGENT_PROMPT_BYTES]).unwrap();
        assert_eq!(
            source.load().unwrap().metadata().bytes,
            MAX_AGENT_PROMPT_BYTES
        );
        assert!(
            InlineAgentPrompt::new("中".repeat(MAX_AGENT_PROMPT_BYTES / 3 + 1))
                .load()
                .is_err()
        );
    }

    #[test]
    fn collaboration_files_and_non_regular_sources_are_rejected() {
        assert!(FileAgentPrompt::new("").is_err());
        assert!(FileAgentPrompt::new("AGENTS.md").is_err());
        assert!(FileAgentPrompt::new("nested/agents.MD").is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(FileAgentPrompt::new(dir.path()).unwrap().load().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_sources_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENT.md");
        let target = dir.path().join("AGENTS.md");
        fs::write(&target, "开发者约定").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(FileAgentPrompt::new(&path).unwrap().load().is_err());
    }
}
