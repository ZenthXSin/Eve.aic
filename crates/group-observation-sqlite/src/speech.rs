//! 群自主发言：事实待审、模型三态和实际投递分开记账。
use super::*;
use eve_chime_api::{ChimeSpeechDecision, GroupAuthoredOutbound, GroupSpeechReview};
use rusqlite::TransactionBehavior;

const RETRY_MS: u64 = 60_000;
const LATEST: &str = "NOT EXISTS(SELECT 1 FROM observations newer WHERE newer.conversation=m.conversation AND newer.seq>m.seq AND json_extract(newer.payload,'$.from_me')=0) AND (SELECT platform_id FROM group_inputs WHERE conversation=m.conversation ORDER BY seq DESC LIMIT 1)=(SELECT platform_id FROM group_anchors WHERE conversation=m.conversation AND id=m.id)";

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |v| u64::try_from(v.as_millis()).unwrap_or(u64::MAX))
}
fn number(value: u64) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| "群自主发言时间或版本超限".into())
}
fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.trim() == value
        && !value.chars().any(char::is_control)
}
fn same_message(a: &GroupMessage, b: &GroupMessage) -> bool {
    let mut a = a.clone();
    // SDK 重发同一平台事件时接收时间可能变化，原持久接收时间始终保留。
    a.at_ms = b.at_ms;
    a == *b
}
fn latest(db: &Connection, message: &GroupMessage) -> Result<bool, String> {
    let id: Option<String> = db
        .query_row(
            "SELECT id FROM observations WHERE conversation=?1 AND json_extract(payload,'$.from_me')=0 ORDER BY seq DESC LIMIT 1",
            [&message.conversation],
            |r| r.get(0),
        )
        .optional()
        .map_err(|_| BROKEN)?;
    if id.as_deref() != Some(&message.id) {
        return Ok(false);
    }
    let anchors:Option<(String,String)>=db.query_row("SELECT a.platform_id,(SELECT platform_id FROM group_inputs WHERE conversation=a.conversation ORDER BY seq DESC LIMIT 1) FROM group_anchors a WHERE a.conversation=?1 AND a.id=?2",params![message.conversation,message.id],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|_|BROKEN)?;
    Ok(anchors.is_some_and(|(a, b)| a == b))
}
fn source(db: &Connection, conversation: &str, id: &str) -> Result<GroupMessage, String> {
    let raw: String = db
        .query_row(
            "SELECT payload FROM observations WHERE conversation=?1 AND id=?2",
            params![conversation, id],
            |r| r.get(0),
        )
        .map_err(|_| BROKEN)?;
    checked_message(&raw, conversation, id)
}

pub(super) fn migrate(db: &mut Connection) -> Result<(), String> {
    db.execute_batch(
        "BEGIN IMMEDIATE;
         CREATE TABLE IF NOT EXISTS group_anchors(conversation TEXT NOT NULL,id TEXT NOT NULL,platform_id TEXT NOT NULL,PRIMARY KEY(conversation,id),UNIQUE(conversation,platform_id));
         CREATE TABLE IF NOT EXISTS speech_reviews(conversation TEXT NOT NULL,id TEXT NOT NULL,outbound_id TEXT NOT NULL UNIQUE,state TEXT NOT NULL,revision INTEGER NOT NULL,due_ms INTEGER NOT NULL,decision TEXT,PRIMARY KEY(conversation,id));
         CREATE INDEX IF NOT EXISTS speech_due ON speech_reviews(state,due_ms);
         CREATE TABLE IF NOT EXISTS group_inputs(seq INTEGER PRIMARY KEY AUTOINCREMENT,conversation TEXT NOT NULL,platform_id TEXT NOT NULL,UNIQUE(conversation,platform_id));
         UPDATE metadata SET version=4;
         COMMIT;",
    )
    .map_err(|_| BROKEN)?;
    Ok(())
}

