//! 自然语言管理工具的宿主边界：使用真实插件与文件账本验证隔离、拒绝和重启恢复。
//! 候选来自固定测试产物；这些测试不把模拟运行证据当作真实模型或外部运行器验收。
use crate::{qq_outreach, qq_skill, qq_tools};
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::{ContextScope, Tool, ToolCall, ToolExecutionContext, ToolExecutionError};
use eve_outreach_api::{
    Fact, FactKind, Invitation, Milestone, OUTREACH_PLUGIN_ID, OUTREACH_STATE_KEY, OutreachAdmin,
    OutreachPolicy,
};
use eve_outreach_plugin::{OutreachController, OutreachPlugin};
use eve_plugin_api::{PluginId, StateStore};
use eve_practice_api::{ArtifactFile, Probe, ProbeResult, RunEvidence, RunExit, RunnerProfile};
use eve_skill_api::{
    DistillStatus, Distillation, ParameterKind, Proposal, SKILL_PLUGIN_ID, SKILL_STATE_KEY,
    SkillAdmin, SkillOrigin, SkillParameter, SkillProposal, SkillTemplate,
};
use eve_skill_plugin::{SkillController, SkillPlugin};
use eve_toolforge_api::{
    CheckRule, CheckSpec, ExampleFiles, ForgeAttempt, ForgeOutput, ForgeStatus, GapRef,
    TOOLFORGE_PLUGIN_ID, TOOLFORGE_STATE_KEY, ToolAdmin,
};
use eve_toolforge_plugin::{ToolForgeController, ToolForgePlugin};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

struct Host {
    kernel: Kernel,
    store: Arc<FileStateStore>,
    skills: Arc<SkillController>,
    tools: Arc<ToolForgeController>,
    outreach: Arc<OutreachController>,
}

impl Host {
    async fn open(directory: &Path) -> Self {
        let store = Arc::new(FileStateStore::open(directory).unwrap());
        let kernel = Kernel::with_services(KernelServices {
            state: store.clone(),
            ..KernelServices::default()
        });
        let skills = SkillPlugin::new().unwrap();
        let tools = ToolForgePlugin::new().unwrap();
        let outreach = OutreachPlugin::new().unwrap();
        let skill_admin = Arc::new(skills.controller());
        let tool_admin = Arc::new(tools.controller());
        let outreach_admin = Arc::new(outreach.controller());
        kernel.register(Box::new(skills)).unwrap();
        kernel.register(Box::new(tools)).unwrap();
        kernel.register(Box::new(outreach)).unwrap();
        kernel.start_all().await.unwrap();
        Self {
            kernel,
            store,
            skills: skill_admin,
            tools: tool_admin,
            outreach: outreach_admin,
        }
    }

    fn raw(&self, plugin: &str, key: &str) -> Option<Vec<u8>> {
        self.store
            .get(&PluginId::new(plugin).unwrap(), key)
            .unwrap()
    }

    async fn close(self) {
        self.kernel.stop_all().await.unwrap();
    }
}

fn scoped(owner: &str) -> ToolExecutionContext {
    ToolExecutionContext::new().with_scope(Some(ContextScope {
        // 同一群聊中的不同成员仍必须按可信 user_id 隔离。
        session_id: "shared-group".into(),
        user_id: owner.into(),
    }))
}

async fn execute(
    tool: &dyn Tool,
    arguments: Value,
    context: ToolExecutionContext,
) -> Result<Value, ToolExecutionError> {
    tool.execute(
        ToolCall {
            id: "natural-management-call".into(),
            name: tool.definition().name,
            arguments,
        },
        context,
    )
    .await
}

fn profile() -> RunnerProfile {
    RunnerProfile {
        runner_id: "natural-management-fixture:v1".into(),
        runtime: "固定数据运行器".into(),
        domain: "测试产物".into(),
        layout: "items/item.hjson".into(),
        properties: vec!["value".into()],
    }
}

