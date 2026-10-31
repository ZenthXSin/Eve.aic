//! 原生进程内凭据代理；只接受宿主显式绑定，不读环境、不访问磁盘和网络。
use eve_llm_api::{CredentialError, CredentialLease, CredentialPurpose, ModelCredentials};
use std::{collections::BTreeMap, sync::RwLock};

type Binding = (CredentialPurpose, String);
#[derive(Default)]
pub struct CredentialBank {
    bindings: RwLock<BTreeMap<Binding, Option<CredentialLease>>>,
}
impl CredentialBank {
    /// 首次声明可缺少凭据，只有实际选中时才报 Missing。重复声明拒绝，避免覆盖绑定。
    pub fn bind(
        &self,
        purpose: CredentialPurpose,
        reference: &str,
        secret: Option<String>,
    ) -> Result<(), CredentialError> {
        if reference.is_empty()
            || reference.len() > 256
            || reference
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control())
        {
            return Err(CredentialError::Invalid);
        }
        let lease = secret
            .map(|secret| CredentialLease::new(purpose, reference.into(), 1, secret))
            .transpose()?;
        let mut bindings = self
            .bindings
            .write()
            .map_err(|_| CredentialError::Unavailable)?;
        let key = (purpose, reference.into());
        if bindings.contains_key(&key) {
            return Err(CredentialError::Invalid);
        }
        bindings.insert(key, lease);
        Ok(())
    }
    /// 原子替换已存在且有凭据的绑定；旧租约仍可完成在途调用，不修改其正文。
    /// 这不是业务工具，只有持有此原生管理句柄的宿主才能调用。
    pub fn replace(
        &self,
        purpose: CredentialPurpose,
        reference: &str,
        expected_version: u64,
        secret: String,
    ) -> Result<u64, CredentialError> {
        let mut bindings = self
            .bindings
            .write()
            .map_err(|_| CredentialError::Unavailable)?;
        let current = bindings
            .get_mut(&(purpose, reference.into()))
            .ok_or(CredentialError::UnknownReference)?;
        let previous = current.as_ref().ok_or(CredentialError::Missing)?;
        if previous.version() != expected_version {
            return Err(CredentialError::Invalid);
        }
        let next = expected_version
            .checked_add(1)
            .ok_or(CredentialError::VersionExhausted)?;
        let lease = CredentialLease::new(purpose, reference.into(), next, secret)?;
        *current = Some(lease);
        Ok(next)
    }
}
impl ModelCredentials for CredentialBank {
    fn resolve(
        &self,
        purpose: CredentialPurpose,
        reference: &str,
    ) -> Result<CredentialLease, CredentialError> {
        self.bindings
            .read()
            .map_err(|_| CredentialError::Unavailable)?
            .get(&(purpose, reference.into()))
            .ok_or(CredentialError::UnknownReference)?
            .clone()
            .ok_or(CredentialError::Missing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_leases_are_redacted_and_cannot_be_issued() {
        for (reference, version, secret) in [
            ("ref", 0, "secret"),
            ("bad\nref", 1, "secret"),
            ("ref", 1, "\u{7f}"),
            ("ref", 1, ""),
            ("ref", 1, "contains space"),
        ] {
            assert!(matches!(
                CredentialLease::new(
                    CredentialPurpose::Primary,
                    reference.into(),
                    version,
                    secret.into()
                ),
                Err(CredentialError::Invalid)
            ));
        }
        for error in [
            CredentialError::UnknownReference,
            CredentialError::Missing,
            CredentialError::Invalid,
            CredentialError::Unavailable,
            CredentialError::VersionExhausted,
        ] {
            assert!(!error.to_string().contains("secret"));
        }
    }
    #[test]
    fn concurrent_replacement_has_exactly_one_winner() {
        let bank = std::sync::Arc::new(CredentialBank::default());
        bank.bind(CredentialPurpose::Primary, "ref", Some("old".into()))
            .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(12));
        let threads: Vec<_> = (0..12)
            .map(|index| {
                let bank = bank.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    bank.replace(CredentialPurpose::Primary, "ref", 1, format!("new-{index}"))
                        .is_ok()
                })
            })
            .collect();
        assert_eq!(
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .filter(|won| *won)
                .count(),
            1
        );
        assert_eq!(
            bank.resolve(CredentialPurpose::Primary, "ref")
                .unwrap()
                .version(),
            2
        );
    }
    #[test]
    fn exhausted_version_preserves_existing_binding() {
        let bank = CredentialBank::default();
        bank.bindings.write().unwrap().insert(
            (CredentialPurpose::Primary, "ref".into()),
            Some(
                CredentialLease::new(
                    CredentialPurpose::Primary,
                    "ref".into(),
                    u64::MAX,
                    "old".into(),
                )
                .unwrap(),
            ),
        );
        assert_eq!(
            bank.replace(CredentialPurpose::Primary, "ref", u64::MAX, "new".into()),
            Err(CredentialError::VersionExhausted)
        );
        let lease = bank.resolve(CredentialPurpose::Primary, "ref").unwrap();
        assert_eq!((lease.version(), lease.expose()), (u64::MAX, "old"));
    }
    #[test]
    fn isolation_missing_and_explicit_references_have_no_fallback() {
        let bank = CredentialBank::default();
        bank.bind(
            CredentialPurpose::Primary,
            "primary",
            Some("test-primary".into()),
        )
        .unwrap();
        bank.bind(CredentialPurpose::Semantic, "semantic", None)
            .unwrap();
        assert!(matches!(
            bank.resolve(CredentialPurpose::Jev, "primary"),
            Err(CredentialError::UnknownReference)
        ));
        assert!(matches!(
            bank.resolve(CredentialPurpose::Primary, "unknown"),
            Err(CredentialError::UnknownReference)
        ));
        assert!(matches!(
            bank.resolve(CredentialPurpose::Semantic, "semantic"),
            Err(CredentialError::Missing)
        ));
        assert!(
            bank.bind(
                CredentialPurpose::Primary,
                "primary",
                Some("override".into())
            )
            .is_err()
        );
        let lease = bank.resolve(CredentialPurpose::Primary, "primary").unwrap();
        assert_eq!(lease.expose(), "test-primary");
        assert!(
            lease
                .validate_for(CredentialPurpose::Jev, "primary")
                .is_err()
        );
        assert!(
            lease
                .validate_for(CredentialPurpose::Primary, "other")
                .is_err()
        );
    }
    #[test]
    fn replacement_preserves_inflight_lease_and_rejects_stale_or_invalid_updates() {
        let bank = CredentialBank::default();
        bank.bind(CredentialPurpose::Primary, "primary", Some("old".into()))
            .unwrap();
        let old = bank.resolve(CredentialPurpose::Primary, "primary").unwrap();
        assert_eq!(
            bank.replace(CredentialPurpose::Primary, "primary", 1, "new".into())
                .unwrap(),
            2
        );
        assert_eq!(old.expose(), "old");
        assert!(
            bank.replace(CredentialPurpose::Primary, "primary", 1, "stale".into())
                .is_err()
        );
        for bad in ["", "line\nbreak", "white space", "密钥"] {
            assert!(
                bank.replace(CredentialPurpose::Primary, "primary", 2, bad.into())
                    .is_err()
            );
        }
        let new = bank.resolve(CredentialPurpose::Primary, "primary").unwrap();
        assert_eq!((new.version(), new.expose()), (2, "new"));
    }
}
