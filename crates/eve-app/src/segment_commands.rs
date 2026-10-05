//! `/segment` 命令：按会话查看和修改分段设置。只做有界本地状态操作，不调用模型。
//! QQ 与终端共用同一套语法和回复，作用域由宿主绑定，不从正文读取身份。
use eve_plugin_api::{PluginError, PluginResult};
use eve_qqbot_plugin::{QqCommandHandler, QqCommandInput, segment_scope};
use eve_segment_api::{
    DEFAULT_PAUSE_PERCENT, MAX_PAUSE_PERCENT, MIN_PREFERRED_SEGMENTS, SegmentChange, SegmentPolicy,
    SegmentPreference, SegmentPreferenceError, SegmentPreferences, SegmentScope,
};
use std::sync::Arc;

pub(crate) const QQ_DISABLED: &str =
    "分段投递未开启：通道启动时未加 --segmented，回复始终整条发送。";
pub(crate) const CONSOLE_DISABLED: &str = "分段显示未开启：以 --segmented 启动后可按会话设置。";
pub(crate) const CONSOLE_STATE_FAILURE: &str =
    "分段设置保存结果无法确认；已停止接收输入，请重新启动后用 /segment 查看。";
const QQ_STATE_FAILURE: &str = "分段设置状态无法确认；服务已停止，请重新启动后发送 /segment 查看。";
const LIMIT_REACHED: &str = "分段设置容量已满；原设置保留，本次没有保存。";
const OFF_NOTE: &str = "当前分段已关闭，发送 /segment on 后生效。";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SegmentCommand<'a> {
    Status,
    Help,
    Set(SegmentChange),
    Suggestions(usize),
    Adopt(&'a str, u64),
}
impl<'a> SegmentCommand<'a> {
    /// 首个词必须正好是 `/segment`；参数小写精确匹配，其他写法回复用法。
    pub(crate) fn parse(text: &'a str) -> Option<Self> {
        let mut words = text.split_whitespace();
        if words.next()? != "/segment" {
            return None;
        }
        let rest: Vec<&str> = words.collect();
        Some(match rest.as_slice() {
            [] | ["status"] => Self::Status,
            ["on"] => Self::Set(SegmentChange::Enabled(true)),
            ["off"] => Self::Set(SegmentChange::Enabled(false)),
            ["reset"] => Self::Set(SegmentChange::Reset),
            ["suggestions"] => Self::Suggestions(1),
            ["suggestions", page] => page
                .parse()
                .ok()
                .filter(|p| *p > 0)
                .map_or(Self::Help, Self::Suggestions),
            ["adopt", ..] => {
                // 公共偏好 ID 可包含非控制空白，保持原 ID，只把最后一个词当作版本。
                let body = text
                    .trim()
                    .strip_prefix("/segment")
                    .unwrap()
                    .trim_start()
                    .strip_prefix("adopt")
                    .unwrap()
                    .trim();
                body.rsplit_once(char::is_whitespace)
                    .and_then(|(id, revision)| {
                        revision
                            .parse()
                            .ok()
                            .filter(|r| *r > 0)
                            .map(|r| Self::Adopt(id.trim_end(), r))
                    })
                    .unwrap_or(Self::Help)
            }
            ["parts", n] => n
                .parse()
                .map_or(Self::Help, |n| Self::Set(SegmentChange::MaxSegments(n))),
            ["pace", p] => p
                .trim_end_matches(['%', '％'])
                .parse()
                .map_or(Self::Help, |p| Self::Set(SegmentChange::PausePercent(p))),
            _ => Self::Help,
        })
    }
}

fn seconds(ms: u64) -> String {
    let text = format!("{:.2}", ms as f64 / 1000.0);
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}
fn help(policy: &SegmentPolicy) -> String {
    format!(
        "分段命令：/segment 查看；/segment on 开启；/segment off 整条发送；/segment parts 段数（{MIN_PREFERRED_SEGMENTS} 至 {}，默认 {}）；/segment pace 百分比（0 至 {MAX_PAUSE_PERCENT}，默认 {DEFAULT_PAUSE_PERCENT}，0 为不停顿）；/segment reset 恢复默认；QQ 开启记忆后用 /segment suggestions [页码] 查看建议，/segment adopt 偏好ID 版本 明确采用。只影响本会话，从下一条回复生效。",
        policy.max_segments, policy.defaults.max_segments
    )
}
pub(crate) fn status(preference: &SegmentPreference, policy: &SegmentPolicy) -> String {
    let source = |set: bool| if set { "已设置" } else { "默认" };
    let Ok(effective) = preference.effective(policy) else {
        return help(policy);
    };
    let mode = match preference.enabled {
        Some(false) => "关闭，整条发送（已设置）",
        Some(true) => "开启（已设置）",
        None => "开启（默认）",
    };
    format!(
        "本会话分段：{mode}；最多 {} 段（{}）；段间停顿 {}%（{}），单次不超过 {} 秒。设置只影响本会话，从下一条回复生效；发送 /segment help 查看命令。",
        effective.limits.max_segments,
        source(preference.max_segments.is_some()),
        effective.pause_percent,
        source(preference.pause_percent.is_some()),
        seconds(effective.limits.max_pause_ms),
    )
}

