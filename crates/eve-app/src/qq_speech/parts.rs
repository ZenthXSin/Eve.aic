//! 自主分条的完整发送快照；在途结果不明时封存后缀，绝不恢复发送。
use super::{Store, error, key, now};
use eve_llm_api::{InputReplyDelivery, InputReplyPartDelivery};
use eve_plugin_api::PluginResult;
use eve_qqbot_plugin::{QqSpeechDecision, QqSpeechEvent, QqSpeechPartsReceipt};
use rusqlite::{Connection, params};

fn validate_plan(
    event: &QqSpeechEvent,
    decision: &QqSpeechDecision,
    receipt: &QqSpeechPartsReceipt,
) -> PluginResult<()> {
    super::validate_event(event).map_err(error)?;
    super::validate_record_decision(event, decision).map_err(error)?;
    let QqSpeechDecision::SpeakParts { parts, .. } = decision else {
        return Err(error("分条回执没有对应决定"));
    };
    receipt.validate().map_err(error)?;
    let text = parts
        .iter()
        .map(|part| part.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    if text.len() > 2048
        || receipt.text() != text
        || receipt.plan.first().is_none_or(|part| part.after_ms != 0)
    {
        return Err(error("实际分条计划改变原文或首段延后"));
    }
    Ok(())
}

fn receipt_state(receipt: &QqSpeechPartsReceipt) -> PluginResult<&'static str> {
    receipt.validate().map_err(error)?;
    let InputReplyDelivery::Parts { parts } = &receipt.delivery else {
        return Err(error("缺少分条快照"));
    };
    // 第一条在认领时已落盘 Sending；全 Planned 或全 NotAttempted 无法由它到达。
    if parts.first().is_none_or(|part| {
        matches!(
            part.delivery,
            InputReplyPartDelivery::Planned | InputReplyPartDelivery::NotAttempted
        )
    }) {
        return Err(error("分条快照缺少首次发送尝试"));
    }
    if !receipt.delivery.is_terminal() {
        return Ok("sending");
    }
    if parts
        .iter()
        .all(|part| matches!(part.delivery, InputReplyPartDelivery::Sent { .. }))
    {
        Ok("sent")
    } else if parts
        .iter()
        .any(|part| part.delivery == InputReplyPartDelivery::Unknown)
    {
        Ok("unknown")
    } else {
        Ok("failed")
    }
}

pub(super) fn validate_parts_record(
    event: &QqSpeechEvent,
    decision: &QqSpeechDecision,
    state: &str,
    receipt: Option<&str>,
) -> PluginResult<()> {
    super::validate_event(event).map_err(error)?;
    super::validate_record_decision(event, decision).map_err(error)?;
    if !matches!(decision, QqSpeechDecision::SpeakParts { .. }) {
        return Err(error("分条记录没有对应决定"));
    }
    let Some(receipt) = receipt else {
        return if matches!(state, "ready" | "stale") {
            Ok(())
        } else {
            Err(error("分条发送凭据缺失"))
        };
    };
    let receipt: QqSpeechPartsReceipt = serde_json::from_str(receipt).map_err(error)?;
    validate_plan(event, decision, &receipt)?;
    if state != receipt_state(&receipt)? {
        return Err(error("分条记录状态与快照不一致"));
    }
    Ok(())
}

