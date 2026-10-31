use crate::cold::{MAX_LEDGER_BYTES, ReceiptIndex, VersionIndex, VersionSummary};
use crate::{
    PublishedVersion, Receipt, ReceiptStatus, Result, ScriptSpec, StateMigration, StateSnapshot,
    VersionRef, digest, fail, reject,
};
use eve_llm_api::ContextScope;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    versions: BTreeMap<String, VersionIndex>,
    active: BTreeMap<String, VersionRef>,
    states: BTreeMap<String, StateSnapshot>,
    receipts: BTreeMap<String, ReceiptIndex>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    format: u32,
    checksum: String,
    ledger: Ledger,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyLedger {
    versions: BTreeMap<String, PublishedVersion>,
    active: BTreeMap<String, VersionRef>,
    states: BTreeMap<String, StateSnapshot>,
    receipts: BTreeMap<String, Receipt>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyEnvelope {
    format: u32,
    checksum: String,
    ledger: LegacyLedger,
}

// 事务候选只序列化借用；不能为Envelope再复制整个版本/回执账本。
#[derive(Serialize)]
struct BorrowedEnvelope<'a> {
    format: u32,
    checksum: String,
    ledger: &'a Ledger,
}

pub struct ScriptStore {
    directory: PathBuf,
    ledger: Mutex<Ledger>,
    poisoned: std::sync::atomic::AtomicBool,
    _lock: StoreLock,
    #[cfg(test)]
    lock_attempts: std::sync::atomic::AtomicUsize,
}
// 只有成功取得的锁才包装；失败的独立打开不能影响另一个 Store。
struct StoreLock(File);
impl Drop for StoreLock {
    fn drop(&mut self) {
        // Unix 的 fork/dup 可保留同一 open file description；所有者结束即显式释放。
        // 解锁失败仍关闭本句柄，不删锁文件、不重试或绕过排他性。
        let _ = self.0.unlock();
    }
}
impl ScriptStore {
    /// 独占进程锁；损坏、未知格式、缺失已初始化账本均拒绝打开，不清空。
    pub fn open(directory: impl AsRef<Path>) -> Result<Arc<Self>> {
        let directory = directory.as_ref().to_path_buf();
        std::fs::create_dir_all(&directory).map_err(fail)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("ledger.lock"))
            .map_err(fail)?;
        // Rust 1.89 的标准库锁在 Unix/Windows 使用对应 OS 独占锁，整个 Store 持有。
        lock.try_lock()
            .map_err(|_| fail("脚本账本锁不可用或已被另一运行器占用"))?;
        let mut lock = StoreLock(lock);
        let path = directory.join("ledger.json");
        let ledger = if path.exists() {
            if std::fs::metadata(&path).map_err(fail)?.len() > MAX_LEDGER_BYTES as u64 {
                return Err(fail("脚本账本超限，未清空"));
            }
            let data = std::fs::read(&path).map_err(fail)?;
            let value = strict_json(&data)?;
            let ledger = match value.get("format").and_then(Value::as_u64) {
                Some(1) => {
                    let envelope: LegacyEnvelope = serde_json::from_value(value).map_err(fail)?;
                    if envelope.checksum != digest(&envelope.ledger)? {
                        return Err(fail("旧脚本账本校验错误，未清空"));
                    }
                    let old = envelope.ledger;
                    let mut ledger = Ledger {
                        active: old.active,
                        states: old.states,
                        ..Ledger::default()
                    };
                    // 冷记录先持久化，原账本只有在全部验证通过后才原子切换。
                    for (id, version) in old.versions {
                        ledger
                            .versions
                            .insert(id, VersionIndex::create(&directory, &version)?);
                    }
                    for (id, receipt) in old.receipts {
                        let mut index = ReceiptIndex::hot(receipt)?;
                        index.freeze(&directory)?;
                        ledger.receipts.insert(id, index);
                    }
                    ledger
                }
                Some(2) => {
                    let envelope: Envelope = serde_json::from_value(value).map_err(fail)?;
                    if envelope.checksum != digest(&envelope.ledger)? {
                        return Err(fail("脚本账本校验错误，未清空"));
                    }
                    envelope.ledger
                }
                _ => return Err(fail("脚本账本格式未知，未清空")),
            };
            validate_ledger(&ledger, &directory)?;
            ledger
        } else {
            let mut marker = String::new();
            lock.0.read_to_string(&mut marker).map_err(fail)?;
            if !marker.is_empty() {
                return Err(fail("已初始化的脚本账本缺失，拒绝重建"));
            }
            Ledger::default()
        };
        let store = Arc::new(Self {
            directory,
            ledger: Mutex::new(ledger),
            poisoned: std::sync::atomic::AtomicBool::new(false),
            _lock: lock,
            #[cfg(test)]
            lock_attempts: std::sync::atomic::AtomicUsize::new(0),
        });
        // 只把崩溃时未完成的调用标成 unknown；从不重新启动它们。
        store.change(|ledger| {
            for receipt in ledger.receipts.values_mut() {
                if receipt.status == ReceiptStatus::Running {
                    let mut body = receipt.load(&store.directory)?;
                    body.status = ReceiptStatus::Unknown;
                    body.error = Some("宿主重启中断；须核对外部效果，禁止自动重放".into());
                    *receipt = ReceiptIndex::hot(body)?;
                }
            }
            Ok(())
        })?;
        let mut marker = &store._lock.0;
        marker.write_all(b"initialized-v1\n").map_err(fail)?;
        marker.sync_all().map_err(fail)?;
        Ok(store)
    }
    fn read<T>(&self, action: impl FnOnce(&Ledger) -> Result<T>) -> Result<T> {
        if self.poisoned.load(std::sync::atomic::Ordering::Acquire) {
            return Err(fail("账本提交结果未知，须重新打开核对"));
        }
        #[cfg(test)]
        self.lock_attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let ledger = self.ledger.lock().map_err(fail)?;
        // 排队期间前一个提交可能失败；持锁后必须再次确认，不能覆盖未知提交。
        if self.poisoned.load(std::sync::atomic::Ordering::Acquire) {
            return Err(fail("账本提交结果未知，须重新打开核对"));
        }
        action(&ledger)
    }
    fn change<T>(&self, action: impl FnOnce(&mut Ledger) -> Result<T>) -> Result<T> {
        if self.poisoned.load(std::sync::atomic::Ordering::Acquire) {
            return Err(fail("账本提交结果未知，须重新打开核对"));
        }
        #[cfg(test)]
        self.lock_attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut guard = self.ledger.lock().map_err(fail)?;
        // 排队期间前一个提交可能失败；持锁后必须再次确认，不能覆盖未知提交。
        if self.poisoned.load(std::sync::atomic::Ordering::Acquire) {
            return Err(fail("账本提交结果未知，须重新打开核对"));
        }
        let mut next = guard.clone();
        let output = action(&mut next)?;
        for receipt in next.receipts.values_mut() {
            receipt.freeze(&self.directory)?;
        }
        let envelope = BorrowedEnvelope {
            format: 2,
            checksum: digest(&next)?,
            ledger: &next,
        };
        let bytes = serde_json::to_vec(&envelope).map_err(fail)?;
        if bytes.len() > MAX_LEDGER_BYTES {
            return Err(fail("脚本账本容量已满；保留全部记录"));
        }
        let persist = || -> Result<()> {
            let mut file = tempfile::NamedTempFile::new_in(&self.directory).map_err(fail)?;
            file.write_all(&bytes).map_err(fail)?;
            file.as_file().sync_all().map_err(fail)?;
            file.persist(self.directory.join("ledger.json"))
                .map_err(fail)?;
            #[cfg(unix)]
            File::open(&self.directory)
                .map_err(fail)?
                .sync_all()
                .map_err(fail)?;
            // Windows 的普通目录句柄不能 FlushFileBuffers；文件已fsync并原子替换。
            Ok(())
        };
        if let Err(error) = persist() {
            self.poisoned
                .store(true, std::sync::atomic::Ordering::Release);
            return Err(error);
        }
        *guard = next;
        Ok(output)
    }
    pub fn publish(&self, owner: &ContextScope, spec: ScriptSpec) -> Result<PublishedVersion> {
        if matches!(spec.language, crate::Language::RustCargo(_)) {
            return Err(reject("Cargo必须由准备入口验证并规范化后保存"));
        }
        spec.validate()?;
        let version = PublishedVersion {
            reference: VersionRef {
                script: spec.name.clone(),
                digest: digest(&(owner, &spec))?,
            },
            owner: owner.clone(),
            spec,
        };
        self.change(|ledger| {
            for dependency in version.spec.dependencies.values() {
                get_version(ledger, owner, dependency)?;
            }
            if let Some(existing) = ledger.versions.get(&version.reference.digest) {
                return existing.load(&self.directory);
            }
            if ledger.versions.len() >= 256 {
                return Err(reject("版本容量已满，未删除历史版本"));
            }
            ledger.versions.insert(
                version.reference.digest.clone(),
                VersionIndex::create(&self.directory, &version)?,
            );
            Ok(version)
        })
    }
    pub fn version(
        &self,
        owner: &ContextScope,
        reference: &VersionRef,
    ) -> Result<PublishedVersion> {
        self.read(|ledger| get_version(ledger, owner, reference)?.load(&self.directory))
    }
    pub fn list(&self, owner: &ContextScope) -> Result<Vec<PublishedVersion>> {
        self.read(|ledger| {
            ledger
                .versions
                .values()
                .filter(|version| &version.owner == owner)
                .map(|version| version.load(&self.directory))
                .collect::<Result<Vec<_>>>()
        })
    }
    /// 只读版本元信息；列表不会加载源码。需要全文时显式调用 version。
    pub fn summaries(&self, owner: &ContextScope) -> Result<Vec<VersionSummary>> {
        self.read(|ledger| {
            Ok(ledger
                .versions
                .values()
                .filter(|v| &v.owner == owner)
                .map(|v| v.summary.clone())
                .collect())
        })
    }
    pub fn selected(&self, owner: &ContextScope, script: &str) -> Result<VersionRef> {
        self.read(|ledger| {
            ledger
                .active
                .get(&active_key(owner, script)?)
                .cloned()
                .ok_or_else(|| reject("脚本尚未启用"))
        })
    }
    /// 首次启用用 initial_state；schema 改变必须给显式迁移。回退只选择已有版本状态快照。
    pub fn activate(
        &self,
        owner: &ContextScope,
        reference: &VersionRef,
        migration: Option<StateMigration>,
    ) -> Result<()> {
        self.activate_with_receipt(owner, reference, migration, None)
    }
    pub(crate) fn activate_with_receipt(
        &self,
        owner: &ContextScope,
        reference: &VersionRef,
        migration: Option<StateMigration>,
        receipt: Option<Receipt>,
    ) -> Result<()> {
        self.change(|ledger| {
            if let Some(receipt) = &receipt
                && ledger.receipts.contains_key(&receipt.call_id)
            {
                return Err(reject("迁移调用ID已存在"));
            }
            let version = get_version(ledger, owner, reference)?.load(&self.directory)?;
            if !ledger.states.contains_key(&reference.digest) {
                let previous = ledger
                    .active
                    .get(&active_key(owner, &reference.script)?)
                    .cloned();
                let value = if let Some(migration) = migration {
                    get_version(ledger, owner, &migration.from)?;
                    if migration.from.script != reference.script {
                        return Err(reject("不能跨脚本迁移状态"));
                    }
                    if ledger.receipts.values().any(|r| {
                        r.version == migration.from
                            && matches!(r.status, ReceiptStatus::Running | ReceiptStatus::Unknown)
                    }) {
                        return Err(reject("源版本存在运行中或未知调用，先核对效果"));
                    }
                    let source = ledger
                        .states
                        .get(&migration.from.digest)
                        .ok_or_else(|| reject("迁移源没有状态"))?;
                    if source.revision != migration.expected_revision {
                        return Err(reject("迁移源状态已变化"));
                    }
                    migration.value
                } else if let Some(previous) = previous {
                    if ledger.receipts.values().any(|r| {
                        r.version == previous
                            && (r.status == ReceiptStatus::Unknown
                                || (r.status == ReceiptStatus::Running
                                    && !(version.spec.state_schema.fields.is_empty()
                                        && ledger.states.get(&previous.digest).is_some_and(
                                            |state| {
                                                state.schema == version.spec.state_schema
                                                    && state.schema.fields.is_empty()
                                            },
                                        ))))
                    }) {
                        return Err(reject("源版本存在运行中或未知调用，不能复制状态"));
                    }
                    let source = ledger
                        .states
                        .get(&previous.digest)
                        .ok_or_else(|| fail("启用版本缺少状态"))?;
                    if source.schema != version.spec.state_schema {
                        return Err(reject("schema 变化需要显式状态迁移"));
                    }
                    source.value.clone()
                } else {
                    version.spec.initial_state.clone()
                };
                version.spec.state_schema.validate(&value)?;
                ledger.states.insert(
                    reference.digest.clone(),
                    StateSnapshot {
                        revision: 0,
                        schema: version.spec.state_schema,
                        value,
                    },
                );
            } else if migration.is_some() {
                return Err(reject("目标已有独立状态，不能隐式覆盖"));
            }
            ledger
                .active
                .insert(active_key(owner, &reference.script)?, reference.clone());
            if let Some(receipt) = receipt {
                ledger
                    .receipts
                    .insert(receipt.call_id.clone(), ReceiptIndex::hot(receipt)?);
            }
            Ok(())
        })
    }
    pub(crate) fn migration_source(
        &self,
        owner: &ContextScope,
        target: &VersionRef,
        from: &VersionRef,
        revision: u64,
        call_id: &str,
    ) -> Result<StateSnapshot> {
        self.read(|ledger| {
            get_version(ledger, owner, target)?;
            get_version(ledger, owner, from)?;
            if target.script != from.script
                || ledger.states.contains_key(&target.digest)
                || ledger.receipts.contains_key(call_id)
            {
                return Err(reject("迁移脚本/目标状态/调用ID无效"));
            }
            if ledger.receipts.values().any(|r| {
                r.version == *from
                    && matches!(r.status, ReceiptStatus::Running | ReceiptStatus::Unknown)
            }) {
                return Err(reject("源版本存在运行中或未知调用"));
            }
            let state = ledger
                .states
                .get(&from.digest)
                .ok_or_else(|| reject("迁移源没有状态"))?;
            if state.revision != revision {
                return Err(reject("迁移源状态已变化"));
            }
            Ok(state.clone())
        })
    }
    pub fn state(&self, owner: &ContextScope, reference: &VersionRef) -> Result<StateSnapshot> {
        self.read(|ledger| {
            get_version(ledger, owner, reference)?;
            ledger
                .states
                .get(&reference.digest)
                .cloned()
                .ok_or_else(|| reject("版本尚未初始化状态"))
        })
    }
    pub fn receipt(&self, owner: &ContextScope, call_id: &str) -> Result<Receipt> {
        self.read(|ledger| {
            ledger
                .receipts
                .get(call_id)
                .filter(|r| &r.owner == owner)
                .ok_or_else(|| reject("没有该作用域的回执"))?
                .load(&self.directory)
        })
    }
    pub(crate) fn begin(&self, receipt: Receipt) -> Result<StateSnapshot> {
        self.change(|ledger| {
            if ledger.receipts.contains_key(&receipt.call_id) {
                return Err(reject("调用 ID 已存在；读取回执核对，禁止重放"));
            }
            // 同一版本状态采用串行执行，避免外部效果发生后才发现 CAS 冲突。
            if ledger.receipts.values().any(|r| {
                r.version == receipt.version
                    && matches!(r.status, ReceiptStatus::Running | ReceiptStatus::Unknown)
            }) {
                return Err(reject("该版本已有运行中或未知调用，须先核对"));
            }
            let state = ledger
                .states
                .get(&receipt.version.digest)
                .cloned()
                .ok_or_else(|| reject("版本尚未启用过"))?;
            ledger
                .receipts
                .insert(receipt.call_id.clone(), ReceiptIndex::hot(receipt)?);
            Ok(state)
        })
    }
    pub(crate) fn record(&self, receipt: &Receipt) -> Result<()> {
        self.change(|ledger| {
            let existing = ledger
                .receipts
                .get(&receipt.call_id)
                .ok_or_else(|| fail("回执不存在"))?;
            if existing.status != ReceiptStatus::Running {
                return Err(fail("回执已终结"));
            }
            ledger
                .receipts
                .insert(receipt.call_id.clone(), ReceiptIndex::hot(receipt.clone())?);
            Ok(())
        })
    }
    pub(crate) fn complete(&self, receipt: &Receipt, state: Option<Value>) -> Result<()> {
        self.change(|ledger| {
            let existing = ledger
                .receipts
                .get(&receipt.call_id)
                .ok_or_else(|| fail("回执不存在"))?;
            if existing.status != ReceiptStatus::Running {
                return Err(fail("回执已终结"));
            }
            if let Some(value) = state {
                if receipt.status != ReceiptStatus::Succeeded {
                    return Err(fail("失败不能提交状态"));
                }
                let snapshot = ledger
                    .states
                    .get_mut(&receipt.version.digest)
                    .ok_or_else(|| fail("状态不存在"))?;
                snapshot.schema.validate(&value)?;
                snapshot.revision = snapshot
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| fail("状态版本溢出"))?;
                snapshot.value = value;
            }
            ledger
                .receipts
                .insert(receipt.call_id.clone(), ReceiptIndex::hot(receipt.clone())?);
            Ok(())
        })
    }
    pub(crate) fn interrupted(&self, owner: &ContextScope, call_id: &str) -> Result<()> {
        self.change(|ledger| {
            let receipt = ledger
                .receipts
                .get_mut(call_id)
                .filter(|r| &r.owner == owner)
                .ok_or_else(|| fail("中断回执不存在"))?;
            if receipt.status == ReceiptStatus::Running {
                let mut body = receipt.load(&self.directory)?;
                body.status = ReceiptStatus::Unknown;
                body.error = Some("调用future被宿主中断；须核对效果，禁止自动重放".into());
                *receipt = ReceiptIndex::hot(body)?;
            }
            Ok(())
        })
    }
    /// 只由可信人工核对入口调用；保留证据，不恢复、不重放历史调用。
    pub fn acknowledge_unknown(
        &self,
        owner: &ContextScope,
        call_id: &str,
        evidence: &str,
    ) -> Result<()> {
        if evidence.trim().is_empty() || evidence.len() > 2048 {
            return Err(reject("核对需要有界非空证据"));
        }
        self.change(|ledger| {
            let receipt = ledger
                .receipts
                .get_mut(call_id)
                .filter(|r| &r.owner == owner)
                .ok_or_else(|| reject("回执不存在"))?;
            if receipt.status != ReceiptStatus::Unknown {
                return Err(reject("只有 unknown 可核对"));
            }
            let mut body = receipt.load(&self.directory)?;
            body.status = ReceiptStatus::Failed;
            body.error = Some(format!("已核对，未重放: {evidence}"));
            *receipt = ReceiptIndex::hot(body)?;
            Ok(())
        })
    }
}
fn active_key(owner: &ContextScope, script: &str) -> Result<String> {
    digest(&(owner, script))
}
fn get_version<'a>(
    ledger: &'a Ledger,
    owner: &ContextScope,
    reference: &VersionRef,
) -> Result<&'a VersionIndex> {
    ledger
        .versions
        .get(&reference.digest)
        .filter(|v| &v.reference == reference && &v.owner == owner)
        .ok_or_else(|| reject("不可见或未知脚本版本"))
}
fn validate_ledger(ledger: &Ledger, directory: &Path) -> Result<()> {
    for (id, index) in &ledger.versions {
        let version = index.load(directory)?;
        version.spec.validate()?;
        if matches!(version.spec.language, crate::Language::RustCargo(_)) {
            return Err(reject("账本含未规范化Cargo版本"));
        }
        if id != &version.reference.digest
            || version.reference.script != version.spec.name
            || digest(&(&version.owner, &version.spec))? != *id
        {
            return Err(fail("脚本版本内容被修改，未清空"));
        }
        for dependency in version.spec.dependencies.values() {
            get_version(ledger, &version.owner, dependency)?;
        }
    }
    for (key, reference) in &ledger.active {
        let version = ledger
            .versions
            .get(&reference.digest)
            .ok_or_else(|| fail("启用版本缺失"))?;
        if &version.reference != reference
            || active_key(&version.owner, &reference.script)? != *key
            || !ledger.states.contains_key(&reference.digest)
        {
            return Err(fail("启用记录损坏"));
        }
    }
    for (id, state) in &ledger.states {
        let version = ledger.versions.get(id).ok_or_else(|| fail("孤立状态"))?;
        if state.schema != version.state_schema {
            return Err(fail("状态 schema 不匹配"));
        }
        state.schema.validate(&state.value)?;
    }
    for (id, index) in &ledger.receipts {
        let receipt = index.load(directory)?;
        if id != &receipt.call_id {
            return Err(fail("回执 ID 不匹配"));
        }
        get_version(ledger, &receipt.owner, &receipt.version)?;
        if receipt
            .parent_call_id
            .as_ref()
            .is_some_and(|parent| !ledger.receipts.contains_key(parent))
        {
            return Err(fail("回执调用链断裂"));
        }
    }
    Ok(())
}