/// 走真实提炼账本的 Proposing -> Verifying -> Verified/Unverified 状态转换。
fn skill_candidate(
    host: &Host,
    owner: &str,
    sequence: u64,
    extends: Option<&str>,
    verified: bool,
) -> Distillation {
    let template = SkillTemplate {
        title: format!("{owner} 的数值产物"),
        summary: "生成并验证指定数值的测试文件".into(),
        parameters: vec![SkillParameter {
            name: "value".into(),
            description: "数值".into(),
            kind: ParameterKind::Integer { min: 1, max: 20 },
        }],
        files: vec![ArtifactFile {
            path: "items/item.hjson".into(),
            content: "value: {{value}}\n".into(),
        }],
        probes: vec![Probe {
            subject: "item".into(),
            property: "value".into(),
            expected: "{{value}}".into(),
        }],
    };
    let proposal = SkillProposal {
        extends: extends.map(str::to_string),
        name: "valued-item".into(),
        template,
        arguments: [("value".into(), "5".into())].into_iter().collect(),
    };
    let source = eve_skill_api::instantiate(&proposal.template, &proposal.arguments).unwrap();
    let (issues, holdout) = eve_skill_api::proposal_issues(&proposal, &source, &profile());
    assert!(issues.is_empty());
    let at = sequence * 10;
    let begun = host
        .skills
        .begin_distillation(
            owner,
            SkillOrigin {
                practice_run_id: format!("fixture-{owner}-{sequence}"),
                attempt: 1,
                goal_id: format!("goal-{owner}-{sequence}"),
            },
            source,
            &profile(),
            "fixture-distiller:v1",
            at,
        )
        .unwrap()
        .unwrap();
    let pending = host
        .skills
        .record_proposal(
            &begun.id,
            at + 1,
            Ok(Proposal::Skill(proposal)),
            issues,
            holdout,
        )
        .unwrap();
    let draft = pending.holdout_draft().unwrap();
    let actual = draft.files[0]
        .content
        .trim()
        .strip_prefix("value: ")
        .unwrap()
        .to_string();
    let evidence = RunEvidence {
        runtime_version: "fixture:v1".into(),
        exit: RunExit::Completed,
        loaded: true,
        warnings: if verified {
            vec![]
        } else {
            vec!["验证警告".into()]
        },
        probes: draft
            .probes
            .iter()
            .map(|probe| ProbeResult {
                probe: probe.clone(),
                actual: Some(actual.clone()),
                passed: actual == probe.expected,
            })
            .collect(),
        log_excerpt: "fixture loaded".into(),
        log_sha256: "a".repeat(64),
        log_bytes: 14,
        duration_ms: 1,
    };
    host.skills
        .record_verification(&begun.id, at + 2, evidence)
        .unwrap()
}

/// 用固定的有问题/已修复文件实际执行只读规则回放，再写入真实锻造账本。
fn forged_candidate(host: &Host, owner: &str, sequence: usize, verified: bool) -> ForgeAttempt {
    let gap = GapRef {
        runner_id: profile().runner_id,
        key: "legacy-field".into(),
        summary: "旧字段会使产物加载失败".into(),
    };
    let spec = CheckSpec {
        name: "no-legacy".into(),
        summary: format!("{owner} 的旧字段检查"),
        message: "请去掉旧字段".into(),
        rules: vec![CheckRule::ForbidText {
            pattern: "*.hjson".into(),
            text: if verified { "legacy:" } else { "missing:" }.into(),
        }],
    };
    let example = |suffix: &str, content: &str| ExampleFiles {
        run_id: format!("fixture-{owner}-{sequence}-{suffix}"),
        attempt: 1,
        files: vec![ArtifactFile {
            path: "items/item.hjson".into(),
            content: content.into(),
        }],
    };
    let verification = eve_toolforge_api::verify(
        &spec,
        &[example("failing", "legacy: true\n")],
        &[example("passing", "value: 5\n")],
    );
    assert_eq!(verification.passed(), verified);
    let at = sequence as u64 * 10;
    let begun = host
        .tools
        .begin_forge(owner, gap, sequence, "fixture-forger:v1", at)
        .unwrap()
        .unwrap();
    host.tools
        .record_forge(
            &begun.id,
            at + 1,
            Ok(ForgeOutput::Check(spec)),
            Some(verification),
        )
        .unwrap()
}

