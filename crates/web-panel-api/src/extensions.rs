//! 插件页面与生命周期面板契约；页面由插件提供数据，HTTP/DOM 由面板负责。
use crate::{PanelError, PanelResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc};

pub type PanelFuture<'a, T> = Pin<Box<dyn Future<Output = PanelResult<T>> + Send + 'a>>;
pub const MAX_PLUGIN_PAGES: usize = 64;
pub const MAX_PAGE_FIELDS: usize = 64;

pub fn valid_panel_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}
pub fn page_service_id(plugin: &str) -> String {
    format!("{plugin}.web-pages.v1")
}

/// 只有宿主与显式授信的页面实现持有此能力；不序列化、不发布到服务目录。
#[derive(Clone, Default)]
pub struct PageWritePermit(Arc<()>);
impl PageWritePermit {
    pub fn same_grant(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PageDescriptor {
    pub id: String,
    pub title: String,
    pub description: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct PluginPageLink {
    pub plugin_id: String,
    pub page: PageDescriptor,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PageFieldKind {
    Boolean,
    Text,
    Integer {
        minimum: Option<i64>,
        maximum: Option<i64>,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PageField {
    pub id: String,
    pub label: String,
    pub kind: PageFieldKind,
    pub value: Option<Value>,
    pub override_value: Option<Value>,
    pub source: &'static str,
    pub restart_required: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PluginPage {
    pub descriptor: PageDescriptor,
    /// 插件当前服务实例；重启后的旧页面不能写入新实例。
    pub instance: String,
    pub revision: u64,
    pub fields: Vec<PageField>,
}
impl PluginPage {
    pub fn validate(&self) -> PanelResult<()> {
        if !valid_panel_id(&self.descriptor.id)
            || self.descriptor.title.is_empty()
            || self.descriptor.title.len() > 256
            || self.descriptor.description.len() > 4096
            || self.instance.is_empty()
            || self.instance.len() > 256
            || self.fields.len() > MAX_PAGE_FIELDS
        {
            return Err(PanelError::Unavailable);
        }
        let mut seen = std::collections::BTreeSet::new();
        for field in &self.fields {
            if !valid_panel_id(&field.id)
                || !seen.insert(&field.id)
                || field.label.len() > 256
                || field
                    .value
                    .as_ref()
                    .is_some_and(|v| v.to_string().len() > 8192)
                || field
                    .override_value
                    .as_ref()
                    .is_some_and(|v| v.to_string().len() > 8192)
            {
                return Err(PanelError::Unavailable);
            }
        }
        Ok(())
    }
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageSaveRequest {
    pub plugin_id: String,
    pub page_id: String,
    pub instance: String,
    pub expected_revision: u64,
    /// null 恢复默认值；只修改列出的字段，保留其他页面与命名空间。
    #[serde(deserialize_with = "unique_values")]
    pub values: BTreeMap<String, Option<Value>>,
}
fn unique_values<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, Option<Value>>, D::Error> {
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = BTreeMap<String, Option<Value>>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("不含重复字段的值对象")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> Result<Self::Value, M::Error> {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = map.next_entry()? {
                if values.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("重复字段"));
                }
            }
            Ok(values)
        }
    }
    d.deserialize_map(Visitor)
}
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PageSaved {
    pub revision: u64,
    pub restart_required: Vec<String>,
}

/// 服务由插件按 page_service_id 发布，宿主核对注册者身份；停止时由 Kernel 自动撤销。
pub trait PluginPages: Send + Sync {
    fn pages(&self) -> PanelResult<Vec<PageDescriptor>>;
    fn read(&self, page: &str) -> PanelResult<PluginPage>;
    fn save(&self, request: &PageSaveRequest, permit: &PageWritePermit) -> PanelResult<PageSaved>;
}
#[derive(Clone)]
pub struct PluginPagesHandle(pub Arc<dyn PluginPages>);

#[derive(Clone, Debug, Serialize)]
pub struct PluginView {
    pub id: String,
    pub version: String,
    pub state: String,
    pub dependencies: Vec<String>,
    pub permissions: Vec<String>,
    pub can_start: bool,
    pub can_stop: bool,
    pub reason: Option<&'static str>,
}
#[derive(Clone, Debug, Serialize)]
pub struct PluginList {
    pub instance: String,
    pub items: Vec<PluginView>,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginActionKind {
    Start,
    Stop,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginAction {
    pub instance: String,
    pub plugin_id: String,
    pub expected_state: String,
    pub action: PluginActionKind,
}
#[derive(Clone, Debug, Serialize)]
pub struct OperationReceipt {
    pub id: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct PluginOperation {
    pub id: u64,
    pub plugin_id: String,
    pub action: PluginActionKind,
    pub state: &'static str,
}