/// serde_json 默认接受重复 map 键；账本与进程协议必须拒绝。
pub(crate) fn strict_json(bytes: &[u8]) -> Result<Value> {
    struct Strict;
    impl<'de> serde::de::Visitor<'de> for Strict {
        type Value = Value;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("无重复键的 JSON")
        }
        fn visit_bool<E: serde::de::Error>(self, v: bool) -> std::result::Result<Value, E> {
            Ok(Value::Bool(v))
        }
        fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Value, E> {
            Ok(v.into())
        }
        fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Value, E> {
            Ok(v.into())
        }
        fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Value, E> {
            serde_json::Number::from_f64(v)
                .map(Value::Number)
                .ok_or_else(|| E::custom("无效数字"))
        }
        fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Value, E> {
            Ok(v.into())
        }
        fn visit_none<E: serde::de::Error>(self) -> std::result::Result<Value, E> {
            Ok(Value::Null)
        }
        fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Value, E> {
            Ok(Value::Null)
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Value, A::Error> {
            let mut items = Vec::new();
            while let Some(item) = seq.next_element::<StrictValue>()? {
                items.push(item.0);
            }
            Ok(Value::Array(items))
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> std::result::Result<Value, A::Error> {
            let mut object = serde_json::Map::new();
            while let Some((key, value)) = map.next_entry::<String, StrictValue>()? {
                if object.insert(key, value.0).is_some() {
                    return Err(serde::de::Error::custom("重复 JSON 键"));
                }
            }
            Ok(Value::Object(object))
        }
    }
    struct StrictValue(Value);
    impl<'de> Deserialize<'de> for StrictValue {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
            d.deserialize_any(Strict).map(Self)
        }
    }
    serde_json::from_slice::<StrictValue>(bytes)
        .map(|value| value.0)
        .map_err(fail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[cfg(unix)]
    #[test]
    fn last_owner_releases_lock_even_if_fork_equivalent_descriptor_is_still_open() {
        let directory = tempfile::tempdir().unwrap();
        let store = ScriptStore::open(directory.path()).unwrap();
        // dup 与 fork 的描述符共享同一 open file description；无需不安全 fork。
        let inherited = store._lock.0.try_clone().unwrap();
        let owner = store.clone();
        drop(store);
        assert!(ScriptStore::open(directory.path()).is_err());
        drop(owner);
        // 并行子进程尚未 exec/关闭继承的描述符，不应延长已结束 Store 的所有权。
        let reopened = ScriptStore::open(directory.path()).unwrap();
        drop(inherited);
        // 旧描述符关闭也不能释放后来重新取得的独占锁。
        assert!(ScriptStore::open(directory.path()).is_err());
        drop(reopened);
        assert!(ScriptStore::open(directory.path()).is_ok());
    }

    #[test]
    fn queued_access_fails_closed_after_an_earlier_commit_poisoned_the_store() {
        let directory = tempfile::tempdir().unwrap();
        let store = ScriptStore::open(directory.path()).unwrap();
        // Force the next atomic replacement to fail without touching external files.
        std::fs::remove_file(directory.path().join("ledger.json")).unwrap();
        std::fs::create_dir(directory.path().join("ledger.json")).unwrap();
        let mut waiting = Vec::new();
        let failed: Result<()> = store.change(|_| {
            let initial = store.lock_attempts.load(Ordering::SeqCst);
            for write in [false, true] {
                let store = store.clone();
                waiting.push(std::thread::spawn(move || {
                    if write {
                        store.change(|_| Ok(()))
                    } else {
                        store.read(|_| Ok(()))
                    }
                }));
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while store.lock_attempts.load(Ordering::SeqCst) < initial + 2 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "waiters did not reach lock"
                );
                std::thread::yield_now();
            }
            Ok(())
        });
        assert!(failed.is_err());
        for waiter in waiting {
            let error = waiter
                .join()
                .unwrap()
                .expect_err("queued access bypassed poisoned store");
            assert!(error.to_string().contains("提交结果未知"));
        }
    }
    fn fixture_version() -> PublishedVersion {
        let owner = ContextScope {
            session_id: "cold-session".into(),
            user_id: "alice".into(),
        };
        let spec = ScriptSpec {
            name: "cold-script".into(),
            language: crate::Language::Python,
            source: format!("# private source {}", "x".repeat(128 * 1024)),
            dependencies: BTreeMap::new(),
            tools: vec![],
            state_schema: crate::StateSchema {
                version: 1,
                fields: BTreeMap::new(),
            },
            initial_state: serde_json::json!({}),
        };
        PublishedVersion {
            reference: VersionRef {
                script: spec.name.clone(),
                digest: digest(&(&owner, &spec)).unwrap(),
            },
            owner,
            spec,
        }
    }
    fn fixture_receipt(version: &PublishedVersion, id: &str, status: ReceiptStatus) -> Receipt {
        Receipt {
            call_id: id.into(),
            parent_call_id: None,
            root_call_id: id.into(),
            owner: version.owner.clone(),
            version: version.reference.clone(),
            request_digest: "request".into(),
            status,
            result: Some(serde_json::json!({"private_result": "y".repeat(32 * 1024)})),
            evidence: vec![],
            error: None,
            failure_diagnostic: None,
        }
    }
    #[test]
    fn legacy_migration_cools_source_and_terminal_receipts_without_replaying() {
        let dir = tempfile::tempdir().unwrap();
        let version = fixture_version();
        let completed = fixture_receipt(&version, "completed", ReceiptStatus::Succeeded);
        let running = fixture_receipt(&version, "running", ReceiptStatus::Running);
        let ledger = LegacyLedger {
            versions: BTreeMap::from([(version.reference.digest.clone(), version.clone())]),
            active: BTreeMap::from([(
                active_key(&version.owner, &version.reference.script).unwrap(),
                version.reference.clone(),
            )]),
            states: BTreeMap::from([(
                version.reference.digest.clone(),
                StateSnapshot {
                    revision: 3,
                    schema: version.spec.state_schema.clone(),
                    value: serde_json::json!({}),
                },
            )]),
            receipts: BTreeMap::from([
                (completed.call_id.clone(), completed.clone()),
                (running.call_id.clone(), running.clone()),
            ]),
        };
        let old = LegacyEnvelope {
            format: 1,
            checksum: digest(&ledger).unwrap(),
            ledger,
        };
        std::fs::write(
            dir.path().join("ledger.json"),
            serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();
        let store = ScriptStore::open(dir.path()).unwrap();
        assert_eq!(
            store.version(&version.owner, &version.reference).unwrap(),
            version
        );
        assert_eq!(
            store.receipt(&version.owner, "completed").unwrap(),
            completed
        );
        assert_eq!(
            store.receipt(&version.owner, "running").unwrap().status,
            ReceiptStatus::Unknown
        );
        assert!(store.begin(completed.clone()).is_err());
        assert_eq!(
            store
                .state(&version.owner, &version.reference)
                .unwrap()
                .revision,
            3
        );
        store
            .read(|ledger| {
                assert!(ledger.receipts["completed"].hot.is_none());
                assert!(ledger.receipts["running"].hot.is_some());
                Ok(())
            })
            .unwrap();
        let saved = std::fs::read_to_string(dir.path().join("ledger.json")).unwrap();
        assert!(!saved.contains("private source"));
        let parsed: Value = serde_json::from_str(&saved).unwrap();
        assert_eq!(parsed["format"], 2);
        assert!(parsed["ledger"]["receipts"]["completed"]["hot"].is_null());
        drop(store);
        let store = ScriptStore::open(dir.path()).unwrap();
        assert_eq!(
            store.receipt(&version.owner, "completed").unwrap(),
            completed
        );
        assert_eq!(
            store.receipt(&version.owner, "running").unwrap().status,
            ReceiptStatus::Unknown
        );
        assert!(store.begin(running).is_err());
        store
            .acknowledge_unknown(&version.owner, "running", "人工核对未执行，不重放")
            .unwrap();
        store
            .read(|ledger| {
                assert!(ledger.receipts["running"].hot.is_none());
                Ok(())
            })
            .unwrap();
    }
    #[test]
    fn corrupt_cold_source_is_preserved_and_refused_on_read_and_restart() {
        let dir = tempfile::tempdir().unwrap();
        let version = fixture_version();
        let store = ScriptStore::open(dir.path()).unwrap();
        store.publish(&version.owner, version.spec.clone()).unwrap();
        let blob = store
            .read(|ledger| Ok(ledger.versions[&version.reference.digest].blob.clone()))
            .unwrap();
        let path = dir
            .path()
            .join("blobs")
            .join(format!("{}.json", blob.digest));
        let mut corrupted = std::fs::read(&path).unwrap();
        let last = corrupted.len() - 1;
        corrupted[last] ^= 1;
        std::fs::write(&path, &corrupted).unwrap();
        assert!(store.version(&version.owner, &version.reference).is_err());
        // 清单只读已经校验的身份索引，不复制正文；清单不是执行授权。
        assert_eq!(store.summaries(&version.owner).unwrap().len(), 1);
        let foreign = ContextScope {
            session_id: "elsewhere".into(),
            user_id: "bob".into(),
        };
        assert!(store.summaries(&foreign).unwrap().is_empty());
        assert!(store.version(&foreign, &version.reference).is_err());
        assert!(store.publish(&version.owner, version.spec.clone()).is_err());
        drop(store);
        assert!(ScriptStore::open(dir.path()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), corrupted);
    }
}