fn pending_invitation(host: &Host, owner: &str, at: u64) -> Invitation {
    let invitation = host
        .outreach
        .begin(
            owner,
            &format!("goal-{owner}"),
            Milestone {
                practice_run_id: format!("practice-{owner}"),
                skill_id: None,
            },
            vec![Fact {
                kind: FactKind::Progress,
                text: format!("{owner} 的进展"),
            }],
            "fixture-composer:v1",
            at,
        )
        .unwrap()
        .unwrap();
    host.outreach
        .record_composition(&invitation.id, at + 1, Ok(format!("{owner} 的邀请")))
        .unwrap()
}

#[tokio::test]
async fn natural_skill_management_is_owned_verified_and_persistent() {
    let directory = tempfile::tempdir().unwrap();
    let host = Host::open(directory.path()).await;
    let first = skill_candidate(&host, "owner-a", 1, None, true);
    let id = first.skill.unwrap().skill_id;
    let second = skill_candidate(&host, "owner-a", 2, Some(&id), true);
    assert_eq!(second.skill.unwrap().version, 2);
    let rejected = skill_candidate(&host, "owner-a", 3, Some(&id), false);
    assert_eq!(rejected.status, DistillStatus::Unverified);
    assert!(rejected.skill.is_none());
    let other = skill_candidate(&host, "owner-b", 4, None, true)
        .skill
        .unwrap()
        .skill_id;
    let tool = qq_skill::ManageTool {
        skills: host.skills.clone(),
    };
    assert_eq!(tool.definition().name, "manage_skill");

    let listed = execute(&tool, json!({"action":"list"}), scoped("owner-a"))
        .await
        .unwrap();
    assert_eq!(listed["skills"].as_array().unwrap().len(), 1);
    assert_eq!(listed["skills"][0]["id"], id);
    assert!(!listed.to_string().contains(&other));
    let shown = execute(
        &tool,
        json!({"action":"show","skill_id":id}),
        scoped("owner-a"),
    )
    .await
    .unwrap();
    assert_eq!(shown["skill"]["enabled"], 2);
    assert_eq!(shown["skill"]["versions"].as_array().unwrap().len(), 2);

    let before = host.raw(SKILL_PLUGIN_ID, SKILL_STATE_KEY);
    for action in ["show", "disable", "enable", "rollback"] {
        let mut args = json!({"action":action,"skill_id":other});
        if action == "enable" {
            args["version"] = json!(1);
        }
        assert!(execute(&tool, args, scoped("owner-a")).await.is_err());
    }
    for args in [
        json!({"action":"enable","skill_id":id,"version":3}),
        json!({"action":"show","skill_id":"missing-skill"}),
        json!({"action":"enable","skill_id":id,"version":999}),
    ] {
        assert!(execute(&tool, args, scoped("owner-a")).await.is_err());
    }
    assert_eq!(host.raw(SKILL_PLUGIN_ID, SKILL_STATE_KEY), before);

    let disabled = execute(
        &tool,
        json!({"action":"disable","skill_id":id}),
        scoped("owner-a"),
    )
    .await
    .unwrap();
    assert_eq!(disabled["changed"], true);
    assert!(disabled["skill"]["enabled"].is_null());
    assert!(
        host.skills
            .snapshot()
            .unwrap()
            .skill(&id)
            .unwrap()
            .held_by_owner()
    );
    let enabled = execute(
        &tool,
        json!({"action":"enable","skill_id":id,"version":2}),
        scoped("owner-a"),
    )
    .await
    .unwrap();
    assert_eq!(enabled["skill"]["enabled"], 2);
    let rolled_back = execute(
        &tool,
        json!({"action":"rollback","skill_id":id}),
        scoped("owner-a"),
    )
    .await
    .unwrap();
    assert_eq!(rolled_back["skill"]["enabled"], 1);
    let before = host.raw(SKILL_PLUGIN_ID, SKILL_STATE_KEY);
    assert!(
        execute(
            &tool,
            json!({"action":"rollback","skill_id":id}),
            scoped("owner-a")
        )
        .await
        .is_err()
    );
    assert_eq!(host.raw(SKILL_PLUGIN_ID, SKILL_STATE_KEY), before);
    let saved = host.skills.snapshot().unwrap().skill(&id).unwrap().clone();
    assert_eq!(saved.enabled, Some(1));
    assert_eq!(saved.versions.len(), 2);
    assert_eq!(
        saved.changes.last().unwrap().actor,
        eve_skill_api::Actor::Owner
    );
    assert_eq!(
        host.skills
            .snapshot()
            .unwrap()
            .skill(&other)
            .unwrap()
            .enabled,
        Some(1)
    );
    drop(tool);
    host.close().await;

    let reopened = Host::open(directory.path()).await;
    assert!(reopened.skills.snapshot().unwrap().skill(&id) == Some(&saved));
    let restored = qq_skill::ManageTool {
        skills: reopened.skills.clone(),
    };
    assert_eq!(
        execute(
            &restored,
            json!({"action":"show","skill_id":id}),
            scoped("owner-a")
        )
        .await
        .unwrap()["skill"]["enabled"],
        1
    );
    reopened.close().await;
}

