//! 默认规则：正在做与能开始的事优先，其次是与用户的对话，最后才判断停止。
use eve_pursuit_api::*;
use eve_pursuit_plugin::RulePursuitPolicy;

fn capability(runs_used: usize, max_runs: usize, current: Option<Outcome>) -> Capability {
    Capability {
        runs_used,
        max_runs,
        ledger_full: false,
        current,
        succeeded_ever: current == Some(Outcome::Succeeded),
        admitted: true,
    }
}
fn research(runs_used: usize, current: Option<Outcome>) -> Option<Capability> {
    Some(capability(runs_used, 3, current))
}
fn practice(runs_used: usize, current: Option<Outcome>) -> Option<Capability> {
    Some(capability(runs_used, 2, current))
}
/// 两项能力都开启、当前版本都已成功、对话都已结束。
fn settled() -> PursuitFacts {
    PursuitFacts {
        research: research(1, Some(Outcome::Succeeded)),
        practice: practice(1, Some(Outcome::Succeeded)),
        clarify: Exchange::NotStarted,
        outreach: Exchange::Closed,
        open_follow_ups: 0,
        room_for_input: true,
    }
}
fn evaluate(facts: &PursuitFacts) -> Verdict {
    RulePursuitPolicy.evaluate(facts)
}
fn wait(reason: WaitReason) -> Verdict {
    Verdict::Wait { reason }
}
fn go(step: Step, started: bool) -> Verdict {
    Verdict::Continue { step, started }
}
const SPENT: Verdict = Verdict::Stop {
    reason: StopReason::LearningBudgetSpent,
};

#[test]
fn work_in_progress_or_ready_to_start_continues() {
    assert_eq!(RulePursuitPolicy.version(), POLICY_VERSION);
    let mut facts = settled();
    facts.research = research(1, Some(Outcome::Running));
    facts.practice = practice(0, None);
    assert_eq!(evaluate(&facts), go(Step::Research, true));

    facts.research = research(1, Some(Outcome::Succeeded));
    facts.practice = practice(1, Some(Outcome::Running));
    assert_eq!(evaluate(&facts), go(Step::Practice, true));

    // 新版本：研究先开始，实践等研究有结局。
    facts.research = research(1, None);
    facts.practice = practice(1, None);
    assert_eq!(evaluate(&facts), go(Step::Research, false));
    facts.research = research(2, Some(Outcome::Failed));
    assert_eq!(evaluate(&facts), go(Step::Practice, false));

    // 研究次数已经用完时，新版本直接实践。
    facts.research = research(3, None);
    assert_eq!(evaluate(&facts), go(Step::Practice, false));
}

#[test]
fn autonomy_budget_and_full_ledgers_wait_instead_of_stopping() {
    let mut facts = settled();
    facts.research = research(1, None);
    facts.research.as_mut().unwrap().admitted = false;
    assert_eq!(evaluate(&facts), wait(WaitReason::AutonomyBudget));

    // 研究账本满：研究不能开始，实践也等不到研究的结局。
    let full = facts.research.as_mut().unwrap();
    full.admitted = true;
    full.ledger_full = true;
    facts.practice = practice(1, None);
    assert_eq!(evaluate(&facts), wait(WaitReason::Capacity));

    let mut facts = settled();
    facts.research = research(3, Some(Outcome::Succeeded));
    facts.practice = practice(1, None);
    facts.practice.as_mut().unwrap().ledger_full = true;
    assert_eq!(evaluate(&facts), wait(WaitReason::Capacity));
    facts.practice.as_mut().unwrap().ledger_full = false;
    facts.practice.as_mut().unwrap().admitted = false;
    assert_eq!(evaluate(&facts), wait(WaitReason::AutonomyBudget));
}

#[test]
fn a_stalled_version_asks_once_then_waits_for_the_users_reply() {
    let mut facts = settled();
    facts.practice = practice(1, Some(Outcome::Unsuccessful));
    assert_eq!(evaluate(&facts), Verdict::Clarify);
    facts.practice = practice(1, Some(Outcome::NotApplicable));
    assert_eq!(evaluate(&facts), Verdict::Clarify);
    facts.clarify = Exchange::Preparing;
    assert_eq!(evaluate(&facts), Verdict::Clarify);
    facts.clarify = Exchange::AwaitingReply;
    assert_eq!(evaluate(&facts), wait(WaitReason::UserReply));
    // 问过之后没有新的版本，就等用户提到新的想法。
    facts.clarify = Exchange::Closed;
    assert_eq!(evaluate(&facts), wait(WaitReason::NewInput));
    // 未开启请求澄清时不问。
    facts.clarify = Exchange::Disabled;
    assert_eq!(evaluate(&facts), wait(WaitReason::NewInput));

    // 只开启研究时，看研究有没有找到可引用的资料。
    let mut facts = settled();
    facts.practice = None;
    facts.research = research(1, Some(Outcome::Unsuccessful));
    assert_eq!(evaluate(&facts), Verdict::Clarify);
    // 模型失败或中断不算卡住，不问。
    facts.research = research(1, Some(Outcome::Failed));
    assert_eq!(evaluate(&facts), wait(WaitReason::NewInput));
}

