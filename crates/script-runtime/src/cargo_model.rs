//! Cargo 编译输入与宿主可信资源定义；生成代码不能指定宿主路径或容器参数。
use crate::{MAX_SOURCE_BYTES, Result, reject};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CargoProject {
    pub source: String,
    pub dependencies: BTreeMap<String, String>,
    pub bundle_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_source: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CargoSettings {
    pub dependencies: BTreeMap<String, String>,
    pub bundle_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_source: Option<String>,
    /// 宿主准备成功后重新填入；运行时不得默默接受另一份构建产物。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_sha256: Option<String>,
}
impl CargoSettings {
    pub fn project(&self, source: &str) -> CargoProject {
        CargoProject {
            source: source.into(),
            dependencies: self.dependencies.clone(),
            bundle_sha256: self.bundle_sha256.clone(),
            build_source: self.build_source.clone(),
        }
    }
}
const SOURCE_PREFIX: &str = "// eve-cargo-v1 ";
const SOURCE_GUARD: &str = "compile_error!(\"EVE_CARGO_V1_REQUIRES_VERIFIED_COMPILER\");";
pub(crate) fn encoded_source(settings: &CargoSettings, source: &str) -> Result<String> {
    if settings
        .artifact_sha256
        .as_deref()
        .is_none_or(|v| !valid_digest(v))
    {
        return Err(reject("Cargo缺少实际产物摘要"));
    }
    let header = serde_json::to_string(settings).map_err(|_| reject("Cargo配方无法编码"))?;
    let result = format!("{SOURCE_PREFIX}{header}\n{SOURCE_GUARD}\n{source}");
    if result.len() > MAX_SOURCE_BYTES {
        return Err(reject("Cargo配方与源码合计超限"));
    }
    Ok(result)
}
/// 持久格式仍是旧Rust字符串；旧运行器可读取账本，但保护头使其拒绝误跑Cargo产物。
pub(crate) fn decoded_source(source: &str) -> Result<Option<(CargoSettings, &str)>> {
    let Some(rest) = source.strip_prefix(SOURCE_PREFIX) else {
        return Ok(None);
    };
    let (header, rest) = rest
        .split_once('\n')
        .ok_or_else(|| reject("Cargo配方头无效"))?;
    let (guard, source) = rest
        .split_once('\n')
        .ok_or_else(|| reject("Cargo保护头缺失"))?;
    if guard != SOURCE_GUARD {
        return Err(reject("Cargo保护头已改变"));
    }
    let settings: CargoSettings =
        serde_json::from_str(header).map_err(|_| reject("Cargo配方头无效"))?;
    settings.project(source).validate()?;
    if settings
        .artifact_sha256
        .as_deref()
        .is_none_or(|v| !valid_digest(v))
    {
        return Err(reject("Cargo缺少冻结产物摘要"));
    }
    Ok(Some((settings, source)))
}
impl CargoProject {
    pub fn validate(&self) -> Result<()> {
        if self.source.is_empty()
            || self.source.len() > MAX_SOURCE_BYTES
            || self
                .build_source
                .as_ref()
                .is_some_and(|source| source.len() > MAX_SOURCE_BYTES)
            || self.dependencies.len() > 32
            || !valid_digest(&self.bundle_sha256)
        {
            return Err(reject("Cargo源码、依赖数量或冻结资源摘要无效"));
        }
        for (name, version) in &self.dependencies {
            if name.is_empty()
                || name.len() > 64
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                || version.len() > 32
                || version.split('.').count() != 3
                || !version.split('.').all(|part| {
                    !part.is_empty()
                        && (part.len() == 1 || !part.starts_with('0'))
                        && part.bytes().all(|b| b.is_ascii_digit())
                })
            {
                return Err(reject("Cargo依赖只接受普通crate名与明确的x.y.z版本"));
            }
        }
        Ok(())
    }
}
pub(crate) fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 仅由可信宿主装配；整个vendor目录被视为公开编译资源，不得含凭据或私有数据。
#[derive(Clone)]
pub struct CargoCompilerConfig {
    pub cli: PathBuf,
    pub socket: PathBuf,
    pub image: String,
    pub toolchain: PathBuf,
    pub vendor: PathBuf,
    pub bundle_sha256: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CargoCompilationFailure {
    pub code: String,
    pub stage: Option<String>,
    pub exit_code: Option<i32>,
    pub diagnostics: String,
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frozen_recipe_requires_digest_guard_and_original_source_with_legacy_rust_tag() {
        let mut settings = CargoSettings {
            dependencies: BTreeMap::new(),
            bundle_sha256: "a".repeat(64),
            build_source: None,
            artifact_sha256: None,
        };
        assert!(encoded_source(&settings, "fn main(){}").is_err());
        settings.artifact_sha256 = Some("b".repeat(64));
        let text = encoded_source(&settings, "fn main(){}").unwrap();
        let (actual, source) = decoded_source(&text).unwrap().unwrap();
        assert_eq!(actual, settings);
        assert_eq!(source, "fn main(){}");
        assert!(decoded_source(&text.replace(SOURCE_GUARD, "// skipped")).is_err());
        assert!(decoded_source(&text.replace(&"b".repeat(64), "wrong")).is_err());
        assert!(decoded_source("fn main(){}").unwrap().is_none());
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum OldLanguage {
            Rust,
        }
        assert!(matches!(
            serde_json::from_value::<OldLanguage>(
                serde_json::to_value(crate::Language::Rust).unwrap()
            )
            .unwrap(),
            OldLanguage::Rust
        ));
        let large = "x".repeat(MAX_SOURCE_BYTES);
        assert!(encoded_source(&settings, &large).is_err());
    }
}