/// 只有无法确认持久状态时返回错误；输入或容量问题作为回复返回。
pub(crate) fn execute(
    store: &dyn SegmentPreferences,
    scope: &SegmentScope,
    policy: &SegmentPolicy,
    command: SegmentCommand<'_>,
) -> Result<String, SegmentPreferenceError> {
    let change = match command {
        SegmentCommand::Help => return Ok(help(policy)),
        SegmentCommand::Status => return Ok(status(&store.get(scope)?, policy)),
        SegmentCommand::Set(change) => change,
        SegmentCommand::Suggestions(_) | SegmentCommand::Adopt(_, _) => {
            return Ok(
                "节奏建议当前由 QQ 的交互记忆提供；终端可用 /segment parts 和 pace 直接设置。"
                    .into(),
            );
        }
    };
    // 先按宿主上限检查，越界时不访问存储。
    match change {
        SegmentChange::MaxSegments(n)
            if !(MIN_PREFERRED_SEGMENTS..=policy.max_segments).contains(&n) =>
        {
            return Ok(format!(
                "段数须为 {MIN_PREFERRED_SEGMENTS} 至 {} 的整数；需要整条发送请用 /segment off。设置未改变。",
                policy.max_segments
            ));
        }
        SegmentChange::PausePercent(p) if p > MAX_PAUSE_PERCENT => {
            return Ok(format!(
                "停顿比例须为 0 至 {MAX_PAUSE_PERCENT} 的整数（默认 {DEFAULT_PAUSE_PERCENT}，0 为不停顿）。设置未改变。"
            ));
        }
        SegmentChange::Patch(patch)
            if patch.validate().is_err()
                || patch.max_segments.is_some_and(|n| n > policy.max_segments) =>
        {
            return Ok(help(policy));
        }
        _ => {}
    }
    let next = match store.update(scope, change) {
        Ok(next) => next,
        Err(SegmentPreferenceError::InvalidInput) => return Ok(help(policy)),
        Err(SegmentPreferenceError::LimitReached) => return Ok(LIMIT_REACHED.into()),
        Err(error) => return Err(error),
    };
    let effective = next
        .effective(policy)
        .map_err(|_| SegmentPreferenceError::CorruptState)?;
    let off = if effective.enabled { "" } else { OFF_NOTE };
    Ok(match change {
        SegmentChange::Enabled(true) => format!(
            "已开启本会话分段：下一条回复起按自然段至多 {} 段发送。",
            effective.limits.max_segments
        ),
        SegmentChange::Enabled(false) => {
            "已关闭本会话分段：下一条回复起整条发送；发送 /segment on 可重新开启。".into()
        }
        SegmentChange::MaxSegments(n) => {
            format!("已将本会话分段上限设为 {n} 段，从下一条回复生效。{off}")
        }
        SegmentChange::PausePercent(p) => format!(
            "已将本会话段间停顿设为 {p}%（单次不超过 {} 秒），从下一条回复生效。{off}",
            seconds(effective.limits.max_pause_ms)
        ),
        SegmentChange::Reset if !next.is_default() => format!(
            "已清除本会话手动设置，恢复跟随有效学习偏好。{}",
            status(&next, policy)
        ),
        SegmentChange::Reset => format!(
            "已恢复本会话默认分段：开启，最多 {} 段，段间停顿 {DEFAULT_PAUSE_PERCENT}%，从下一条回复生效。",
            policy.defaults.max_segments
        ),
        SegmentChange::Patch(_) => status(&next, policy),
    })
}

