use super::*;
use crate::{QqAuthoredPushPage, QqGroupChimeDelivery, QqOutreachMessage, QqOutreachPush};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Source {
    rows: Vec<QqOutreachPush>,
    filtered: bool,
}
impl Source {
    fn new() -> Self {
        Self {
            rows: (0..72)
                .map(|i| QqOutreachPush {
                    session: Some(SessionKey::new(format!("group-{i}"), "eve-chime").unwrap()),
                    user_id: "eve-chime".into(),
                    message: QqOutreachMessage {
                        id: format!("chime-{i:03}"),
                        text: format!("Eve 决定的第 {i} 条"),
                    },
                })
                .collect(),
            filtered: false,
        }
    }
}
impl QqOutreach for Source {
    fn authored_pushes(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> PluginResult<QqAuthoredPushPage> {
        let mut rows: Vec<_> = self
            .rows
            .iter()
            .filter(|p| after.is_none_or(|after| p.message.id.as_str() > after))
            .take(limit + 1)
            .cloned()
            .collect();
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next = more.then(|| rows.last().unwrap().message.id.clone());
        if self.filtered {
            rows.retain(|p| p.session.as_ref().unwrap().session_id == "group-70");
        }
        Ok(QqAuthoredPushPage { pushes: rows, next })
    }
    fn group_chime(&self, id: &str) -> PluginResult<Option<QqGroupChimeDelivery>> {
        if id == "chime-000" {
            return Err(failure("故障来源"));
        }
        Ok(self
            .rows
            .iter()
            .find(|p| p.message.id == id)
            .map(|p| QqGroupChimeDelivery {
                session: p.session.clone().unwrap(),
                speaker_id: "member".into(),
                anchor: Some(format!("anchor-{id}")),
                parts: vec![(p.message.text.clone(), 0)],
            }))
    }
    fn claim_authored_push(&self, _: &QqOutreachPush) -> PluginResult<bool> {
        panic!("选择路由不能认领")
    }
    fn due(&self, _: &SessionKey) -> PluginResult<bool> {
        Ok(false)
    }
    fn judge(&self, _: QqOutreachMoment) -> crate::QqOutreachFuture {
        Box::pin(async { Ok(false) })
    }
    fn attach(&self, _: &SessionKey, _: &str) -> PluginResult<Option<QqOutreachMessage>> {
        panic!("不能回退自动推送")
    }
    fn next_push(&self) -> PluginResult<Option<QqOutreachPush>> {
        panic!("不能回退自动推送")
    }
    fn delivered(&self, _: &str, _: QqOutreachResult) -> PluginResult<()> {
        panic!("只读阶段不能伪造送达")
    }
}

#[test]
fn missing_routes_and_broken_metadata_cannot_block_a_later_group_or_claim_it() {
    let source = Source::new();
    let mut cursor = None;
    let warnings = AtomicUsize::new(0);
    let route = |group: &str| (group == "group-70").then(|| "actual-group-70".into());
    let unavailable = || {
        warnings.fetch_add(1, Ordering::SeqCst);
    };
    assert!(select_authored_push(&source, &mut cursor, route, unavailable).is_none());
    assert_eq!(cursor.as_deref(), Some("chime-063"));
    assert_eq!(warnings.load(Ordering::SeqCst), 1);
    let found = select_authored_push(&source, &mut cursor, route, unavailable).unwrap();
    assert_eq!(found.push.message.id, "chime-070");
    assert_eq!(found.push.message.text, "Eve 决定的第 70 条");
    assert_eq!(found.anchor, "anchor-chime-070");
    assert_eq!(found.target, "actual-group-70");
    assert!(select_authored_push(&source, &mut cursor, route, unavailable).is_none());
    assert_eq!(cursor, None);
    let recovered = select_authored_push(
        &source,
        &mut cursor,
        |group| (group == "group-1").then(|| "route-restored".into()),
        unavailable,
    )
    .unwrap();
    assert_eq!(recovered.push.message.id, "chime-001");
}

#[test]
fn an_empty_permission_filtered_page_keeps_the_cursor_for_the_next_group() {
    let mut source = Source::new();
    source.filtered = true;
    let mut cursor = None;
    assert!(
        select_authored_push(
            &source,
            &mut cursor,
            |s| Some(s.into()),
            || panic!("有效分页")
        )
        .is_none()
    );
    assert_eq!(cursor.as_deref(), Some("chime-063"));
    let found = select_authored_push(
        &source,
        &mut cursor,
        |s| Some(s.into()),
        || panic!("有效分页"),
    )
    .unwrap();
    assert_eq!(found.push.message.id, "chime-070");
}
