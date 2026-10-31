//! 宿主预声明的凭据引用策略；引用只选择注入对象，绝不据其名字读取环境。
use eve_llm_api::{CredentialError, CredentialLease, CredentialPurpose, ModelCredentials};
use eve_model_credentials::CredentialBank;
use std::sync::Arc;

pub(crate) const PRIMARY: &str = "env:EVE_OPENAI_API_KEY";
pub(crate) const AUXILIARY: &str = "env:EVE_AUXILIARY_API_KEY";
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
        (CredentialPurpose::Primary, PRIMARY)
        | (CredentialPurpose::Auxiliary, PRIMARY | AUXILIARY)
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
/// 可选辅助密钥只在宿主装配时读取；未选择的坏密钥不使主模型不可用。
pub(crate) fn core_bank(key: String) -> Result<Arc<dyn ModelCredentials>, CredentialError> {
    let auxiliary = match std::env::var("EVE_AUXILIARY_API_KEY") {
        Ok(key) => Ok(Some(key)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(_) => Err(CredentialError::Invalid),
    };
    core_bank_with_auxiliary(key, auxiliary)
}
fn core_bank_with_auxiliary(
    key: String,
    auxiliary: Result<Option<String>, CredentialError>,
) -> Result<Arc<dyn ModelCredentials>, CredentialError> {
    let bank = CredentialBank::default();
    bank.bind(CredentialPurpose::Primary, PRIMARY, Some(key.clone()))?;
    bank.bind(CredentialPurpose::Auxiliary, PRIMARY, Some(key))?;
    let auxiliary_error = match auxiliary {
        Ok(key) => bank
            .bind(CredentialPurpose::Auxiliary, AUXILIARY, key)
            .err(),
        Err(error) => Some(error),
    };
    Ok(Arc::new(CoreCredentials {
        bank,
        auxiliary_error,
    }))
}
struct CoreCredentials {
    bank: CredentialBank,
    auxiliary_error: Option<CredentialError>,
}
impl ModelCredentials for CoreCredentials {
    fn resolve(
        &self,
        purpose: CredentialPurpose,
        reference: &str,
    ) -> Result<CredentialLease, CredentialError> {
        if purpose == CredentialPurpose::Auxiliary
            && reference == AUXILIARY
            && let Some(error) = self.auxiliary_error
        {
            return Err(error);
        }
        self.bank.resolve(purpose, reference)
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_auxiliary_key_is_explicit_and_cannot_cross_into_primary_or_semantic() {
        let bank = core_bank_with_auxiliary(
            "primary-synthetic".into(),
            Ok(Some("auxiliary-synthetic".into())),
        )
        .unwrap();
        assert_eq!(
            resolve(bank.as_ref(), CredentialPurpose::Auxiliary, Some(AUXILIARY))
                .unwrap()
                .expose(),
            "auxiliary-synthetic"
        );
        assert_eq!(
            resolve(bank.as_ref(), CredentialPurpose::Auxiliary, None)
                .unwrap()
                .expose(),
            "primary-synthetic"
        );
        assert_eq!(
            resolve(bank.as_ref(), CredentialPurpose::Primary, None)
                .unwrap()
                .expose(),
            "primary-synthetic"
        );
        for purpose in [
            CredentialPurpose::Primary,
            CredentialPurpose::Jev,
            CredentialPurpose::Semantic,
        ] {
            assert!(resolve(bank.as_ref(), purpose, Some(AUXILIARY)).is_err());
        }
    }
    #[test]
    fn unselected_bad_auxiliary_key_does_not_poison_primary_or_shared_auxiliary_binding() {
        for (key, expected) in [
            (Ok(None), CredentialError::Missing),
            (
                Ok(Some("invalid synthetic key".into())),
                CredentialError::Invalid,
            ),
            (Err(CredentialError::Invalid), CredentialError::Invalid),
        ] {
            let bank = core_bank_with_auxiliary("primary-synthetic".into(), key).unwrap();
            assert!(resolve(bank.as_ref(), CredentialPurpose::Primary, None).is_ok());
            assert!(resolve(bank.as_ref(), CredentialPurpose::Auxiliary, None).is_ok());
            assert!(
                matches!(resolve(bank.as_ref(), CredentialPurpose::Auxiliary, Some(AUXILIARY)), Err(error) if error == expected)
            );
        }
    }
}
