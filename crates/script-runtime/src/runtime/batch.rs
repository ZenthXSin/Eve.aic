//! 并发只遵循宿主工具契约；每个叶子保留独立身份、预算与持久证据。
use super::*;
use std::task::Poll;

impl ScriptRuntime {
    pub(super) async fn tool_batch(
        &self,
        calls: Vec<crate::CodeToolCall>,
        version: &PublishedVersion,
        receipt: &mut Receipt,
        context: &ToolExecutionContext,
        chain: &Chain,
        count: usize,
    ) -> Result<Value> {
        if calls.is_empty() || calls.len() > 8 {
            return Err(reject("工具批次只接受1至8个叶子"));
        }
        let calls = calls
            .into_iter()
            .enumerate()
            .map(|(index, call)| {
                if !version.spec.tools.contains(&call.name) {
                    return Err(reject("脚本未声明该叶子工具"));
                }
                let call = ToolCall {
                    id: format!("{}-tool-{count}-{index}", receipt.call_id),
                    name: call.name,
                    arguments: resolve_workspace(call.arguments, chain.work.path())?,
                };
                let ctx = context.clone().with_call_id(call.id.clone());
                self.dispatch.preflight_call(&call, &ctx)?;
                Ok((call, ctx))
            })
            .collect::<Result<Vec<_>>>()?;
        check_stop(context, chain.deadline)?;
        if !context
            .shared_steps()
            .ok_or_else(|| reject("缺少共享预算"))?
            .reserve(calls.len())
            .map_err(fail)?
        {
            return Err(reject("调用链共享工具预算不足"));
        }
        let parallel = calls
            .iter()
            .all(|(call, _)| self.dispatch.parallel_safe(&call.name));
        let start = receipt.evidence.len();
        for (call, _) in &calls {
            receipt.evidence.push(Evidence {
                operation: "tool".into(),
                target: call.name.clone(),
                status: ReceiptStatus::Running,
                result_digest: None,
            });
        }
        self.store.record(receipt)?;
        let mut futures: Vec<Option<eve_llm_api::ToolFuture<'_>>> = calls
            .into_iter()
            .map(|(call, ctx)| {
                let future: eve_llm_api::ToolFuture<'_> = Box::pin(async move {
                    check_stop(&ctx, chain.deadline)?;
                    guarded(
                        self.dispatch.dispatch(call, ctx.clone()),
                        &ctx,
                        chain.deadline,
                    )
                    .await
                });
                Some(future)
            })
            .collect();
        let mut values = vec![Value::Null; futures.len()];
        let mut error = None;
        for next in 0..futures.len() {
            let (index, outcome) = std::future::poll_fn(|cx| {
                for (index, future) in futures.iter_mut().enumerate() {
                    if !parallel && index != next {
                        continue;
                    }
                    if let Some(future) = future
                        && let Poll::Ready(value) = future.as_mut().poll(cx)
                    {
                        return Poll::Ready((index, value));
                    }
                }
                Poll::Pending
            })
            .await;
            futures[index] = None;
            let evidence = &mut receipt.evidence[start + index];
            match outcome {
                Ok(value) => {
                    evidence.result_digest = Some(digest(&value)?);
                    evidence.status =
                        if value.get("success").and_then(Value::as_bool) == Some(false) {
                            error.get_or_insert_with(|| fail("叶子工具确认失败"));
                            ReceiptStatus::Failed
                        } else {
                            ReceiptStatus::Succeeded
                        };
                    values[index] = value;
                }
                Err(failure) => {
                    evidence.status = if matches!(failure, ToolExecutionError::Rejected(_)) {
                        ReceiptStatus::Rejected
                    } else {
                        ReceiptStatus::Unknown
                    };
                    error.get_or_insert(failure);
                }
            }
            if !parallel && error.is_some() {
                for evidence in &mut receipt.evidence[start + next + 1..] {
                    evidence.operation = "tool_not_started".into();
                    evidence.status = ReceiptStatus::Rejected;
                }
                self.store.record(receipt)?;
                break;
            }
            self.store.record(receipt)?;
        }
        match error {
            Some(error) => Err(error),
            None => Ok(json!({"values":values,"parallel":parallel})),
        }
    }
}
