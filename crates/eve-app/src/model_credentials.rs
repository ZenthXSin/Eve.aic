//! 宿主预声明的凭据引用策略；引用只选择注入对象，绝不据其名字读取环境。
use eve_llm_api::{CredentialError, CredentialLease, CredentialPurpose, ModelCredentials};
use eve_model_credentials::CredentialBank;
use std::sync::Arc;

pub(crate) const PRIMARY: &str = "env:EVE_OPENAI_API_KEY";
pub(crate) const JEV: &str = "env:EVE_JEV_API_KEY";
pub(crate) const SEMANTIC: &str = "env:EVE_SEMANTIC_API_KEY";

pub(crate) fn reference(
    purpose: CredentialPurpose,
    explicit: Option<&str>,
) -> Result<&str, CredentialError> {
    let reference = explicit.unwrap_or(match purpose {
        CredentialPurpose::Primary | CredentialPurpose::Auxiliary | CredentialPurpose::Semantic => {
            PRIMARY
        }
        CredentialPurpose::Jev => JEV,
    });
    match (purpose, reference) {
        (CredentialPurpose::Primary | CredentialPurpose::Auxiliary, PRIMARY)
        | (CredentialPurpose::Jev, JEV)
        | (CredentialPurpose::Semantic, PRIMARY | SEMANTIC) => Ok(reference),
        _ => Err(CredentialError::UnknownReference),
    }
}
pub(crate) fn bank(
    purpose: CredentialPurpose,
    key: String,
) -> Result<Arc<dyn ModelCredentials>, CredentialError> {
    let bank = Arc::new(CredentialBank::default());
    bank.bind(purpose, reference(purpose, None)?, Some(key))?;
    Ok(bank)
}
/// 默认 CLI 主/辅角色由宿主显式绑定同一启动密钥，租约用途仍独立核验。
/// 替换代理可为两个用途注入不同密钥，不按配置引用读取任意环境。
pub(crate) fn core_bank(key: String) -> Result<Arc<dyn ModelCredentials>, CredentialError> {
    let bank = Arc::new(CredentialBank::default());
    bank.bind(CredentialPurpose::Primary, PRIMARY, Some(key.clone()))?;
    bank.bind(CredentialPurpose::Auxiliary, PRIMARY, Some(key))?;
    Ok(bank)
}

pub(crate) fn resolve(
    broker: &dyn ModelCredentials,
    purpose: CredentialPurpose,
    explicit: Option<&str>,
) -> Result<CredentialLease, CredentialError> {
    let reference = reference(purpose, explicit)?;
    let lease = broker.resolve(purpose, reference)?;
    lease.validate_for(purpose, reference)?;
    Ok(lease)
}