impl Store {
    pub(super) fn dispatched_parts(
        &self,
        event: &QqSpeechEvent,
        receipt: &QqSpeechPartsReceipt,
    ) -> PluginResult<()> {
        receipt.validate().map_err(error)?;
        let InputReplyDelivery::Parts { parts } = &receipt.delivery else {
            return Err(error("缺少分条计划"));
        };
        if parts
            .first()
            .is_none_or(|part| part.delivery != InputReplyPartDelivery::Sending)
            || parts
                .iter()
                .skip(1)
                .any(|part| part.delivery != InputReplyPartDelivery::Planned)
        {
            return Err(error("首次分条快照必须登记首条在途与完整后缀"));
        }
        let db = self.connection.lock().map_err(error)?;
        let tx = db.unchecked_transaction().map_err(error)?;
        let encoded_event = serde_json::to_string(event).map_err(error)?;
        let id = key(event)?;
        let (decision, state, old_receipt): (String, String, Option<String>) = tx
            .query_row(
                "SELECT decision,state,receipt FROM speech WHERE id=?1 AND event=?2",
                params![id, encoded_event],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(error)?;
        let decision: QqSpeechDecision = serde_json::from_str(&decision).map_err(error)?;
        validate_parts_record(event, &decision, &state, old_receipt.as_deref())?;
        validate_plan(event, &decision, receipt)?;
        let changed = tx.execute(
            "UPDATE speech SET state='sending',receipt=?2,updated=?3 WHERE id=?1 AND event=?4 AND state='ready' AND receipt IS NULL AND due<=?3",
            params![id, serde_json::to_string(receipt).map_err(error)?, now(), encoded_event],
        ).map_err(error)?;
        if changed != 1 {
            return Err(error("分条决定未就绪或已经认领"));
        }
        tx.commit().map_err(error)
    }

    pub(super) fn update_parts(
        &self,
        event: &QqSpeechEvent,
        receipt: &QqSpeechPartsReceipt,
    ) -> PluginResult<()> {
        let db = self.connection.lock().map_err(error)?;
        let tx = db.unchecked_transaction().map_err(error)?;
        let encoded_event = serde_json::to_string(event).map_err(error)?;
        let id = key(event)?;
        let (decision, state, old_receipt): (String, String, String) = tx
            .query_row(
                "SELECT decision,state,receipt FROM speech WHERE id=?1 AND event=?2",
                params![id, encoded_event],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(error)?;
        let decision: QqSpeechDecision = serde_json::from_str(&decision).map_err(error)?;
        validate_parts_record(event, &decision, &state, Some(&old_receipt))?;
        validate_plan(event, &decision, receipt)?;
        let previous: QqSpeechPartsReceipt = serde_json::from_str(&old_receipt).map_err(error)?;
        if previous.plan != receipt.plan || !previous.delivery.permits_snapshot(&receipt.delivery) {
            return Err(error("分条发送计划改变或回执倒退"));
        }
        let (InputReplyDelivery::Parts { parts: old }, InputReplyDelivery::Parts { parts: next }) =
            (&previous.delivery, &receipt.delivery)
        else {
            return Err(error("缺少分条快照"));
        };
        // 跨层消费允许跳过中间快照；发送权威账本不允许跳过 Sending。
        if old.iter().zip(next).any(|(old, next)| {
            old.delivery == InputReplyPartDelivery::Planned
                && !matches!(
                    next.delivery,
                    InputReplyPartDelivery::Planned
                        | InputReplyPartDelivery::Sending
                        | InputReplyPartDelivery::NotAttempted
                )
        }) {
            return Err(error("分条尚未登记在途就写入发送结果"));
        }
        let next_state = receipt_state(receipt)?;
        if state != "sending" {
            return if previous == *receipt {
                Ok(())
            } else {
                Err(error("分条终态不可重开"))
            };
        }
        let changed = tx.execute(
            "UPDATE speech SET state=?2,receipt=?3,updated=?4 WHERE id=?1 AND event=?5 AND state='sending' AND receipt=?6",
            params![id, next_state, serde_json::to_string(receipt).map_err(error)?, now(), encoded_event, old_receipt],
        ).map_err(error)?;
        if changed != 1 {
            return Err(error("分条发送尝试不匹配"));
        }
        tx.commit().map_err(error)
    }
}

pub(super) fn recover_parts(db: &Connection) -> PluginResult<()> {
    let tx = db.unchecked_transaction().map_err(error)?;
    let rows: Vec<(String, String, String, Option<String>)> = {
        let mut query = tx
            .prepare("SELECT id,event,decision,receipt FROM speech WHERE state='sending'")
            .map_err(error)?;
        query
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .map_err(error)?
            .collect::<Result<_, _>>()
            .map_err(error)?
    };
    for (id, encoded_event, decision, encoded_receipt) in rows {
        let decision: QqSpeechDecision = serde_json::from_str(&decision).map_err(error)?;
        if !matches!(decision, QqSpeechDecision::SpeakParts { .. }) {
            continue;
        }
        let event: QqSpeechEvent = serde_json::from_str(&encoded_event).map_err(error)?;
        if key(&event)? != id {
            return Err(error("分条恢复身份不匹配"));
        }
        validate_parts_record(&event, &decision, "sending", encoded_receipt.as_deref())?;
        let mut receipt: QqSpeechPartsReceipt = serde_json::from_str(
            encoded_receipt
                .as_deref()
                .ok_or_else(|| error("回执缺失"))?,
        )
        .map_err(error)?;
        let InputReplyDelivery::Parts { parts } = &mut receipt.delivery else {
            return Err(error("缺少分条快照"));
        };
        for part in parts {
            part.delivery = match &part.delivery {
                InputReplyPartDelivery::Sending => InputReplyPartDelivery::Unknown,
                InputReplyPartDelivery::Planned => InputReplyPartDelivery::NotAttempted,
                terminal => terminal.clone(),
            };
        }
        let state = receipt_state(&receipt)?;
        validate_parts_record(
            &event,
            &decision,
            state,
            Some(&serde_json::to_string(&receipt).map_err(error)?),
        )?;
        let changed = tx.execute(
            "UPDATE speech SET state=?2,receipt=?3,updated=?4 WHERE id=?1 AND state='sending' AND receipt=?5",
            params![id, state, serde_json::to_string(&receipt).map_err(error)?, now(), encoded_receipt],
        ).map_err(error)?;
        if changed != 1 {
            return Err(error("分条恢复记录改变"));
        }
    }
    tx.commit().map_err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_llm_api::InputReplyPart;
    use eve_qqbot_plugin::QqSpeechPart;
    use eve_session_api::SessionKey;
    use serde_json::json;

    fn event(id: &str) -> QqSpeechEvent {
        QqSpeechEvent {
            id: id.into(),
            session: SessionKey {
                session_id: "qq:session".into(),
                user_id: "qq:owner".into(),
            },
            anchor: "input-1".into(),
            facts: json!({"kind":"task_result","result":"已核实"}),
        }
    }

    fn decision() -> QqSpeechDecision {
        QqSpeechDecision::SpeakParts {
            parts: vec![
                QqSpeechPart {
                    text: "第一项已经核实".into(),
                    after_ms: 0,
                },
                QqSpeechPart {
                    text: "第二项还在继续".into(),
                    after_ms: 100,
                },
                QqSpeechPart {
                    text: "有结果再接着说".into(),
                    after_ms: 200,
                },
            ],
            after_ms: 0,
        }
    }

    fn receipt() -> QqSpeechPartsReceipt {
        let QqSpeechDecision::SpeakParts { parts: plan, .. } = decision() else {
            unreachable!()
        };
        QqSpeechPartsReceipt {
            version: 2,
            delivery: InputReplyDelivery::Parts {
                parts: plan
                    .iter()
                    .enumerate()
                    .map(|(index, part)| InputReplyPart {
                        index,
                        text: part.text.clone(),
                        delivery: if index == 0 {
                            InputReplyPartDelivery::Sending
                        } else {
                            InputReplyPartDelivery::Planned
                        },
                    })
                    .collect(),
            },
            plan,
        }
    }

    fn set(receipt: &mut QqSpeechPartsReceipt, index: usize, delivery: InputReplyPartDelivery) {
        let InputReplyDelivery::Parts { parts } = &mut receipt.delivery else {
            unreachable!()
        };
        parts[index].delivery = delivery;
    }

    fn sent(id: &str) -> InputReplyPartDelivery {
        InputReplyPartDelivery::Sent {
            platform_message_id: id.into(),
        }
    }

    fn ready(store: &Store, event: &QqSpeechEvent) {
        store.enqueue(event).unwrap();
        store.save(event, "ready", Some(&decision()), 0).unwrap();
    }

    fn saved(store: &Store, event: &QqSpeechEvent) -> (String, Option<String>) {
        store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT state,receipt FROM speech WHERE id=?1",
                [key(event).unwrap()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    }

    #[test]
    fn dispatch_freezes_all_parts_before_any_result_and_rejects_reclaim() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("speech.db")).unwrap();
        let event = event("dispatch");
        ready(&store, &event);
        let mut wrong = receipt();
        wrong.plan[0].after_ms = 1;
        assert!(store.dispatched_parts(&event, &wrong).is_err());
        set(&mut wrong, 0, sent("too-early"));
        assert!(store.dispatched_parts(&event, &wrong).is_err());
        assert_eq!(saved(&store, &event), ("ready".into(), None));
        let receipt = receipt();
        store.dispatched_parts(&event, &receipt).unwrap();
        assert_eq!(
            saved(&store, &event),
            (
                "sending".into(),
                Some(serde_json::to_string(&receipt).unwrap())
            )
        );
        assert!(store.dispatched_parts(&event, &receipt).is_err());
    }

    #[test]
    fn each_part_requires_sending_snapshot_and_terminal_cannot_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("speech.db")).unwrap();
        let event = event("monotonic");
        ready(&store, &event);
        let mut receipt = receipt();
        assert!(store.update_parts(&event, &receipt).is_err());
        store.dispatched_parts(&event, &receipt).unwrap();
        set(&mut receipt, 0, sent("first"));
        store.update_parts(&event, &receipt).unwrap();
        set(&mut receipt, 1, sent("skipped-sending"));
        assert!(store.update_parts(&event, &receipt).is_err());
        set(&mut receipt, 1, InputReplyPartDelivery::Sending);
        store.update_parts(&event, &receipt).unwrap();
        set(&mut receipt, 1, sent("second"));
        set(&mut receipt, 2, InputReplyPartDelivery::Sending);
        store.update_parts(&event, &receipt).unwrap();
        set(&mut receipt, 2, sent("third"));
        store.update_parts(&event, &receipt).unwrap();
        assert_eq!(saved(&store, &event).0, "sent");
        store.update_parts(&event, &receipt).unwrap();
        set(&mut receipt, 2, InputReplyPartDelivery::Sending);
        assert!(store.update_parts(&event, &receipt).is_err());
        assert_eq!(saved(&store, &event).0, "sent");
    }

    #[test]
    fn merged_preference_preserves_original_text_and_frozen_delays() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("speech.db")).unwrap();
        let event = event("merged");
        ready(&store, &event);
        let mut receipt = receipt();
        let text = receipt.text();
        receipt.plan = vec![QqSpeechPart {
            text: text.clone(),
            after_ms: 0,
        }];
        receipt.delivery = InputReplyDelivery::Parts {
            parts: vec![InputReplyPart {
                index: 0,
                text,
                delivery: InputReplyPartDelivery::Sending,
            }],
        };
        store.dispatched_parts(&event, &receipt).unwrap();
        let mut changed = receipt.clone();
        changed.plan[0].after_ms = 1;
        assert!(store.update_parts(&event, &changed).is_err());
        set(&mut receipt, 0, sent("merged"));
        store.update_parts(&event, &receipt).unwrap();
        assert_eq!(saved(&store, &event).0, "sent");
    }

    #[test]
    fn recovery_preserves_sent_prefix_seals_unknown_and_never_queues_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("speech.db");
        let store = Store::open(&path).unwrap();
        let event = event("recover");
        ready(&store, &event);
        let mut receipt = receipt();
        store.dispatched_parts(&event, &receipt).unwrap();
        set(&mut receipt, 0, sent("first"));
        set(&mut receipt, 1, InputReplyPartDelivery::Sending);
        store.update_parts(&event, &receipt).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        set(&mut receipt, 1, InputReplyPartDelivery::Unknown);
        set(&mut receipt, 2, InputReplyPartDelivery::NotAttempted);
        assert_eq!(
            saved(&store, &event),
            (
                "unknown".into(),
                Some(serde_json::to_string(&receipt).unwrap())
            )
        );
        assert!(store.pending().unwrap().is_empty());
        assert!(store.dispatched_parts(&event, &receipt).is_err());
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(saved(&store, &event).0, "unknown");
    }

    #[test]
    fn recovery_without_inflight_marks_unsent_suffix_failed_not_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("speech.db")).unwrap();
        let event = event("between");
        ready(&store, &event);
        let mut receipt = receipt();
        store.dispatched_parts(&event, &receipt).unwrap();
        set(&mut receipt, 0, sent("first"));
        store.update_parts(&event, &receipt).unwrap();
        recover_parts(&store.connection.lock().unwrap()).unwrap();
        set(&mut receipt, 1, InputReplyPartDelivery::NotAttempted);
        set(&mut receipt, 2, InputReplyPartDelivery::NotAttempted);
        assert_eq!(
            saved(&store, &event),
            (
                "failed".into(),
                Some(serde_json::to_string(&receipt).unwrap())
            )
        );
        assert!(store.pending().unwrap().is_empty());
    }

    #[test]
    fn malformed_snapshot_is_not_recovered_or_silently_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("speech.db");
        let store = Store::open(&path).unwrap();
        let first = event("valid");
        let second = event("corrupt");
        for event in [&first, &second] {
            ready(&store, event);
            store.dispatched_parts(event, &receipt()).unwrap();
        }
        let before = saved(&store, &first);
        let broken = "{\"version\":2,\"plan\":[]}";
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE speech SET receipt=?2 WHERE id=?1",
                params![key(&second).unwrap(), broken],
            )
            .unwrap();
        assert!(recover_parts(&store.connection.lock().unwrap()).is_err());
        assert_eq!(saved(&store, &first), before);
        assert_eq!(
            saved(&store, &second),
            ("sending".into(), Some(broken.into()))
        );
        drop(store);
        assert!(Store::open(&path).is_err());
        let db = Connection::open(&path).unwrap();
        let unchanged: String = db
            .query_row(
                "SELECT state FROM speech WHERE id=?1",
                [key(&first).unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unchanged, "sending");
    }

    #[test]
    fn stored_state_must_match_complete_receipt_and_decision() {
        let event = event("validation");
        let decision = decision();
        for state in ["ready", "stale"] {
            validate_parts_record(&event, &decision, state, None).unwrap();
        }
        for state in ["sending", "sent", "failed", "unknown", "waiting", "pending"] {
            assert!(validate_parts_record(&event, &decision, state, None).is_err());
        }
        let receipt = serde_json::to_string(&receipt()).unwrap();
        validate_parts_record(&event, &decision, "sending", Some(&receipt)).unwrap();
        for state in ["sent", "failed", "unknown", "ready", "stale"] {
            assert!(validate_parts_record(&event, &decision, state, Some(&receipt)).is_err());
        }
    }

    fn schema_one(path: &std::path::Path) -> Connection {
        let db = Connection::open(path).unwrap();
        db.execute_batch(
            "CREATE TABLE speech (id TEXT PRIMARY KEY, event TEXT NOT NULL, decision TEXT, state TEXT NOT NULL CHECK(state IN ('pending','evaluating','waiting','ready','silence','sending','sent','failed','unknown','stale')), due INTEGER NOT NULL, updated INTEGER NOT NULL, receipt TEXT); PRAGMA user_version=1;",
        ).unwrap();
        db
    }

    fn raw_record(db: &Connection) -> String {
        db.query_row(
            "SELECT json_array(id,event,decision,state,due,updated,receipt) FROM speech",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn schema_one_single_message_migrates_without_rewriting_original_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("speech.db");
        let db = schema_one(&path);
        let event = event("legacy-single-sent");
        // 故意保留旧 JSON 的空白和字段顺序，迁移不能重写正文或凭据。
        let decision = r#"{ "text": "旧版说过的原话\n第二行", "after_ms": 0, "action": "speak" }"#;
        let receipt = r#"{ "message_id" : "legacy-platform-receipt" }"#;
        db.execute(
            "INSERT INTO speech (id,event,decision,state,due,updated,receipt) VALUES (?1,?2,?3,'sent',17,23,?4)",
            params![key(&event).unwrap(), serde_json::to_string(&event).unwrap(), decision, receipt],
        ).unwrap();
        let original = raw_record(&db);
        drop(db);

        let store = Store::open(&path).unwrap();
        {
            let db = store.connection.lock().unwrap();
            let schema: u32 = db
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .unwrap();
            assert_eq!(schema, 2);
            assert_eq!(raw_record(&db), original);
        }
        assert!(store.pending().unwrap().is_empty());
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(raw_record(&store.connection.lock().unwrap()), original);
    }

    #[test]
    fn schema_one_with_injected_parts_is_rejected_before_migration_or_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("speech.db");
        let db = schema_one(&path);
        let event = event("invalid-legacy-parts");
        db.execute(
            "INSERT INTO speech (id,event,decision,state,due,updated,receipt) VALUES (?1,?2,?3,'sending',17,23,?4)",
            params![key(&event).unwrap(), serde_json::to_string(&event).unwrap(), serde_json::to_string(&decision()).unwrap(), serde_json::to_string(&receipt()).unwrap()],
        ).unwrap();
        let original = raw_record(&db);
        drop(db);

        assert!(Store::open(&path).is_err());
        let db = Connection::open(&path).unwrap();
        let schema: u32 = db
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(schema, 1);
        assert_eq!(raw_record(&db), original);
        assert!(!path.with_extension("required").exists());
    }

    #[test]
    fn single_not_sent_can_be_reconsidered_as_parts_without_inheriting_receipt() {
        for rebind_anchor in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("speech.db");
            let store = Store::open(&path).unwrap();
            let mut event = event("retry-with-parts");
            store.enqueue(&event).unwrap();
            store
                .save(
                    &event,
                    "ready",
                    Some(&QqSpeechDecision::Speak {
                        text: "还没开始发送".into(),
                        after_ms: 0,
                    }),
                    0,
                )
                .unwrap();
            store.dispatched(&event).unwrap();
            store
                .delivered(&event, eve_qqbot_plugin::QqOutreachResult::NotSent)
                .unwrap();
            let (state, previous_receipt) = saved(&store, &event);
            assert_eq!(state, "waiting");
            assert!(previous_receipt.is_some());
            if rebind_anchor {
                event.anchor = "input-2".into();
                store.enqueue(&event).unwrap();
                assert_eq!(saved(&store, &event), ("pending".into(), None));
            }
            store.save(&event, "ready", Some(&decision()), 0).unwrap();
            assert_eq!(saved(&store, &event), ("ready".into(), None));
            drop(store);

            let store = Store::open(&path).unwrap();
            let mut receipt = receipt();
            store.dispatched_parts(&event, &receipt).unwrap();
            drop(store);

            let store = Store::open(&path).unwrap();
            set(&mut receipt, 0, InputReplyPartDelivery::Unknown);
            set(&mut receipt, 1, InputReplyPartDelivery::NotAttempted);
            set(&mut receipt, 2, InputReplyPartDelivery::NotAttempted);
            let terminal = (
                "unknown".into(),
                Some(serde_json::to_string(&receipt).unwrap()),
            );
            assert_eq!(saved(&store, &event), terminal);
            assert!(store.save(&event, "ready", Some(&decision()), 0).is_err());
            event.anchor = "input-after-unknown".into();
            store.enqueue(&event).unwrap();
            assert_eq!(saved(&store, &event), terminal);
        }
    }
}