/// 在恢复写操作之前核对新格式全部状态，损坏不能被恢复动作掩盖。
pub(super) fn validate(db: &Connection) -> Result<(), String> {
    let mut inputs = db
        .prepare("SELECT conversation,platform_id FROM group_inputs")
        .map_err(|_| BROKEN)?;
    let rows = inputs
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|_| BROKEN)?;
    for row in rows {
        let (conversation, anchor) = row.map_err(|_| BROKEN)?;
        if !identifier(&conversation) || !identifier(&anchor) {
            return Err(BROKEN.into());
        }
    }
    let mut anchors = db
        .prepare("SELECT conversation,id,platform_id FROM group_anchors")
        .map_err(|_| BROKEN)?;
    let rows = anchors
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(|_| BROKEN)?;
    for row in rows {
        let (conversation, id, anchor) = row.map_err(|_| BROKEN)?;
        let message = source(db, &conversation, &id)?;
        let linked:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM group_inputs WHERE conversation=?1 AND platform_id=?2)",params![conversation,anchor],|r|r.get(0)).map_err(|_|BROKEN)?;
        if message.from_me || !identifier(&anchor) || !linked {
            return Err(BROKEN.into());
        }
    }
    let mut reviews = db
        .prepare(
            "SELECT conversation,id,outbound_id,state,revision,due_ms,decision FROM speech_reviews",
        )
        .map_err(|_| BROKEN)?;
    let rows = reviews
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, u64>(4)?,
                r.get::<_, u64>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })
        .map_err(|_| BROKEN)?;
    for row in rows {
        let (conversation, id, outbound_id, state, revision, _due, decision) =
            row.map_err(|_| BROKEN)?;
        let message = source(db, &conversation, &id)?;
        let anchor: String = db
            .query_row(
                "SELECT platform_id FROM group_anchors WHERE conversation=?1 AND id=?2",
                params![conversation, id],
                |r| r.get(0),
            )
            .map_err(|_| BROKEN)?;
        if message.from_me
            || !identifier(&outbound_id)
            || !outbound_id.starts_with("chime-")
            || !identifier(&anchor)
            || !matches!(
                state.as_str(),
                "pending"
                    | "evaluating"
                    | "waiting"
                    | "ready"
                    | "silence"
                    | "stale"
                    | "sending"
                    | "sent"
                    | "failed"
                    | "unknown"
            )
            || (state != "pending" && state != "stale" && revision == 0)
        {
            return Err(BROKEN.into());
        }
        let decision: Option<ChimeSpeechDecision> = decision
            .map(|raw| serde_json::from_str(&raw).map_err(|_| BROKEN.to_owned()))
            .transpose()?;
        if decision.as_ref().is_some_and(|d| !d.valid())
            || (state == "pending" && (decision.is_some() || revision != 0))
            || (state == "silence" && decision != Some(ChimeSpeechDecision::Silence {}))
            || (matches!(
                state.as_str(),
                "ready" | "sending" | "sent" | "failed" | "unknown"
            ) && !matches!(decision, Some(ChimeSpeechDecision::Speak { .. })))
            || (state == "waiting"
                && decision
                    .as_ref()
                    .is_some_and(|d| !matches!(d, ChimeSpeechDecision::Wait { .. })))
            || (state == "evaluating"
                && decision
                    .as_ref()
                    .is_some_and(|d| !matches!(d, ChimeSpeechDecision::Wait { .. })))
        {
            return Err(BROKEN.into());
        }
        let saved: Option<(String, String)> = db
            .query_row(
                "SELECT payload,status FROM outbound WHERE id=?1",
                [&outbound_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|_| BROKEN)?;
        if let Some((raw, status)) = saved {
            let outbound = checked_outbound(raw, &outbound_id, &conversation, &id)?;
            let Some(reply) = decision.as_ref().and_then(|d| d.reply(&message)) else {
                return Err(BROKEN.into());
            };
            let expected_status = match state.as_str() {
                "ready" => "queued",
                "stale" => "cancelled",
                v => v,
            };
            if outbound.speaker != message.speaker
                || outbound.reply != reply
                || status != expected_status
            {
                return Err(BROKEN.into());
            }
        } else if matches!(decision, Some(ChimeSpeechDecision::Speak { .. })) {
            return Err(BROKEN.into());
        }
    }
    Ok(())
}

pub(super) fn recover(db: &mut Connection) -> Result<(), String> {
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| BROKEN)?;
    // queued 在新契约里明确表示尚未认领；旧 queued 无此保证，保持旧 unknown 语义。
    tx.execute("UPDATE outbound SET status='unknown' WHERE status='sending' OR (status='queued' AND NOT EXISTS(SELECT 1 FROM speech_reviews s WHERE s.outbound_id=outbound.id))",[]).map_err(|_|BROKEN)?;
    tx.execute(
        "UPDATE speech_reviews SET state='unknown' WHERE state='sending'",
        [],
    )
    .map_err(|_| BROKEN)?;
    tx.execute("UPDATE speech_reviews SET state='waiting',decision=NULL,due_ms=?1 WHERE state='evaluating'",[number(now_ms().saturating_add(1000))?]).map_err(|_|BROKEN)?;
    tx.commit().map_err(|_| BROKEN)?;
    Ok(())
}