/// QQ 宿主命令；未开启分段时仍识别 `/segment`，回复未开启而不交给模型。
pub(crate) struct QqCommands {
    store: Option<(Arc<dyn SegmentPreferences>, SegmentPolicy)>,
    advice: Option<crate::segment_advice::Commands>,
}
impl QqCommands {
    pub(crate) fn new(store: Arc<dyn SegmentPreferences>, policy: SegmentPolicy) -> Arc<Self> {
        Arc::new(Self {
            store: Some((store, policy)),
            advice: None,
        })
    }
    pub(crate) fn disabled() -> Arc<Self> {
        Arc::new(Self {
            store: None,
            advice: None,
        })
    }
    pub(crate) fn new_with_advice(
        store: Arc<dyn SegmentPreferences>,
        policy: SegmentPolicy,
        memory: Arc<dyn eve_memory_api::MemoryAdmin>,
        advisor: Arc<dyn eve_segment_api::SegmentAdvisor>,
        automatic: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            store: Some((store, policy)),
            advice: Some(crate::segment_advice::Commands::new(
                memory, advisor, automatic,
            )),
        })
    }
}
impl QqCommandHandler for QqCommands {
    fn handle(&self, input: QqCommandInput<'_>) -> PluginResult<Option<String>> {
        let Some(command) = SegmentCommand::parse(input.text) else {
            return Ok(None);
        };
        let Some((store, policy)) = &self.store else {
            return Ok(Some(QQ_DISABLED.into()));
        };
        if matches!(
            command,
            SegmentCommand::Suggestions(_) | SegmentCommand::Adopt(_, _)
        ) {
            return match &self.advice {
                Some(advice) => advice
                    .execute(&input, store.as_ref(), policy, command)
                    .map(Some),
                None => Ok(Some(
                    "节奏建议需要同时启用 --memory 与 --segmented。".into(),
                )),
            };
        }
        execute(
            store.as_ref(),
            &segment_scope(input.session),
            policy,
            command,
        )
        .map(Some)
        .map_err(|_| PluginError::State(QQ_STATE_FAILURE.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_qqbot_plugin::{QQ_SEGMENT_POLICY, QqCommandInput};
    use eve_session_api::SessionKey;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        value: Mutex<SegmentPreference>,
        calls: Mutex<usize>,
        fail: Mutex<Option<SegmentPreferenceError>>,
    }
    impl SegmentPreferences for Fake {
        fn get(&self, _: &SegmentScope) -> Result<SegmentPreference, SegmentPreferenceError> {
            *self.calls.lock().unwrap() += 1;
            match *self.fail.lock().unwrap() {
                Some(error) => Err(error),
                None => Ok(*self.value.lock().unwrap()),
            }
        }
        fn update(
            &self,
            _: &SegmentScope,
            change: SegmentChange,
        ) -> Result<SegmentPreference, SegmentPreferenceError> {
            *self.calls.lock().unwrap() += 1;
            if let Some(error) = *self.fail.lock().unwrap() {
                return Err(error);
            }
            let mut value = self.value.lock().unwrap();
            *value = value.apply(change)?;
            Ok(*value)
        }
    }
    fn scope() -> SegmentScope {
        SegmentScope {
            channel: "qq".into(),
            session_id: "s".into(),
            user_id: "u".into(),
        }
    }
    fn run(store: &Fake, text: &str) -> Result<String, SegmentPreferenceError> {
        execute(
            store,
            &scope(),
            &QQ_SEGMENT_POLICY,
            SegmentCommand::parse(text).unwrap(),
        )
    }

    #[test]
    fn parse_requires_exact_first_word_and_maps_arguments() {
        for text in [
            "/segmentation",
            "正文 /segment off",
            "segment off",
            "/Segment",
            "",
        ] {
            assert_eq!(SegmentCommand::parse(text), None, "{text}");
        }
        let set = |change| Some(SegmentCommand::Set(change));
        for (text, expected) in [
            ("/segment", Some(SegmentCommand::Status)),
            ("  /segment   status ", Some(SegmentCommand::Status)),
            ("/segment on", set(SegmentChange::Enabled(true))),
            ("/segment off", set(SegmentChange::Enabled(false))),
            ("/segment reset", set(SegmentChange::Reset)),
            ("/segment parts 4", set(SegmentChange::MaxSegments(4))),
            ("/segment pace 50%", set(SegmentChange::PausePercent(50))),
            ("/segment pace 150％", set(SegmentChange::PausePercent(150))),
            ("/segment pace 0", set(SegmentChange::PausePercent(0))),
            ("/segment suggestions", Some(SegmentCommand::Suggestions(1))),
            (
                "/segment suggestions 2",
                Some(SegmentCommand::Suggestions(2)),
            ),
            (
                "/segment adopt source 3",
                Some(SegmentCommand::Adopt("source", 3)),
            ),
            (
                "/segment adopt source with spaces 3",
                Some(SegmentCommand::Adopt("source with spaces", 3)),
            ),
            ("/segment suggestions 0", Some(SegmentCommand::Help)),
            ("/segment adopt source 0", Some(SegmentCommand::Help)),
            ("/segment help", Some(SegmentCommand::Help)),
            ("/segment parts", Some(SegmentCommand::Help)),
            ("/segment parts two", Some(SegmentCommand::Help)),
            ("/segment pace -1", Some(SegmentCommand::Help)),
            ("/segment off now", Some(SegmentCommand::Help)),
            ("/segment 关闭", Some(SegmentCommand::Help)),
        ] {
            assert_eq!(SegmentCommand::parse(text), expected, "{text}");
        }
    }

    #[test]
    fn replies_describe_effective_settings_and_rejections_skip_storage() {
        let store = Fake::default();
        assert_eq!(
            run(&store, "/segment").unwrap(),
            "本会话分段：开启（默认）；最多 3 段（默认）；段间停顿 100%（默认），单次不超过 2.5 秒。设置只影响本会话，从下一条回复生效；发送 /segment help 查看命令。"
        );
        assert_eq!(
            run(&store, "/segment off").unwrap(),
            "已关闭本会话分段：下一条回复起整条发送；发送 /segment on 可重新开启。"
        );
        assert_eq!(
            run(&store, "/segment parts 5").unwrap(),
            "已将本会话分段上限设为 5 段，从下一条回复生效。当前分段已关闭，发送 /segment on 后生效。"
        );
        assert_eq!(
            run(&store, "/segment on").unwrap(),
            "已开启本会话分段：下一条回复起按自然段至多 5 段发送。"
        );
        assert_eq!(
            run(&store, "/segment pace 200").unwrap(),
            "已将本会话段间停顿设为 200%（单次不超过 5 秒），从下一条回复生效。"
        );
        assert_eq!(
            run(&store, "/segment pace 50").unwrap(),
            "已将本会话段间停顿设为 50%（单次不超过 1.25 秒），从下一条回复生效。"
        );
        assert_eq!(
            run(&store, "/segment status").unwrap(),
            "本会话分段：开启（已设置）；最多 5 段（已设置）；段间停顿 50%（已设置），单次不超过 1.25 秒。设置只影响本会话，从下一条回复生效；发送 /segment help 查看命令。"
        );
        let calls = *store.calls.lock().unwrap();
        for (text, reply) in [
            (
                "/segment parts 1",
                "段数须为 2 至 5 的整数；需要整条发送请用 /segment off。设置未改变。",
            ),
            (
                "/segment parts 6",
                "段数须为 2 至 5 的整数；需要整条发送请用 /segment off。设置未改变。",
            ),
            (
                "/segment pace 201",
                "停顿比例须为 0 至 200 的整数（默认 100，0 为不停顿）。设置未改变。",
            ),
        ] {
            assert_eq!(run(&store, text).unwrap(), reply);
        }
        assert!(
            run(&store, "/segment help")
                .unwrap()
                .starts_with("分段命令：")
        );
        assert_eq!(*store.calls.lock().unwrap(), calls);
        assert_eq!(
            run(&store, "/segment reset").unwrap(),
            "已恢复本会话默认分段：开启，最多 3 段，段间停顿 100%，从下一条回复生效。"
        );
        assert!(store.value.lock().unwrap().is_default());
    }

    #[test]
    fn storage_failures_stop_while_capacity_is_a_reply() {
        let store = Fake::default();
        *store.fail.lock().unwrap() = Some(SegmentPreferenceError::LimitReached);
        assert_eq!(run(&store, "/segment off").unwrap(), LIMIT_REACHED);
        for error in [
            SegmentPreferenceError::Storage,
            SegmentPreferenceError::Unavailable,
            SegmentPreferenceError::CorruptState,
        ] {
            *store.fail.lock().unwrap() = Some(error);
            assert_eq!(run(&store, "/segment off"), Err(error));
            assert_eq!(run(&store, "/segment"), Err(error));
        }
        let key = SessionKey::new("qq:session", "qq:user").unwrap();
        let input = |text| QqCommandInput {
            message_id: "m-1",
            session: &key,
            text,
        };
        let failing = Arc::new(Fake::default());
        *failing.fail.lock().unwrap() = Some(SegmentPreferenceError::Storage);
        let commands = QqCommands::new(failing.clone(), QQ_SEGMENT_POLICY);
        assert!(matches!(
            commands.handle(input("/segment off")),
            Err(PluginError::State(_))
        ));
        assert_eq!(commands.handle(input("/remember x")).unwrap(), None);
        assert_eq!(
            QqCommands::disabled()
                .handle(input("/segment off"))
                .unwrap(),
            Some(QQ_DISABLED.into())
        );
    }
}
