use eve_budget_api::*;
use eve_budget_plugin::{BudgetController, BudgetPlugin, BudgetedResolver};
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::{
    ChatMessage, ChatRole, LlmError, LlmFuture, LlmModelResolver, LlmProvider, ModelRequest,
    ModelResponse, ModelSelection,
};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

const HOUR: u64 = 3_600_000;
/// 某个整点之后 10 分钟；窗口按纪元对齐。
const T0: u64 = 1_000 * HOUR + 600_000;

struct Store {
    inner: FileStateStore,
    fail: AtomicBool,
}
impl StateStore for Store {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.inner.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(PluginError::State("private-storage-detail".into()));
        }
        self.inner.set(namespace, key, value)
    }
}

struct World {
    _directory: tempfile::TempDir,
    state: Arc<Store>,
}
impl World {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(Store {
            inner: FileStateStore::open(directory.path().join("state")).unwrap(),
            fail: AtomicBool::new(false),
        });
        Self {
            _directory: directory,
            state,
        }
    }
    async fn open(&self, policy: BudgetPolicy) -> PluginResult<(Kernel, BudgetController)> {
        let kernel = Kernel::with_services(KernelServices {
            state: self.state.clone(),
            ..KernelServices::default()
        });
        let plugin = BudgetPlugin::new(policy)?;
        let controller = plugin.controller();
        kernel.register(Box::new(plugin))?;
        kernel.start_all().await?;
        Ok((kernel, controller))
    }
    fn raw(&self) -> Option<Vec<u8>> {
        self.state
            .get(&PluginId::new(BUDGET_PLUGIN_ID).unwrap(), BUDGET_STATE_KEY)
            .unwrap()
    }
    fn write(&self, bytes: Vec<u8>) {
        self.state
            .inner
            .set(
                &PluginId::new(BUDGET_PLUGIN_ID).unwrap(),
                BUDGET_STATE_KEY.into(),
                bytes,
            )
            .unwrap();
    }
}

fn hourly(limit: u32) -> BudgetPolicy {
    BudgetPolicy {
        window_ms: HOUR,
        limit,
    }
}

fn categories(window: &BudgetWindow) -> BTreeMap<&str, u32> {
    window
        .by_category
        .iter()
        .map(|(category, count)| (category.as_str(), *count))
        .collect()
}