#[tokio::test]
async fn natural_forged_tool_management_is_owned_verified_and_persistent() {
    let directory = tempfile::tempdir().unwrap();
    let host = Host::open(directory.path()).await;
    let first = forged_candidate(&host, "owner-a", 1, true);
    let id = first.tool.unwrap().tool_id;
    assert_eq!(
        forged_candidate(&host, "owner-a", 2, true)
            .tool
            .unwrap()
            .version,
        2
    );
    let rejected = forged_candidate(&host, "owner-a", 3, false);
    assert_eq!(rejected.status, ForgeStatus::Rejected);
    assert!(rejected.tool.is_none());
    let other = forged_candidate(&host, "owner-b", 1, true)
        .tool
        .unwrap()
        .tool_id;
    let tool = qq_tools::ManageTool {
        tools: host.tools.clone(),
    };
    assert_eq!(tool.definition().name, "manage_forged_tool");

    let listed = execute(&tool, json!({"action":"list"}), scoped("owner-a"))
        .await
        .unwrap();
    assert_eq!(listed["tools"].as_array().unwrap().len(), 1);
    assert_eq!(listed["tools"][0]["id"], id);
    assert!(!listed.to_string().contains(&other));
    let shown = execute(
        &tool,
        json!({"action":"show","tool_id":id}),
        scoped("owner-a"),
    )
    .await
    .unwrap();
    assert_eq!(shown["tool"]["enabled"], 2);
    assert_eq!(shown["tool"]["versions"].as_array().unwrap().len(), 2);

    let before = host.raw(TOOLFORGE_PLUGIN_ID, TOOLFORGE_STATE_KEY);
    for action in ["show", "disable", "enable", "rollback"] {
        let mut args = json!({"action":action,"tool_id":other});
        if action == "enable" {
            args["version"] = json!(1);
        }
        assert!(execute(&tool, args, scoped("owner-a")).await.is_err());
    }
    for args in [
        json!({"action":"enable","tool_id":id,"version":3}),
        json!({"action":"show","tool_id":"missing-tool"}),
        json!({"action":"enable","tool_id":id,"version":999}),
    ] {
        assert!(execute(&tool, args, scoped("owner-a")).await.is_err());
    }
    assert_eq!(host.raw(TOOLFORGE_PLUGIN_ID, TOOLFORGE_STATE_KEY), before);

    let disabled = execute(
        &tool,
        json!({"action":"disable","tool_id":id}),
        scoped("owner-a"),
    )
    .await
    .unwrap();
    assert_eq!(disabled["changed"], true);
    assert!(disabled["tool"]["enabled"].is_null());
    assert!(
        host.tools
            .snapshot()
            .unwrap()
            .tool(&id)
            .unwrap()
            .disabled_by_owner()
    );
    let enabled = execute(
        &tool,
        json!({"action":"enable","tool_id":id,"version":2}),
        scoped("owner-a"),
    )
    .await
    .unwrap();
    assert_eq!(enabled["tool"]["enabled"], 2);
    let rolled_back = execute(
        &tool,
        json!({"action":"rollback","tool_id":id}),
        scoped("owner-a"),
    )
    .await
    .unwrap();
    assert_eq!(rolled_back["tool"]["enabled"], 1);
    let before = host.raw(TOOLFORGE_PLUGIN_ID, TOOLFORGE_STATE_KEY);
    assert!(
        execute(
            &tool,
            json!({"action":"rollback","tool_id":id}),
            scoped("owner-a")
        )
        .await
        .is_err()
    );
    assert_eq!(host.raw(TOOLFORGE_PLUGIN_ID, TOOLFORGE_STATE_KEY), before);
    let saved = host.tools.snapshot().unwrap().tool(&id).unwrap().clone();
    assert_eq!(saved.enabled, Some(1));
    assert_eq!(saved.versions.len(), 2);
    assert_eq!(
        saved.changes.last().unwrap().actor,
        eve_toolforge_api::Actor::Owner
    );
    assert_eq!(
        host.tools.snapshot().unwrap().tool(&other).unwrap().enabled,
        Some(1)
    );
    drop(tool);
    host.close().await;

    let reopened = Host::open(directory.path()).await;
    assert_eq!(reopened.tools.snapshot().unwrap().tool(&id), Some(&saved));
    let restored = qq_tools::ManageTool {
        tools: reopened.tools.clone(),
    };
    assert_eq!(
        execute(
            &restored,
            json!({"action":"show","tool_id":id}),
            scoped("owner-a")
        )
        .await
        .unwrap()["tool"]["enabled"],
        1
    );
    reopened.close().await;
}