#[test]
fn a_verified_practice_leads_to_one_invitation_and_its_reply() {
    let mut facts = settled();
    facts.outreach = Exchange::NotStarted;
    assert_eq!(evaluate(&facts), go(Step::Invite, false));
    facts.outreach = Exchange::Preparing;
    assert_eq!(evaluate(&facts), go(Step::Invite, true));
    facts.outreach = Exchange::AwaitingReply;
    assert_eq!(evaluate(&facts), wait(WaitReason::UserReply));
    facts.outreach = Exchange::Closed;
    facts.open_follow_ups = 1;
    assert_eq!(evaluate(&facts), wait(WaitReason::FollowUp));
    facts.open_follow_ups = 0;
    assert_eq!(evaluate(&facts), wait(WaitReason::NewInput));
    // 邀请不分版本：之前的版本验证过、现在还没邀请，也会邀请。
    facts.practice = practice(2, Some(Outcome::Unsuccessful));
    facts.practice.as_mut().unwrap().succeeded_ever = true;
    facts.clarify = Exchange::Closed;
    facts.outreach = Exchange::NotStarted;
    assert_eq!(evaluate(&facts), go(Step::Invite, false));
}

#[test]
fn stops_only_when_nothing_is_left_to_do() {
    let mut facts = settled();
    facts.research = research(3, Some(Outcome::Succeeded));
    facts.practice = practice(2, Some(Outcome::Unsuccessful));
    facts.clarify = Exchange::Closed;
    assert_eq!(evaluate(&facts), SPENT);
    // 还在问用户、等回复或有后续创作时不停止。
    facts.clarify = Exchange::AwaitingReply;
    assert_eq!(evaluate(&facts), wait(WaitReason::UserReply));
    facts.clarify = Exchange::Closed;
    facts.open_follow_ups = 2;
    assert_eq!(evaluate(&facts), wait(WaitReason::FollowUp));
    facts.open_follow_ups = 0;

    // 实践次数用完、新版本没有实践可做：研究做完后停止，不再问。
    facts.research = research(3, Some(Outcome::Running));
    facts.practice = practice(2, None);
    facts.clarify = Exchange::NotStarted;
    assert_eq!(evaluate(&facts), go(Step::Research, true));
    facts.research = research(3, Some(Outcome::Unsuccessful));
    assert_eq!(evaluate(&facts), SPENT);

    // 只开启研究时，研究次数用完就停止。
    facts.practice = None;
    facts.research = research(3, Some(Outcome::Succeeded));
    assert_eq!(evaluate(&facts), SPENT);

    // 还有次数但兴趣记不下新的陈述：再也不会有新版本。
    let mut facts = settled();
    facts.room_for_input = false;
    assert_eq!(
        evaluate(&facts),
        Verdict::Stop {
            reason: StopReason::InputClosed
        }
    );
    facts.room_for_input = true;
    assert_eq!(evaluate(&facts), wait(WaitReason::NewInput));

    let none = PursuitFacts {
        research: None,
        practice: None,
        ..settled()
    };
    assert_eq!(evaluate(&none), wait(WaitReason::NoCapability));
}

#[test]
fn verdicts_serialize_as_tagged_snake_case() {
    let text = |verdict: Verdict| serde_json_text(verdict);
    assert_eq!(
        text(go(Step::Practice, false)),
        r#"{"verdict":"continue","step":"practice","started":false}"#
    );
    assert_eq!(text(Verdict::Clarify), r#"{"verdict":"clarify"}"#);
    assert_eq!(
        text(wait(WaitReason::NewInput)),
        r#"{"verdict":"wait","reason":"new_input"}"#
    );
    assert_eq!(
        text(SPENT),
        r#"{"verdict":"stop","reason":"learning_budget_spent"}"#
    );
}

fn serde_json_text(verdict: Verdict) -> String {
    serde_json::to_string(&verdict).unwrap()
}