#[tokio::test]
async fn usage_is_persisted_before_admission_survives_restart_and_rolls_by_window() {
    let world = World::new();
    let (kernel, budget) = world.open(hourly(3)).await.unwrap();
    let start = 1_000 * HOUR;
    assert_eq!(budget.snapshot(T0).unwrap().remaining(), 3);
    assert_eq!(budget.reserve("interest", 1, T0).unwrap(), 2);
    assert_eq!(budget.reserve("research", 2, T0 + 1).unwrap(), 0);
    // 用尽：不计数，给出本窗口的重置时间。
    assert_eq!(
        budget.reserve("interest", 1, T0 + 2),
        Err(BudgetError::Exhausted {
            resets_at_ms: start + HOUR
        })
    );
    assert_eq!(
        budget.reserve("bad category!", 1, T0),
        Err(BudgetError::InvalidInput)
    );
    assert_eq!(
        budget.reserve("interest", 0, T0),
        Err(BudgetError::InvalidInput)
    );
    kernel.stop_all().await.unwrap();
    assert_eq!(budget.snapshot(T0), Err(BudgetError::Unavailable));

    // 重启后同一窗口的计数不变；调高上限立即生效于当前窗口。
    let (kernel, budget) = world.open(hourly(5)).await.unwrap();
    let snapshot = budget.snapshot(T0 + 3).unwrap();
    assert_eq!(snapshot.current.start_ms, start);
    assert_eq!(snapshot.current.used, 3);
    assert_eq!(
        categories(&snapshot.current),
        BTreeMap::from([("interest", 1), ("research", 2)])
    );
    assert_eq!(
        (snapshot.remaining(), snapshot.resets_at_ms),
        (2, start + HOUR)
    );
    // 时钟回拨到更早的窗口：沿用已记录的窗口，不重新计数。
    assert_eq!(budget.reserve("practice", 1, T0 - 2 * HOUR).unwrap(), 1);
    assert_eq!(budget.snapshot(T0).unwrap().current.used, 4);

    // 进入下一个窗口：旧窗口进入历史，新窗口从零计数。
    assert_eq!(budget.reserve("outreach", 1, start + HOUR + 5).unwrap(), 4);
    let snapshot = budget.snapshot(start + HOUR + 6).unwrap();
    assert_eq!(snapshot.current.start_ms, start + HOUR);
    assert_eq!(snapshot.history.len(), 1);
    assert_eq!(snapshot.history[0].used, 4);
    // 只查看不写入：跨过多个空窗口后，历史不出现空窗口。
    let later = budget.snapshot(start + 5 * HOUR).unwrap();
    assert_eq!((later.current.used, later.history.len()), (0, 2));
    kernel.stop_all().await.unwrap();

    // 改变窗口长度：当前窗口进入历史，按新长度重新对齐计数。
    let (kernel, budget) = world
        .open(BudgetPolicy {
            window_ms: 24 * HOUR,
            limit: 2,
        })
        .await
        .unwrap();
    let snapshot = budget.snapshot(start + HOUR + 10).unwrap();
    assert_eq!(snapshot.current.used, 0);
    assert_eq!(
        snapshot.current.start_ms,
        (start + HOUR) / (24 * HOUR) * 24 * HOUR
    );
    assert_eq!(snapshot.history.len(), 2);
    assert_eq!(budget.reserve("interest", 2, start + HOUR + 10).unwrap(), 0);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn history_is_bounded_and_newest_first() {
    let world = World::new();
    let (kernel, budget) = world.open(hourly(1)).await.unwrap();
    for hour in 0..(MAX_HISTORY as u64 + 3) {
        budget.reserve("interest", 1, T0 + hour * HOUR).unwrap();
    }
    let snapshot = budget
        .snapshot(T0 + (MAX_HISTORY as u64 + 3) * HOUR)
        .unwrap();
    assert_eq!(snapshot.history.len(), MAX_HISTORY);
    assert!(
        snapshot
            .history
            .windows(2)
            .all(|pair| pair[0].start_ms > pair[1].start_ms)
    );
    kernel.stop_all().await.unwrap();
}

struct Echo {
    calls: AtomicUsize,
}
impl LlmProvider for Echo {
    fn complete(&self, _request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(ModelResponse::Final {
                text: "好的".into(),
            })
        })
    }
}
struct Fixed(Arc<Echo>);
impl LlmModelResolver for Fixed {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        Ok(ModelSelection {
            provider: self.0.clone(),
            provider_timeout: Duration::from_secs(5),
        })
    }
}

fn request() -> ModelRequest {
    ModelRequest {
        messages: vec![ChatMessage::text(ChatRole::User, "你好")],
        tools: vec![],
    }
}

