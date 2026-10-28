use crate::{
    Evidence, MAX_MESSAGE_BYTES, MAX_SOURCE_BYTES, MigrationScript, Receipt, ReceiptStatus,
    Request, Result, ScriptRuntime, StateMigration, VersionRef, digest, fail, reject,
    runtime::{check_stop, guarded, reserve, send},
    store::strict_json,
};
use eve_llm_api::ToolExecutionContext;
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

impl ScriptRuntime {
    /// 纯状态计算不调用 dispatch；候选状态与源 revision 在激活提交时再次核对。
    pub async fn activate_with_script(
        &self,
        target: &VersionRef,
        call_id: &str,
        migration: MigrationScript,
        context: ToolExecutionContext,
    ) -> Result<Receipt> {
        crate::model::valid_name(call_id)?;
        let owner = context.scope().ok_or_else(|| reject("迁移缺少可信身份"))?;
        if migration.source.is_empty() || migration.source.len() > MAX_SOURCE_BYTES {
            return Err(reject("迁移源码为空或超限"));
        }
        self.sandbox.preflight(&migration.language)?;
        let deadline = context
            .deadline()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(30))
            .min(Instant::now() + Duration::from_secs(30));
        check_stop(&context, deadline)?;
        let state = self.store.migration_source(
            owner,
            target,
            &migration.from,
            migration.expected_revision,
            call_id,
        )?;
        let mut program = self.store.version(owner, target)?;
        program.spec.language = migration.language.clone();
        program.spec.source = migration.source.clone();
        program.spec.tools.clear();
        program.spec.dependencies.clear();
        reserve(&context)?;
        let work = tempfile::tempdir().map_err(fail)?;
        let mut source = tempfile::NamedTempFile::new().map_err(fail)?;
        std::io::Write::write_all(&mut source, migration.source.as_bytes()).map_err(fail)?;
        let compiled = if migration.language == crate::Language::Rust {
            Some(
                self.sandbox
                    .prepare_rust(&migration.source, &context, deadline)
                    .await?,
            )
        } else {
            None
        };
        let executable = compiled
            .as_ref()
            .map(|compiled| compiled.binary.as_path())
            .unwrap_or(source.path());
        let mut child = self.sandbox.spawn(&program, executable, work.path())?;
        let mut input = child.stdin.take().ok_or_else(|| fail("迁移缺少stdin"))?;
        let mut output = BufReader::new(child.stdout.take().ok_or_else(|| fail("迁移缺少stdout"))?);
        send(&mut input, &json!({"protocol":1,"mode":"migration","call_id":call_id,
            "from":migration.from,"version":target,"state":state,"target_schema":program.spec.state_schema}),
            &context, deadline).await?;
        drop(input);
        let mut bytes = Vec::new();
        let length = guarded(
            async {
                (&mut output)
                    .take((MAX_MESSAGE_BYTES + 1) as u64)
                    .read_until(b'\n', &mut bytes)
                    .await
                    .map_err(fail)
            },
            &context,
            deadline,
        )
        .await?;
        if length == 0 || length > MAX_MESSAGE_BYTES || !bytes.ends_with(b"\n") {
            return Err(reject("迁移输出无效或超限"));
        }
        let request: Request = serde_json::from_value(strict_json(&bytes)?).map_err(reject)?;
        let Request::Return { value } = request else {
            return Err(reject("纯迁移只允许return；禁止工具、依赖、写入及产物操作"));
        };
        // 必须恰好一条返回并正常退出；无界输出/挂起仍受期限和取消约束。
        let mut extra = [0u8; 1];
        let trailing = guarded(
            async { output.read(&mut extra).await.map_err(fail) },
            &context,
            deadline,
        )
        .await?;
        if trailing != 0 {
            return Err(reject("迁移返回后有额外输出"));
        }
        let status = guarded(
            async { child.wait().await.map_err(fail) },
            &context,
            deadline,
        )
        .await?;
        if !status.success() {
            return Err(fail("迁移进程异常退出"));
        }
        program.spec.state_schema.validate(&value)?;
        check_stop(&context, deadline)?;
        let receipt = Receipt {
            call_id: call_id.into(),
            parent_call_id: None,
            root_call_id: call_id.into(),
            owner: owner.clone(),
            version: target.clone(),
            request_digest: digest(&migration)?,
            status: ReceiptStatus::Succeeded,
            result: Some(json!({"activated":target})),
            evidence: vec![Evidence {
                operation: "state_migration".into(),
                target: migration.from.digest.clone(),
                status: ReceiptStatus::Succeeded,
                result_digest: Some(digest(&value)?),
            }],
            error: None,
            failure_diagnostic: None,
        };
        self.store.activate_with_receipt(
            owner,
            target,
            Some(StateMigration {
                from: migration.from,
                expected_revision: migration.expected_revision,
                value,
            }),
            Some(receipt.clone()),
        )?;
        Ok(receipt)
    }
}
