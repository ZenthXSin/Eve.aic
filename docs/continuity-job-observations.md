# 生活接续的任务状态观察（049dcf6 基线）

`agents.send`/`delegate_task` 只等待持久准入；`agents.status` 读取子代理服务当时的持久投影，
二者不是同一时刻的状态。`agents.notifications` 只通知顶层交付，不通知开始运行或子任务创建。
生活接续旧实现只索引工具回执外层的 `jobId/status`，忽略已完成脚本的结果页内返回的任务。

实际原件（前轮按固定文件定位各只读一次，本轮复用已保存定位摘要；两个不含任务正文的回执保存在离线 fixture）：
- 意识记录 70268，2026-11-18T11:01:04.508Z，运行源码
  `049dcf6c63c290193366a1e9940a5b21f755072b`：派工返回 `eve_70266_0_task/queued`。
- 子代理记录 12152 accepted，11:01:03.959Z；12153 started，11:01:03.966Z。
  准入回执在保存前任务已开始，不能把回执保存时刻冒充状态取样时刻。
- 意识记录 70488，11:41:52.987Z：脚本已完成，完整结果页返回同一任务
  `running` 和三个 `childJobIds`。旧索引只记外层脚本已完成，遗漏内层任务更新。
- 后续生活接续记录 70495（11:42:55.021Z）、70502（11:44:37.587Z）仍为 `queued`。
  因而不只是两次正常观察的差异，还存在已知回执的展示投影遗漏。

### 历史证据定位

前轮直接读取 `/var/lib/eve-rebuild/backend/` 下的真实 journal 文件，不调用在线状态接口，
不打开会写锁的 journal 实例、不读取子代理正文 bodies；只提取编号、时刻、kind、任务标识和状态。
以下路径均相对此目录：

| 记录 | 原件定位 | 核对字段 | 原件 digest |
| --- | --- | --- | --- |
| 12152 | `agents/events/0000000000012152.json` | `kind=accepted`，`data.agentId` 与 12153 相同；70268 的 `requestSeq=12152` 定位此准入记录 | `3eb32fd2b6df39acb3ec23b0e2e4b3bb9df50baa0ef1b3376d3067fa6ad7e7b8` |
| 12153 | `agents/events/0000000000012153.json` | `kind=started`，`data.jobId=eve_70266_0_task`；previous 等于 12152 digest | `c1a8842fb5e9596165d1ec07978b9ba78a71b1feb6cb91ae3e4ca7e7427fa493` |
| 70268 | `consciousness/events/0000000000070268.json` | `data.result.output` 外层 JSON：同一 jobId、queued、requestSeq 12152 | `d9f0beea5aab75ed929082fc6e7aa4b54e1ef1a5cc204b73634202bf2baa2834` |
| 70488 | `consciousness/events/0000000000070488.json` | `data.result.output` 的 `result.text` JSON：同一 jobId、running、3 个 childJobIds | `56d7721857212eb065dfe475744cfba81491454a6a6d2d6627755ce69a7fc0e7` |
| 70495 | `consciousness/events/0000000000070495.json` | `kind=context_state`，`data.messages[0].content` 第二行 JSON 的 `pendingJobs[0]` 仍为该 jobId/queued | `b7166380501120de5b6e0c93d03b3f948c91cfeeac00b44aa6796dee18506d57` |
| 70502 | `consciousness/events/0000000000070502.json` | 同上，仍为该 jobId/queued | `028e9306c44a0331ed05b0b92805b960fa62b38654c966a0e1496eee4e76d0a0` |

这些是前轮实际核对到的原件，不是仅引用外部结论。70268、70488 的 `data.sourceCommit`
均为 `049dcf6c63c290193366a1e9940a5b21f755072b`；70495、70502 本身无 sourceCommit，
不把上下文记录冒充独立版本证明。另用只读
`git show 049dcf6c63c290193366a1e9940a5b21f755072b:plugins/eve/runtime.ts`
核对基线 `#rememberJobSummary/#rememberCompletions/#rememberJob`（约 173–202 行）确实只解析外层 JSON。
脱敏定位摘要和文件 SHA-256 保存在仓库外
`/var/lib/eve-workspace/tmp/pending-job-review-verification/history-evidence.json`；fixture 仍只保留原来的两条。

