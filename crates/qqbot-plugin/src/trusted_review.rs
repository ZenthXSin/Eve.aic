//! 可信通道原文只在接收入口写入；不从可压缩历史、合并文字或模型输出重建。
use crate::{Message, QqOriginalBinding, QqOriginalMessage, QqReceiptArchive};
use eve_plugin_api::{PluginContext, PluginError, PluginResult};
use eve_session_api::SessionKey;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

const STATE_KEY: &str = "review-originals.v1";

/// 仅用于可信入站消息的同步本地缩权；实现不得授予权限、等待模型或发网络请求。
pub trait QqRestrictionObserver: Send + Sync {
    fn restrict(
        &self,
        session: &SessionKey,
        message_id: &str,
        originals: &[String],
    ) -> PluginResult<()>;
    /// 自身显示正文仅剥离通道验证的机器人提及；引用和附件未拼入。审计仍必须保存 originals。
    fn restrict_body(
        &self,
        session: &SessionKey,
        message_id: &str,
        _own_body: &str,
        originals: &[String],
    ) -> PluginResult<()> {
        self.restrict(session, message_id, originals)
    }
    /// 最新正文可信但历史原话不完整时，只允许保守失败关闭，不得补造授权审计。
    fn restrict_missing(
        &self,
        _session: &SessionKey,
        _message_id: &str,
        _latest_body: &str,
    ) -> PluginResult<()> {
        Ok(())
    }
    fn restrict_missing_body(
        &self,
        session: &SessionKey,
        message_id: &str,
        _own_body: &str,
        latest_body: &str,
    ) -> PluginResult<()> {
        self.restrict_missing(session, message_id, latest_body)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    version: u8,
    identity: String,
    records: u64,
    #[serde(default)]
    legacy_scopes: Vec<LegacyScope>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyScope {
    app_id: String,
    session: SessionKey,
}
impl Binding {
    fn expected(&self) -> QqOriginalBinding {
        QqOriginalBinding {
            identity: self.identity.clone(),
            records: self.records,
        }
    }
}

#[derive(Default)]
struct State {
    backend: Option<Arc<dyn QqReceiptArchive>>,
    binding: Option<Binding>,
}

#[derive(Clone, Default)]
pub(crate) struct TrustedReview(Arc<Mutex<State>>);

fn unavailable() -> PluginError {
    PluginError::State("QQ 审核原文缺失、身份不符或存储不可用；未从加工文本补造授权".into())
}

impl TrustedReview {
    pub(crate) fn attach(
        &self,
        ctx: &PluginContext,
        backend: Option<Arc<dyn QqReceiptArchive>>,
    ) -> PluginResult<()> {
        // 再附着失败必须使旧句柄失效，不能继续读取上一次生命周期的授权。
        let mut state = self.0.lock().map_err(|_| unavailable())?;
        *state = State::default();
        let old = ctx
            .state_get(STATE_KEY)?
            .map(|bytes| {
                let binding: Binding = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
                let mut scopes = std::collections::BTreeSet::new();
                if binding.version != 1
                    || !binding.expected().valid()
                    || binding.legacy_scopes.len() > 65_536
                    || binding.legacy_scopes.iter().any(|scope| {
                        !crate::state::valid_id(&scope.app_id)
                            || scope.session.validate().is_err()
                            || scope.session.session_id != scope.session.user_id
                            || !scopes.insert((&scope.app_id, &scope.session.session_id))
                    })
                {
                    return Err(unavailable());
                }
                Ok(binding)
            })
            .transpose()?;
        let identity = backend
            .as_ref()
            .map(|db| db.original_identity())
            .transpose()?
            .flatten();
        if identity.as_ref().is_some_and(|(identity, records)| {
            !QqOriginalBinding {
                identity: identity.clone(),
                records: *records,
            }
            .valid()
                || (old.is_none() && *records > 0)
        }) {
            // 首次启用先写空库绑定，再收消息；有原文却无标记意味着标记丢失，不能重新绑定。
            return Err(unavailable());
        }
        if let Some(binding) = &old
            && identity
                .as_ref()
                .is_none_or(|(id, count)| id != &binding.identity || *count < binding.records)
        {
            return Err(unavailable());
        }
        let legacy_scopes = match &old {
            Some(old) => old.legacy_scopes.clone(),
            None => {
                // 活跃回执对每个历史会话至少钉住最新一条（冷归档也保留该索引）。
                // 只保存身份与缺口，不将旧加工正文写入原文库。
                let ledger = crate::state::Ledger::load(ctx)?;
                let mut seen = std::collections::BTreeSet::new();
                let mut scopes = Vec::new();
                for entry in ledger.entries {
                    let session = entry.message.session_key(&entry.app_id)?;
                    if seen.insert((entry.app_id.clone(), session.session_id.clone())) {
                        scopes.push(LegacyScope {
                            app_id: entry.app_id,
                            session,
                        });
                    }
                }
                scopes
            }
        };
        let binding = identity.map(|(identity, records)| Binding {
            version: 1,
            identity,
            records,
            legacy_scopes,
        });
        if let Some(binding) = &binding {
            // 单独绑定数据库。旧回执不含 SDK 原文，迁移时绝不从它们补猜。
            ctx.state_set(
                STATE_KEY,
                serde_json::to_vec(binding).map_err(|_| unavailable())?,
            )?;
        }
        *state = State { backend, binding };
        Ok(())
    }

    pub(crate) fn record(
        &self,
        ctx: &PluginContext,
        app: &str,
        message: &Message,
        original: Option<String>,
    ) -> PluginResult<()> {
        let record = QqOriginalMessage {
            app_id: app.into(),
            message_id: message.id.clone(),
            scope: message.scope.clone(),
            target_id: message.target_id.clone(),
            user_id: message.user_id.clone(),
            text: original,
        };
        if !record.valid() {
            return Err(unavailable());
        }
        let mut state = self.0.lock().map_err(|_| unavailable())?;
        let Some(binding) = &state.binding else {
            // 替换通道可不启用审核，但 source.read 必须失败关闭，不能回退普通历史。
            return Ok(());
        };
        let mut next = binding.clone();
        next.records = state
            .backend
            .as_ref()
            .ok_or_else(unavailable)?
            .store_original_bound(&record, &binding.expected())?;
        if next.records < binding.records {
            return Err(unavailable());
        }
        ctx.state_set(
            STATE_KEY,
            serde_json::to_vec(&next).map_err(|_| unavailable())?,
        )?;
        state.binding = Some(next);
        Ok(())
    }

    pub(crate) fn read(
        &self,
        app: &str,
        session: &SessionKey,
        id: &str,
    ) -> PluginResult<Vec<String>> {
        let state = self.0.lock().map_err(|_| unavailable())?;
        let binding = state.binding.as_ref().ok_or_else(unavailable)?;
        let records = state
            .backend
            .as_ref()
            .ok_or_else(unavailable)?
            .read_originals_bound(app, session, id, &binding.expected())?;
        if records.len() < 6
            && binding
                .legacy_scopes
                .iter()
                .any(|legacy| legacy.app_id == app && legacy.session == *session)
        {
            return Err(unavailable());
        }
        let mut ids = std::collections::BTreeSet::new();
        if records.is_empty()
            || records.len() > 6
            || records.last().is_none_or(|r| r.message_id != id)
            || records.iter().any(|r| {
                !r.valid()
                    || r.app_id != app
                    || r.session_key().as_ref().ok() != Some(session)
                    || !ids.insert(r.message_id.as_str())
            })
        {
            return Err(unavailable());
        }
        records
            .into_iter()
            .map(|record| record.text.ok_or_else(unavailable))
            .collect()
    }

    pub(crate) fn is_current(
        &self,
        app: &str,
        session: &SessionKey,
        id: &str,
    ) -> PluginResult<bool> {
        let state = self.0.lock().map_err(|_| unavailable())?;
        let binding = state.binding.as_ref().ok_or_else(unavailable)?;
        state
            .backend
            .as_ref()
            .ok_or_else(unavailable)?
            .original_is_current_bound(app, session, id, &binding.expected())
    }
}

#[cfg(test)]
#[path = "trusted_review_tests.rs"]
mod tests;