#[tokio::test]
async fn budgeted_requests_are_counted_by_category_and_refused_unsent_when_exhausted() {
    let world = World::new();
    // 长窗口使测试在真实时钟下不会跨窗口。
    let (kernel, budget) = world
        .open(BudgetPolicy {
            window_ms: MAX_WINDOW_MS,
            limit: 2,
        })
        .await
        .unwrap();
    let echo = Arc::new(Echo {
        calls: AtomicUsize::new(0),
    });
    let budget: Arc<dyn AutonomyBudget> = Arc::new(budget);
    assert!(
        BudgetedResolver::new(Arc::new(Fixed(echo.clone())), budget.clone(), "坏 分类").is_err()
    );
    let interest =
        BudgetedResolver::new(Arc::new(Fixed(echo.clone())), budget.clone(), "interest").unwrap();
    let practice =
        BudgetedResolver::new(Arc::new(Fixed(echo.clone())), budget.clone(), "practice").unwrap();
    let selection = interest.resolve().unwrap();
    assert_eq!(selection.provider_timeout, Duration::from_secs(5));
    selection.provider.complete(request()).await.unwrap();
    practice
        .resolve()
        .unwrap()
        .provider
        .complete(request())
        .await
        .unwrap();
    let refused = interest
        .resolve()
        .unwrap()
        .provider
        .complete(request())
        .await;
    assert!(matches!(refused, Err(LlmError::Provider(message)) if message.contains("已用尽")));
    assert_eq!(echo.calls.load(Ordering::SeqCst), 2, "用尽后不发出请求");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let current = budget.snapshot(now).unwrap().current;
    assert_eq!(
        categories(&current),
        BTreeMap::from([("interest", 1), ("practice", 1)])
    );
    kernel.stop_all().await.unwrap();
    // 实例关闭后不再放行。
    let closed = practice
        .resolve()
        .unwrap()
        .provider
        .complete(request())
        .await;
    assert!(matches!(closed, Err(LlmError::Provider(message)) if message.contains("不可用")));
    assert_eq!(echo.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn storage_failure_closes_the_instance_and_reopen_reads_the_committed_ledger() {
    let world = World::new();
    let (kernel, budget) = world.open(hourly(5)).await.unwrap();
    budget.reserve("interest", 1, T0).unwrap();
    world.state.fail.store(true, Ordering::SeqCst);
    assert_eq!(budget.reserve("interest", 1, T0), Err(BudgetError::Storage));
    world.state.fail.store(false, Ordering::SeqCst);
    // 写入失败后实例关闭，之后一律拒绝，须重新打开核对。
    assert_eq!(
        budget.reserve("interest", 1, T0),
        Err(BudgetError::Unavailable)
    );
    assert_eq!(budget.snapshot(T0), Err(BudgetError::Unavailable));
    kernel.stop_all().await.unwrap();
    let (kernel, budget) = world.open(hourly(5)).await.unwrap();
    assert_eq!(budget.snapshot(T0).unwrap().current.used, 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn corrupt_or_unknown_ledgers_are_refused_and_preserved() {
    assert!(BudgetPlugin::new(hourly(0)).is_err());
    assert!(
        BudgetPlugin::new(BudgetPolicy {
            window_ms: 1_000,
            limit: 1
        })
        .is_err()
    );
    let world = World::new();
    let (kernel, budget) = world.open(hourly(5)).await.unwrap();
    budget.reserve("interest", 1, T0).unwrap();
    budget.reserve("interest", 1, T0 + HOUR).unwrap();
    kernel.stop_all().await.unwrap();
    let valid: serde_json::Value = serde_json::from_slice(&world.raw().unwrap()).unwrap();
    let mutate = |change: &dyn Fn(&mut serde_json::Value)| {
        let mut value = valid.clone();
        change(&mut value);
        serde_json::to_vec(&value).unwrap()
    };
    let cases = vec![
        b"{\"format_version\":1,\"format_version\":1}".to_vec(),
        b"not json".to_vec(),
        serde_json::to_vec(&serde_json::json!({"format_version": 2})).unwrap(),
        mutate(&|value| value["current"]["used"] = 7.into()),
        mutate(&|value| value["current"]["by_category"]["interest"] = 0.into()),
        mutate(&|value| value["current"]["by_category"] = serde_json::json!({"坏 分类": 1})),
        mutate(&|value| value["history"][0]["start_ms"] = value["current"]["start_ms"].clone()),
        mutate(&|value| value["policy"]["limit"] = 0.into()),
        mutate(&|value| value["extra"] = 1.into()),
    ];
    for case in cases {
        world.write(case.clone());
        assert!(
            world.open(hourly(5)).await.is_err(),
            "{}",
            String::from_utf8_lossy(&case)
        );
        assert_eq!(world.raw().unwrap(), case, "损坏的账本原样保留");
    }
    world.write(serde_json::to_vec(&valid).unwrap());
    let (kernel, budget) = world.open(hourly(5)).await.unwrap();
    assert_eq!(budget.snapshot(T0 + HOUR).unwrap().current.used, 1);
    kernel.stop_all().await.unwrap();
}