/// 后续任意群成员的新消息使旧决定过期；还没认领的正文没有未知发送结局。
pub(super) fn invalidate_older(db: &Connection, message: &GroupMessage) -> Result<(), String> {
    if message.from_me {
        return Ok(());
    }
    db.execute("UPDATE outbound SET status='cancelled' WHERE status='queued' AND id IN (SELECT outbound_id FROM speech_reviews WHERE conversation=?1 AND id<>?2 AND state='ready')",params![message.conversation,message.id]).map_err(|_|BROKEN)?;
    db.execute("UPDATE speech_reviews SET state='stale' WHERE conversation=?1 AND id<>?2 AND state IN ('pending','evaluating','waiting','ready')",params![message.conversation,message.id]).map_err(|_|BROKEN)?;
    Ok(())
}

fn authored(db: &Connection, id: &str) -> Result<Option<GroupAuthoredOutbound>, String> {
    let row:Option<(String,String,String,String,u64,u64)> = db.query_row(
        "SELECT o.conversation,o.reply_to,o.payload,a.platform_id,s.revision,s.due_ms FROM speech_reviews s JOIN outbound o ON o.id=s.outbound_id JOIN group_anchors a ON a.conversation=s.conversation AND a.id=s.id WHERE o.id=?1 AND ((s.state='ready' AND o.status='queued') OR (s.state='sending' AND o.status='sending'))",
        [id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))
        .optional().map_err(|_|BROKEN)?;
    row.map(
        |(conversation, source_id, raw, anchor, revision, due_at_ms)| {
            let outbound = checked_outbound(raw, id, &conversation, &source_id)?;
            let message = source(db, &conversation, &source_id)?;
            let decision: String = db
                .query_row(
                    "SELECT decision FROM speech_reviews WHERE outbound_id=?1",
                    [id],
                    |r| r.get(0),
                )
                .map_err(|_| BROKEN)?;
            let decision: ChimeSpeechDecision =
                serde_json::from_str(&decision).map_err(|_| BROKEN)?;
            if !identifier(&anchor)
                || revision == 0
                || outbound.speaker != message.speaker
                || message.from_me
                || !decision.valid()
                || decision.reply(&message).as_ref() != Some(&outbound.reply)
                || outbound.reply.parts
                    != vec![eve_chime_api::ChimePart {
                        text: outbound.reply.text.clone(),
                        pause_before_ms: 0,
                    }]
                || !(ChimeSpeechDecision::Speak {
                    text: outbound.reply.text.clone(),
                    after_ms: 0,
                })
                .valid()
            {
                return Err(BROKEN.into());
            }
            Ok(GroupAuthoredOutbound {
                outbound,
                anchor,
                revision,
                due_at_ms,
            })
        },
    )
    .transpose()
}

