//! 继承自 eve-portable 的 Mindustry 内容校验规则（外部参考资源，AGPL-3.0）。
//!
//! `eve-portable`（https://gitee.com/zenthxsin/eve-portable，固定版本
//! `e7e43b4db4de38775a8d28f59dd4ec69e5e1686e`，许可证 AGPL-3.0）里的
//! `tools/mindustry_json_validator.py` 按 `ContentParser.java`（v8 / build 251）复刻了游戏加载
//! JSON 内容时的检查：类型能否经 ClassMap 解析、字段类型、`consumes`、研究树、颜色、引用等。
//!
//! 这里不重写那套规则，只保存一份带来源与摘要的原样副本，并在运行时调用它：
//!
//! * 规则文件为 `vendor.py`，逐字节等于上游文件；`SHA256` 是它的摘要，装配时先核对，
//!   不相等就拒绝启用（替换或损坏都不会被当作有效规则继续使用）。
//! * 调用层为 `eve_modcheck.py`：读标准输入里的文件块，转换 hjson 写法，调用规则并输出 JSON。
//! * 两个文件按 AGPL-3.0 单独授权，见同目录 `LICENSE` 与 `README.md`；它们不是本仓库
//!   MIT 许可的一部分，也不被链接进任何可执行文件（只在运行时由解释器执行）。
//!
//! Eve 把它们当作“预检”使用：预检通过只表示值得实际运行，不表示模组已通过游戏验证。

use ring::digest::{Context, SHA256};
use std::path::{Path, PathBuf};

/// 上游规则文件的 SHA-256；装配时逐字节核对。
pub const VENDOR_SHA256: &str = "0c7bdf382f5b4c690b7e04e765e272d1d45c2d6552d461398df043a609e11ba7";
/// 调用层脚本的 SHA-256；随本仓库一起审查，改动时同步更新。
pub const DRIVER_SHA256: &str = "d6f2ae42edfd0ab735f12a87168ea2693ef5900cabd52e6fd711fed5d432a0ac";
/// 上游仓库与固定版本，写进文档和错误信息，便于复核。
pub const UPSTREAM: &str =
    "https://gitee.com/zenthxsin/eve-portable@e7e43b4db4de38775a8d28f59dd4ec69e5e1686e";

/// 校验规则包的位置已确认存在且摘要相符。
#[derive(Clone, Debug)]
pub struct MindustryJsonRules {
    driver: PathBuf,
    vendor: PathBuf,
}

impl MindustryJsonRules {
    /// 核对目录下的 `eve_modcheck.py` 与 `vendor.py`；任一缺失或摘要不符都返回原因。
    ///
    /// 相对目录按当前工作目录转成绝对路径，预检以规则目录为工作目录启动时仍能找到脚本。
    pub fn open(directory: &Path) -> Result<Self, String> {
        let directory =
            std::path::absolute(directory).map_err(|_| "无法解析规则目录".to_string())?;
        let driver = directory.join("eve_modcheck.py");
        let vendor = directory.join("vendor.py");
        for (path, expected) in [(&driver, DRIVER_SHA256), (&vendor, VENDOR_SHA256)] {
            verify(path, expected)?;
        }
        Ok(Self { driver, vendor })
    }

    /// 调用层脚本路径，用于按操作者指定的解释器启动。
    pub fn driver(&self) -> &Path {
        &self.driver
    }

    /// 规则文件路径；调用层从同目录导入它。
    pub fn vendor(&self) -> &Path {
        &self.vendor
    }
}

fn verify(path: &Path, expected: &str) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|_| format!("无法读取 {}", name(path)))?;
    let mut digest = Context::new(&SHA256);
    digest.update(&bytes);
    let actual: String = digest
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "{} 的摘要与固定版本不符：实际 {actual}，期望 {expected}；不启用这套规则",
            name(path)
        ))
    }
}

fn name(path: &Path) -> String {
    path.file_name().map_or_else(
        || "校验规则".to_string(),
        |value| value.to_string_lossy().into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../vendor/eve-portable-mindustry-json-validator")
    }

    #[test]
    fn accepts_the_pinned_copy_and_rejects_any_modified_file() {
        let rules = MindustryJsonRules::open(&directory()).unwrap();
        assert!(rules.driver().ends_with("eve_modcheck.py"));
        assert!(rules.vendor().ends_with("vendor.py"));
        // cargo test 的工作目录是本 crate，相对路径也要解析成绝对路径。
        let relative = MindustryJsonRules::open(Path::new(
            "../../vendor/eve-portable-mindustry-json-validator",
        ))
        .unwrap();
        assert!(relative.driver().is_absolute() && relative.vendor().is_absolute());

        let copy = tempfile::tempdir().unwrap();
        for file in ["eve_modcheck.py", "vendor.py"] {
            std::fs::copy(directory().join(file), copy.path().join(file)).unwrap();
        }
        assert!(MindustryJsonRules::open(copy.path()).is_ok());
        std::fs::write(copy.path().join("vendor.py"), "\"\"\"改过的规则\"\"\"\n").unwrap();
        let error = MindustryJsonRules::open(copy.path()).unwrap_err();
        assert!(error.contains("摘要与固定版本不符"), "{error}");

        let empty = tempfile::tempdir().unwrap();
        let error = MindustryJsonRules::open(empty.path()).unwrap_err();
        assert!(error.contains("无法读取"), "{error}");
    }
}