#[tokio::test]
async fn natural_outreach_preference_suppresses_delivery_for_its_owner_and_survives_restart() {
    let directory = tempfile::tempdir().unwrap();
    let host = Host::open(directory.path()).await;
    let own = pending_invitation(&host, "owner-a", 10);
    let other = pending_invitation(&host, "owner-b", 20);
    let set = qq_outreach::SetEnabledTool {
        outreach: host.outreach.clone(),
    };
    let status = qq_outreach::StatusTool {
        outreach: host.outreach.clone(),
        practice: None,
    };
    assert_eq!(set.definition().name, "set_outreach_enabled");
    assert_eq!(status.definition().name, "outreach_status");
    let policy = OutreachPolicy {
        cooldown_ms: 0,
        proactive_after_ms: Some(0),
    };
    assert!(
        host.outreach
            .snapshot()
            .unwrap()
            .due_for("owner-a", 100, &policy)
            .is_some()
    );

    let saved = execute(&set, json!({"enabled":false}), scoped("owner-a"))
        .await
        .unwrap();
    assert_eq!(saved["saved"], true);
    assert_eq!(saved["enabled"], false);
    assert_eq!(saved["quiet"], true);
    let snapshot = host.outreach.snapshot().unwrap();
    assert!(snapshot.quiet("owner-a"));
    assert!(!snapshot.quiet("owner-b"));
    assert!(snapshot.due_for("owner-a", 100, &policy).is_none());
    assert_eq!(
        snapshot.due_for("owner-b", 100, &policy).unwrap().id,
        other.id
    );
    assert_eq!(
        snapshot.due_proactive(100, &policy).unwrap().owner,
        "owner-b"
    );
    let shown = execute(&status, json!({}), scoped("owner-a"))
        .await
        .unwrap();
    assert_eq!(shown["quiet"], true);
    assert_eq!(shown["enabled"], false);
    assert!(shown["message"].as_str().unwrap().contains(&own.id));
    assert!(!shown.to_string().contains(&other.id));
    let other_status = execute(&status, json!({}), scoped("owner-b"))
        .await
        .unwrap();
    assert_eq!(other_status["enabled"], true);
    assert!(!other_status.to_string().contains(&own.id));
    drop(set);
    drop(status);
    host.close().await;

    let reopened = Host::open(directory.path()).await;
    assert!(reopened.outreach.snapshot().unwrap().quiet("owner-a"));
    assert!(!reopened.outreach.snapshot().unwrap().quiet("owner-b"));
    let set = qq_outreach::SetEnabledTool {
        outreach: reopened.outreach.clone(),
    };
    let restored = execute(&set, json!({"enabled":true}), scoped("owner-a"))
        .await
        .unwrap();
    assert_eq!(restored["quiet"], false);
    assert_eq!(restored["enabled"], true);
    assert_eq!(
        reopened
            .outreach
            .snapshot()
            .unwrap()
            .due_for("owner-a", 100, &policy)
            .unwrap()
            .id,
        own.id
    );
    drop(set);
    reopened.close().await;
    let again = Host::open(directory.path()).await;
    assert!(!again.outreach.snapshot().unwrap().quiet("owner-a"));
    again.close().await;
}