新展示单独维护观察索引，不改用于完成去重/图像关联的原索引、执行或唤醒语义，不新增查询。
保留 `jobId/status`，增加 `observation.record/recordedAt/source`；`recordedAt` 仅是意识回执保存
时刻，不是服务端实时快照时刻。来源缺失时为 null，queued 明示可能只是准入状态。
只用已有完整脚本第一页、合法且已跟踪的 AgentJob-shaped 返回值补充非终态，不递归搜索任意嵌套输出，
不读取/推断子任务进度。缺失、截断、未知或非任务返回值不推测 running。
终态退出 pendingJobs，unknown 保留在未知计数并按原契约不复活；终态不能被迟到回执复活，
已运行不能退回迟到的 queued/checking，checking 也不能退回迟到的 queued。重启用已接受的原件重建相同展示。

## 多久刷新、来源与非法输入

**没有固定刷新周期，也没有定时状态查询。** 展示索引只在新的合格回执写入 journal 并被消费时更新，
生活接续随后展示该索引；重启时从活动原件重建同样结果。为任务展示不新增
`agents.status`、`agents.list` 或子任务查询。

被拒的非法/不完整输出，以及 running 之后迟到的 queued/checking、终态后迟到的非终态等回执，
不会更新该任务的状态、`observation.record` 或保存时刻。相同状态的合格新观察可以更新
记录编号和保存时刻（如 running → running）；终态冻结、unknown 不被非终态复活的保护仍优先。
回执保存时刻不等于服务端取样时刻，回放顺序依据活动 journal 的事件顺序，不按墙钟推断进度。

来源使用确定取值：`tool_receipt`（工具回执外层）、`script_result`（完整脚本结果页）、
`agents_notification`（agents notice）、`scripts_notification`（scripts notice）。
notice 的来源必须严格是字符串 agents/scripts，不再把数组来源强制转换成合法来源。

P2 修复要求任何入口的 `job.status` **先是字符串，再属于合法枚举**，不使用 `String()` 兜底。
数组（包括 `["running"]`、`["completed"]`、`["unknown"]`）、数字、布尔、null、对象等
不能推进、退休或新建任务，也不能替换该任务的 observation；不阻碍后续合法回执。
工具和脚本完成信封仍严格要求字符串 `completed`，非法信封不能授权读取内层合法进度。

### 当前口径：脚本永不建立终态（取代身份证明方案）

主人在第二次审查后决定：不再以 agentId/requestSeq 绑定“证明”用户脚本来源，
删除准入身份索引及比对逻辑。AgentJob 外形及身份字段都不是来源认证。

- `script_result` 返回 completed/failed/cancelled/unknown **一律只记录 scriptReports**，
  不改该 jobId 的 jobs 状态、不退出 pendingJobs、不更新其 observation；即使完整形状和身份
  与准入完全相同也不例外，缺身份字段或非整数身份同样不据此退休。
- 脚本只可补充已跟踪任务的 queued/checking/running 非终态展示。为保留不猜进度的边界，
  仍要求完整第一页、合法 AgentJob 形状（非空 agentId、正安全整数 requestSeq、合法 childJobIds），
  但不与准入身份比对；缺形状或拿不准的合法 jobId/status 值只列为报告。
- 未跟踪 jobId 只进 scriptReports，不新建任务。running 拒绝 queued/checking，checking 拒绝
  queued；已终态/unknown 的任务冻结。脚本被这些保护拒绝时可增加报告，但任务观察编号/时刻不变。
- 终态/unknown 仅由 tool_receipt、agents_notification、scripts_notification 外层服务端回执建立。
  服务端 unknown 可由后续服务端终态解决；脚本 completed 也不能解决它。

`scriptReports` 只包含 jobId、reportedStatus、observation.record/recordedAt/source，最多保留
最近五个 jobId 的最后报告。它们是**脚本自述、未经服务端证明的展示线索，不据此退休任务或更新
pendingJobs**，不进入 unknownJobCount、不触发执行/唤醒/查询。后续合格非终态观察可与旧报告
并存；重启按活动原件重建相同观察及报告列表。未解析的截断/不完整页不是合格观察，不读后续页补证。