impl SqliteGroupObservations {
    pub(super) fn advance_anchor_inner(
        &self,
        conversation: &str,
        anchor: &str,
    ) -> Result<bool, String> {
        if !identifier(conversation) || !identifier(anchor) {
            return Err("群入站锚点无效".into());
        }
        self.run(|db|{
            let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|_|BROKEN)?;
            let changed=tx.execute("INSERT OR IGNORE INTO group_inputs(conversation,platform_id) VALUES(?1,?2)",params![conversation,anchor]).map_err(|_|BROKEN)?;
            if changed==1 {
                tx.execute("UPDATE outbound SET status='cancelled' WHERE status='queued' AND id IN (SELECT outbound_id FROM speech_reviews WHERE conversation=?1 AND state='ready')",[conversation]).map_err(|_|BROKEN)?;
                tx.execute("UPDATE speech_reviews SET state='stale' WHERE conversation=?1 AND state IN ('pending','evaluating','waiting','ready')",[conversation]).map_err(|_|BROKEN)?;
            }
            tx.commit().map_err(|_|BROKEN)?;
            Ok(changed==1)
        })
    }
    pub(super) fn append_anchored_inner(
        &self,
        message: &GroupMessage,
        anchor: &str,
    ) -> Result<bool, String> {
        self.append_fact(message, anchor, None)
    }
    pub(super) fn append_speech_inner(
        &self,
        message: &GroupMessage,
        anchor: &str,
        outbound_id: &str,
        at: u64,
    ) -> Result<bool, String> {
        if !identifier(outbound_id)
            || !outbound_id.starts_with("chime-")
            || message.text.chars().count() > eve_chime_api::MAX_MESSAGE_CHARS
        {
            return Err("群发言事实无效".into());
        }
        self.append_fact(message, anchor, Some((outbound_id, at)))
    }
    fn append_fact(
        &self,
        message: &GroupMessage,
        anchor: &str,
        stage: Option<(&str, u64)>,
    ) -> Result<bool, String> {
        if !message.valid() || message.from_me || message.text.len() > 32768 || !identifier(anchor)
        {
            return Err("群观察来源或平台锚点无效".into());
        }
        self.run(|db| {
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| BROKEN)?;
            let old: Option<String> = tx
                .query_row(
                    "SELECT payload FROM observations WHERE conversation=?1 AND id=?2",
                    params![message.conversation, message.id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|_| BROKEN)?;
            let appended = old.is_none();
            tx.execute(
                "INSERT OR IGNORE INTO group_inputs(conversation,platform_id) VALUES(?1,?2)",
                params![message.conversation, anchor],
            )
            .map_err(|_| BROKEN)?;
            if let Some(old) = old {
                let old = checked_message(&old, &message.conversation, &message.id)?;
                if !same_message(&old, message) {
                    return Err("群消息 ID 冲突；未覆盖原记录".into());
                }
            } else {
                tx.execute(
                    "INSERT INTO observations(conversation,id,payload) VALUES(?1,?2,?3)",
                    params![
                        message.conversation,
                        message.id,
                        serde_json::to_string(message).map_err(|_| BROKEN)?
                    ],
                )
                .map_err(|_| BROKEN)?;
                invalidate_older(&tx, message)?;
            }
            let old_anchor: Option<String> = tx
                .query_row(
                    "SELECT platform_id FROM group_anchors WHERE conversation=?1 AND id=?2",
                    params![message.conversation, message.id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|_| BROKEN)?;
            match old_anchor {
                Some(old) if old != anchor => return Err("群平台锚点冲突；未覆盖原记录".into()),
                Some(_) => {}
                None => {
                    tx.execute(
                        "INSERT INTO group_anchors(conversation,id,platform_id) VALUES(?1,?2,?3)",
                        params![message.conversation, message.id, anchor],
                    )
                    .map_err(|_| BROKEN)?;
                }
            }
            if let Some((outbound_id,at))=stage {
                let existing:Option<String>=tx.query_row("SELECT outbound_id FROM speech_reviews WHERE conversation=?1 AND id=?2",params![message.conversation,message.id],|r|r.get(0)).optional().map_err(|_|BROKEN)?;
                if let Some(existing)=existing {
                    if existing!=outbound_id {return Err("群发言事实 ID 冲突".into());}
                } else {
                    // 旧发送账本只能保留原结局，不能为同一来源重新生成发言。
                    let delivered:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM outbound WHERE (conversation=?1 AND reply_to=?2) OR id=?3)",params![message.conversation,message.id,outbound_id],|r|r.get(0)).map_err(|_|BROKEN)?;
                    if !delivered {
                        let state=if latest(&tx,message)?{"pending"}else{"stale"};
                        tx.execute("INSERT INTO speech_reviews(conversation,id,outbound_id,state,revision,due_ms) VALUES(?1,?2,?3,?4,0,?5)",params![message.conversation,message.id,outbound_id,state,number(at)?]).map_err(|_|BROKEN)?;
                    }
                }
            }
            tx.commit().map_err(|_| BROKEN)?;
            Ok(appended)
        })
    }
    pub(super) fn stage_speech_inner(
        &self,
        message: &GroupMessage,
        outbound_id: &str,
        at: u64,
    ) -> Result<(), String> {
        if !message.valid()
            || message.from_me
            || message.text.chars().count() > eve_chime_api::MAX_MESSAGE_CHARS
            || !identifier(outbound_id)
            || !outbound_id.starts_with("chime-")
        {
            return Err("群发言事实无效".into());
        }
        self.run(|db|{
            let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|_|BROKEN)?;
            let original=source(&tx,&message.conversation,&message.id)?;
            if !same_message(&original,message){return Err("群发言事实与原观察不符".into());}
            let anchor:Option<String>=tx.query_row("SELECT platform_id FROM group_anchors WHERE conversation=?1 AND id=?2",params![message.conversation,message.id],|r|r.get(0)).optional().map_err(|_|BROKEN)?;
            if !anchor.as_deref().is_some_and(identifier){return Err("群发言缺少可信平台锚点".into());}
            let old:Option<String>=tx.query_row("SELECT outbound_id FROM speech_reviews WHERE conversation=?1 AND id=?2",params![message.conversation,message.id],|r|r.get(0)).optional().map_err(|_|BROKEN)?;
            if let Some(old)=old {
                return if old==outbound_id {Ok(())} else {Err("群发言事实 ID 冲突".into())};
            }
            if tx.query_row("SELECT EXISTS(SELECT 1 FROM outbound WHERE (conversation=?1 AND reply_to=?2) OR id=?3)",params![message.conversation,message.id,outbound_id],|r|r.get::<_,bool>(0)).map_err(|_|BROKEN)? {return Err("这条群消息已有投递记录；不重发".into());}
            let state=if latest(&tx,message)? {"pending"} else {"stale"};
            tx.execute("INSERT INTO speech_reviews(conversation,id,outbound_id,state,revision,due_ms) VALUES(?1,?2,?3,?4,0,?5)",params![message.conversation,message.id,outbound_id,state,number(at)?]).map_err(|_|BROKEN)?;
            tx.commit().map_err(|_|BROKEN)?;
            Ok(())
        })
    }
    pub(super) fn pending_speech_inner(
        &self,
        at: u64,
        limit: usize,
    ) -> Result<Vec<GroupMessage>, String> {
        self.run(|db|{
            let sql=format!("SELECT m.conversation,m.id,m.payload FROM speech_reviews s JOIN observations m ON m.conversation=s.conversation AND m.id=s.id WHERE s.state IN ('pending','waiting') AND s.due_ms<=?1 AND {LATEST} ORDER BY s.due_ms,m.seq LIMIT ?2");
            let mut query=db.prepare(&sql).map_err(|_|BROKEN)?;
            query.query_map(params![number(at)?,limit.min(64)],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))
                .map_err(|_|BROKEN)?.map(|r|{let(c,id,raw)=r.map_err(|_|BROKEN)?;checked_message(&raw,&c,&id)}).collect()
        })
    }
    pub(super) fn begin_speech_inner(
        &self,
        message: &GroupMessage,
        at: u64,
    ) -> Result<Option<GroupSpeechReview>, String> {
        self.run(|db|{
            let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|_|BROKEN)?;
            let original=source(&tx,&message.conversation,&message.id)?;
            if !same_message(&original,message)||!latest(&tx,message)? {return Ok(None);}
            let revision:Option<u64>=tx.query_row("SELECT revision FROM speech_reviews WHERE conversation=?1 AND id=?2 AND state IN ('pending','waiting') AND due_ms<=?3",params![message.conversation,message.id,number(at)?],|r|r.get(0)).optional().map_err(|_|BROKEN)?;
            let Some(revision)=revision else {return Ok(None);};
            let revision=revision.checked_add(1).ok_or(BROKEN)?;
            tx.execute("UPDATE speech_reviews SET state='evaluating',revision=?3 WHERE conversation=?1 AND id=?2",params![message.conversation,message.id,number(revision)?]).map_err(|_|BROKEN)?;
            tx.commit().map_err(|_|BROKEN)?;
            Ok(Some(GroupSpeechReview{message:original,revision}))
        })
    }
    pub(super) fn decide_speech_inner(
        &self,
        review: &GroupSpeechReview,
        decision: Option<&ChimeSpeechDecision>,
        at: u64,
    ) -> Result<bool, String> {
        if review.revision == 0 || decision.is_some_and(|d| !d.valid()) {
            return Err("群发言决定格式无效".into());
        }
        self.run(|db|{
            let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|_|BROKEN)?;
            let m=&review.message;
            let row:Option<String>=tx.query_row("SELECT outbound_id FROM speech_reviews WHERE conversation=?1 AND id=?2 AND state='evaluating' AND revision=?3",params![m.conversation,m.id,number(review.revision)?],|r|r.get(0)).optional().map_err(|_|BROKEN)?;
            let Some(outbound_id)=row else {return Ok(false);};
            let original=source(&tx,&m.conversation,&m.id)?;
            if original!=*m||!latest(&tx,m)?{return Ok(false);}
            let (state,delay)=match decision {
                Some(ChimeSpeechDecision::Speak{after_ms,..})=>("ready",*after_ms),
                Some(ChimeSpeechDecision::Wait{after_ms})=>("waiting",*after_ms),
                Some(ChimeSpeechDecision::Silence {})=>("silence",0),
                None=>("waiting",RETRY_MS),
            };
            let due=at.checked_add(delay).ok_or("群发言时间超限")?;
            if let Some(reply)=decision.and_then(|d|d.reply(m)) {
                let outbound=GroupOutbound{id:outbound_id.clone(),reply,speaker:m.speaker.clone()};
                tx.execute("INSERT INTO outbound(id,conversation,reply_to,payload,status) VALUES(?1,?2,?3,?4,'queued')",params![outbound_id,m.conversation,m.id,serde_json::to_string(&outbound).map_err(|_|BROKEN)?]).map_err(|_|BROKEN)?;
            }
            let raw=decision.map(serde_json::to_string).transpose().map_err(|_|BROKEN)?;
            tx.execute("UPDATE speech_reviews SET state=?4,due_ms=?5,decision=?6 WHERE conversation=?1 AND id=?2 AND revision=?3",params![m.conversation,m.id,number(review.revision)?,state,number(due)?,raw]).map_err(|_|BROKEN)?;
            tx.commit().map_err(|_|BROKEN)?;
            Ok(true)
        })
    }
    pub(super) fn next_authored_inner(
        &self,
        at: u64,
    ) -> Result<Option<GroupAuthoredOutbound>, String> {
        self.run(|db|{
            let sql=format!("SELECT s.outbound_id FROM speech_reviews s JOIN observations m ON m.conversation=s.conversation AND m.id=s.id JOIN outbound o ON o.id=s.outbound_id WHERE s.state='ready' AND o.status='queued' AND s.due_ms<=?1 AND {LATEST} ORDER BY s.due_ms,m.seq LIMIT 1");
            let id:Option<String>=db.query_row(&sql,[number(at)?],|r|r.get(0)).optional().map_err(|_|BROKEN)?;
            id.map(|id|authored(db,&id)?.ok_or_else(||BROKEN.into())).transpose()
        })
    }
    pub(super) fn authored_inner(&self, id: &str) -> Result<Option<GroupAuthoredOutbound>, String> {
        self.run(|db| authored(db, id))
    }
    pub(super) fn authored_after_inner(
        &self,
        at: u64,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<GroupAuthoredOutbound>, String> {
        if !(1..=65).contains(&limit) {
            return Err("群发言分页上限无效".into());
        }
        self.run(|db| {
            let sql = format!("SELECT s.outbound_id FROM speech_reviews s JOIN observations m ON m.conversation=s.conversation AND m.id=s.id JOIN outbound o ON o.id=s.outbound_id WHERE s.state='ready' AND o.status='queued' AND s.due_ms<=?1 AND (?2 IS NULL OR s.outbound_id>?2) AND {LATEST} ORDER BY s.outbound_id LIMIT ?3");
            let mut query = db.prepare(&sql).map_err(|_| BROKEN)?;
            let ids = query.query_map(params![number(at)?, after, limit], |r| r.get::<_, String>(0)).map_err(|_| BROKEN)?;
            ids.map(|id| authored(db, &id.map_err(|_| BROKEN)?)?.ok_or_else(|| BROKEN.into())).collect()
        })
    }
    pub(super) fn claim_authored_inner(
        &self,
        expected: &GroupAuthoredOutbound,
        at: u64,
    ) -> Result<bool, String> {
        self.run(|db|{
            let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|_|BROKEN)?;
            let Some(current)=authored(&tx,&expected.outbound.id)? else {return Ok(false);};
            if current!=*expected || current.due_at_ms>at {return Ok(false);}
            let m=source(&tx,&current.outbound.reply.conversation,&current.outbound.reply.reply_to)?;
            if !latest(&tx,&m)? {return Ok(false);}
            let changed=tx.execute("UPDATE speech_reviews SET state='sending' WHERE outbound_id=?1 AND state='ready' AND revision=?2",params![current.outbound.id,number(current.revision)?]).map_err(|_|BROKEN)?;
            if changed!=1 {return Ok(false);}
            if tx.execute("UPDATE outbound SET status='sending' WHERE id=?1 AND status='queued'",[&current.outbound.id]).map_err(|_|BROKEN)?!=1 {return Err(BROKEN.into());}
            tx.commit().map_err(|_|BROKEN)?;
            Ok(true)
        })
    }
}