#[tokio::test]
async fn natural_management_rejects_missing_scope_cancelled_calls_and_identity_injection() {
    let directory = tempfile::tempdir().unwrap();
    let host = Host::open(directory.path()).await;
    let skill_id = skill_candidate(&host, "owner-a", 1, None, true)
        .skill
        .unwrap()
        .skill_id;
    let tool_id = forged_candidate(&host, "owner-a", 1, true)
        .tool
        .unwrap()
        .tool_id;
    let tools: Vec<(Box<dyn Tool>, Value)> = vec![
        (
            Box::new(qq_skill::ManageTool {
                skills: host.skills.clone(),
            }),
            json!({"action":"disable","skill_id":skill_id}),
        ),
        (
            Box::new(qq_tools::ManageTool {
                tools: host.tools.clone(),
            }),
            json!({"action":"disable","tool_id":tool_id}),
        ),
        (
            Box::new(qq_outreach::SetEnabledTool {
                outreach: host.outreach.clone(),
            }),
            json!({"enabled":false}),
        ),
        (
            Box::new(qq_outreach::StatusTool {
                outreach: host.outreach.clone(),
                practice: None,
            }),
            json!({}),
        ),
    ];
    let before = [
        host.raw(SKILL_PLUGIN_ID, SKILL_STATE_KEY),
        host.raw(TOOLFORGE_PLUGIN_ID, TOOLFORGE_STATE_KEY),
        host.raw(OUTREACH_PLUGIN_ID, OUTREACH_STATE_KEY),
    ];
    for (tool, arguments) in &tools {
        tool.validate_arguments(arguments).unwrap();
        assert!(
            execute(
                tool.as_ref(),
                arguments.clone(),
                ToolExecutionContext::new()
            )
            .await
            .is_err()
        );
        let cancelled = scoped("owner-a");
        cancelled.cancellation_handle().cancel();
        assert!(
            execute(tool.as_ref(), arguments.clone(), cancelled)
                .await
                .is_err()
        );
        assert!(
            execute(
                tool.as_ref(),
                arguments.clone(),
                scoped(crate::qq_group::USER)
            )
            .await
            .is_err()
        );
        assert_eq!(host.raw(SKILL_PLUGIN_ID, SKILL_STATE_KEY), before[0]);
        assert_eq!(
            host.raw(TOOLFORGE_PLUGIN_ID, TOOLFORGE_STATE_KEY),
            before[1]
        );
        assert_eq!(host.raw(OUTREACH_PLUGIN_ID, OUTREACH_STATE_KEY), before[2]);
        let mut injected = arguments.clone();
        injected["user_id"] = json!("owner-b");
        assert!(tool.validate_arguments(&injected).is_err());
        assert!(
            execute(tool.as_ref(), injected, scoped("owner-a"))
                .await
                .is_err()
        );
    }
    let preference = qq_outreach::SetEnabledTool {
        outreach: host.outreach.clone(),
    };
    assert!(
        execute(
            &preference,
            json!({"enabled":false}),
            scoped("owner-a").with_delegated(true)
        )
        .await
        .is_err()
    );
    assert_eq!(host.raw(SKILL_PLUGIN_ID, SKILL_STATE_KEY), before[0]);
    assert_eq!(
        host.raw(TOOLFORGE_PLUGIN_ID, TOOLFORGE_STATE_KEY),
        before[1]
    );
    assert_eq!(host.raw(OUTREACH_PLUGIN_ID, OUTREACH_STATE_KEY), before[2]);
    host.close().await;
}