真实 70268→70488 仍以完整 script_result 补充 running / record 70488，scriptReports 为空。
`tests/agents_wake_test.ts` 的 queue(id) 恢复仅包含参数 jobId + queued 的准入输入，
身份合成字段删除后完整集成测试仍通过；所有既有断言（含 running 和终态防复活）、执行/唤醒逻辑不变。
`runtime.ts` 无需接线改动，仍仅消费原有 journal 回执。

## 修改前后对照

以下是对同一份 `tests/fixtures/continuity-agent-status.json` 顺序回放意识记录
70268、70488 后，实际调用 `continuityMessage` 所得的 `pendingJobs` JSON，
不是手写期望值，也不是两次不同时间的在线状态查询。fixture SHA-256：
`d5fc9664670d38d3f02a1bf86dec34bc59bb000821edd31a34d068c96acf3f55`。

以下为最初修复时保存的真实基线对照（历史命令；当时 detached worktree 状态为空，已清理）。
本轮只在现有分支追加新提交，不新建分支/worktree、不重新执行 worktree add：

```bash
git worktree add --detach /var/lib/eve-workspace/tmp/pending-job-baseline 049dcf6c63c290193366a1e9940a5b21f755072b
chmod -R a-w /var/lib/eve-workspace/tmp/pending-job-baseline
```

一次性脚本 `/var/lib/eve-workspace/tmp/pending-job-comparison.ts` 留在仓库外，
SHA-256 为 `1883714fc59d1b004242df19d6e51f4de9347282afca5404ce4649fbaa50df71`。
它读取上述 fixture，分别导入对应版本的 `continuityMessage`，并按 `activeEvents` 回放：

- `baseline` 从基线 `runtime.ts` 原样提取 `#observedJobs`、`#rememberJobSummary`、
  `#rememberCompletions`、`#rememberJob` 到离线包装类；按基线初始化的事件分支调用，
  只索引 `tool_result` 外层 JSON 的 `jobId/status`，不解析脚本结果页。
- `current` 使用当前 worktree 的 `JobObservations`；两者均从真实生活接续消息中解析
  `pendingJobs` 再打印。脚本不实例化 runtime、不调用服务，命令仅授予只读权限，未授予网络权限。

### 修改前：049dcf6 基线

真实执行命令（工作目录均为 `/var/lib/eve-workspace/projects/eve-pending-job-observation`）：

```bash
/tmp/eve-agent-recovery-tools/deno run --no-config --no-lock --allow-read=/var/lib/eve-workspace/tmp/pending-job-baseline,/var/lib/eve-workspace/projects/eve-pending-job-observation /var/lib/eve-workspace/tmp/pending-job-comparison.ts baseline | tee /var/lib/eve-workspace/tmp/pending-job-verification/baseline-output.json
```

标准输出（退出码 0）：

```json
{
  "pendingJobs": [
    {
      "jobId": "eve_70266_0_task",
      "status": "queued"
    }
  ]
}
```

### 修改后：当前 worktree

真实执行命令：

```bash
/tmp/eve-agent-recovery-tools/deno run --no-config --no-lock --allow-read=/var/lib/eve-workspace/tmp/pending-job-baseline,/var/lib/eve-workspace/projects/eve-pending-job-observation /var/lib/eve-workspace/tmp/pending-job-comparison.ts current | tee /var/lib/eve-workspace/tmp/pending-job-verification/current-output.json
```

标准输出（退出码 0）：

```json
{
  "pendingJobs": [
    {
      "jobId": "eve_70266_0_task",
      "status": "running",
      "observation": {
        "record": 70488,
        "recordedAt": "2026-11-18T11:41:52.987Z",
        "source": "script_result"
      }
    }
  ]
}
```

同一次 70488 已保存 `running` 回执之后，基线仍显示 `queued`；修改后显示 `running`，
并标明 `script_result` 来源及意识记录 70488。两个对照均不发起任何状态查询，
三个 `childJobIds` 未被展示索引推断成子任务状态。`recordedAt` 仍仅表示回执保存时刻。

### 本轮新口径对照：合法脚本 completed 不再改变任务状态

对照对象是父提交 `0a4d7f9f82516a21198e8cf7d95b29e586fb95c4` 的编辑前副本与当前修复。
一次性脚本 `/var/lib/eve-workspace/tmp/pending-job-nonterminal-comparison.ts` 留在仓库外，
只读取同一真实 fixture 和保存的旧实现，不实例化 runtime、不调用服务。
先回放 70268→70488，再把同一 AgentJob 的合法字符串 status 改为四种退休状态，
放入完整零动作脚本结果页。72000/时刻是合成测试事件，不是历史 journal 原件。

真实命令（当前 worktree 为工作目录）：

```bash
/tmp/eve-agent-recovery-tools/deno run --no-config --no-lock --allow-read=/var/lib/eve-workspace/tmp/pending-job-nonterminal-verification,/var/lib/eve-workspace/projects/eve-pending-job-observation /var/lib/eve-workspace/tmp/pending-job-nonterminal-comparison.ts before | tee /var/lib/eve-workspace/tmp/pending-job-nonterminal-verification/before.jsonl
/tmp/eve-agent-recovery-tools/deno run --no-config --no-lock --allow-read=/var/lib/eve-workspace/tmp/pending-job-nonterminal-verification,/var/lib/eve-workspace/projects/eve-pending-job-observation /var/lib/eve-workspace/tmp/pending-job-nonterminal-comparison.ts after | tee /var/lib/eve-workspace/tmp/pending-job-nonterminal-verification/after.jsonl
```

修改前真实输出（退出码 0）：

```jsonl
{"case":"fixture_70268_70488","pendingJobs":[{"jobId":"eve_70266_0_task","status":"running","observation":{"record":70488,"recordedAt":"2026-11-18T11:41:52.987Z","source":"script_result"}}],"scriptReports":[]}
{"case":"script_retirement_report","reportedStatus":"completed","pendingJobs":[],"scriptReports":[]}
{"case":"script_retirement_report","reportedStatus":"failed","pendingJobs":[],"scriptReports":[]}
{"case":"script_retirement_report","reportedStatus":"cancelled","pendingJobs":[],"scriptReports":[]}
{"case":"script_retirement_report","reportedStatus":"unknown","pendingJobs":[],"scriptReports":[]}
```

修改后真实输出（退出码 0）：

```jsonl
{"case":"fixture_70268_70488","pendingJobs":[{"jobId":"eve_70266_0_task","status":"running","observation":{"record":70488,"recordedAt":"2026-11-18T11:41:52.987Z","source":"script_result"}}],"scriptReports":[]}
{"case":"script_retirement_report","reportedStatus":"completed","pendingJobs":[{"jobId":"eve_70266_0_task","status":"running","observation":{"record":70488,"recordedAt":"2026-11-18T11:41:52.987Z","source":"script_result"}}],"scriptReports":[{"jobId":"eve_70266_0_task","reportedStatus":"completed","observation":{"record":72000,"recordedAt":"2026-11-18T12:02:00.000Z","source":"script_result"}}]}
{"case":"script_retirement_report","reportedStatus":"failed","pendingJobs":[{"jobId":"eve_70266_0_task","status":"running","observation":{"record":70488,"recordedAt":"2026-11-18T11:41:52.987Z","source":"script_result"}}],"scriptReports":[{"jobId":"eve_70266_0_task","reportedStatus":"failed","observation":{"record":72000,"recordedAt":"2026-11-18T12:02:00.000Z","source":"script_result"}}]}
{"case":"script_retirement_report","reportedStatus":"cancelled","pendingJobs":[{"jobId":"eve_70266_0_task","status":"running","observation":{"record":70488,"recordedAt":"2026-11-18T11:41:52.987Z","source":"script_result"}}],"scriptReports":[{"jobId":"eve_70266_0_task","reportedStatus":"cancelled","observation":{"record":72000,"recordedAt":"2026-11-18T12:02:00.000Z","source":"script_result"}}]}
{"case":"script_retirement_report","reportedStatus":"unknown","pendingJobs":[{"jobId":"eve_70266_0_task","status":"running","observation":{"record":70488,"recordedAt":"2026-11-18T11:41:52.987Z","source":"script_result"}}],"scriptReports":[{"jobId":"eve_70266_0_task","reportedStatus":"unknown","observation":{"record":72000,"recordedAt":"2026-11-18T12:02:00.000Z","source":"script_result"}}]}
```

合法 completed/failed/cancelled/unknown 均保留 running 及 record 70488，只新增报告；
新的报告 record 72000 不替换任务 observation。原 fixture 输出含 scriptReports=[]，未改变。

原 70268→70488 的 current 命令也再次实际运行，并与最初输出逐字核对：

```bash
/tmp/eve-agent-recovery-tools/deno run --no-config --no-lock --allow-read=/var/lib/eve-workspace/tmp/pending-job-baseline,/var/lib/eve-workspace/projects/eve-pending-job-observation /var/lib/eve-workspace/tmp/pending-job-comparison.ts current | tee /var/lib/eve-workspace/tmp/pending-job-nonterminal-verification/current-output.json
cmp /var/lib/eve-workspace/tmp/pending-job-verification/current-output.json /var/lib/eve-workspace/tmp/pending-job-nonterminal-verification/current-output.json && echo 'Original pendingJobs fixture output byte-identical.'
```

cmp 退出码 0，输出 `Original pendingJobs fixture output byte-identical.`。
完整消息 guidance 改为新口径，原 pendingJobs JSON 不变。

## 本轮验证

使用 `/tmp/eve-agent-recovery-tools/deno`，运行全部三个指定文件与六入口 check：

```bash
NO_COLOR=1 /tmp/eve-agent-recovery-tools/deno test --frozen --allow-read=/var/lib/eve-workspace/projects/eve-pending-job-observation,/tmp --allow-write=/tmp tests/job_observations_test.ts tests/continuity_test.ts tests/agents_wake_test.ts
NO_COLOR=1 /tmp/eve-agent-recovery-tools/deno check --frozen plugins/eve/continuity.ts plugins/eve/runtime.ts plugins/eve/job-observations.ts tests/job_observations_test.ts tests/continuity_test.ts tests/agents_wake_test.ts
NO_COLOR=1 /tmp/eve-agent-recovery-tools/deno test --frozen --filter='/script terminal|checking rejects|running rejects|four ingress/' tests/job_observations_test.ts
```

`job_observations_test.ts` 13 passed、`continuity_test.ts` 12 passed、`agents_wake_test.ts` 2 passed；
`ok | 27 passed | 0 failed (4s)`，退出码 0。六入口 check 全部通过，退出码 0。
矩阵按照新口径明确区分：三个服务端入口可以建立终态/unknown，script_result 只能报告；
服务端冻结后，四个迟到入口都不能复活，script_result 的任何终态也不能解决 unknown。
每一步用新索引重放同顺序，比较完整 jobs、pendingJobs、scriptReports。
checking 和 running 的四入口防回退均断言状态及 observation.record 不变、重放一致。

专项真实输出：

```text
Check tests/job_observations_test.ts
running 4 tests from ./tests/job_observations_test.ts
running rejects late queued/checking through all four ingress paths and replay ... ok (22ms)
script terminal/unknown strings only report regardless of identity or missing fields ... ok (111ms)
checking rejects late queued through all four ingress paths and replay ... ok (3ms)
four ingress × terminal/unknown policy and freeze protections match restart replay ... ok (208ms)

ok | 4 passed | 0 failed | 9 filtered out (434ms)

```

身份绑定与“脚本可自证”的测试已按主人新口径替换为终态只报告测试，覆盖正确/错误身份、
requestSeq=1、缺字段和非整数。保留非法 status、截断结果页、终态冻结、重放、缓存和唤醒验证。
完整日志在 `/var/lib/eve-workspace/tmp/pending-job-nonterminal-verification/tests.log`、`deno-check.log`。
没有 agents.status/list/子任务新增查询；仅同分支新提交，未推送、部署或写 source/运行目录。

## 残余边界

非终态展示仍是已保存的脚本信息，不是实时状态或来源认证；完整形状的伪造 running 仍可能影响
未冻结任务展示，但不能建立终态/unknown 或退休任务。没有服务端终态通知时可能继续展示旧 running；
脚本完成报告只作为线索，不为消除这个差异自动轮询。
