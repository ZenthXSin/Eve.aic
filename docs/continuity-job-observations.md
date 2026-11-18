# 生活接续的任务状态观察（049dcf6 基线）

`agents.send`/`delegate_task` 只等待持久准入；`agents.status` 读取子代理服务当时的持久投影，
二者不是同一时刻的状态。`agents.notifications` 只通知顶层交付，不通知开始运行或子任务创建。
生活接续旧实现只索引工具回执外层的 `jobId/status`，忽略已完成脚本的结果页内返回的任务。

实际原件（本次按固定文件定位各只读一次；两个不含任务正文的回执保存在离线 fixture）：
- 意识记录 70268，2026-11-18T11:01:04.508Z，运行源码
  `049dcf6c63c290193366a1e9940a5b21f755072b`：派工返回 `eve_70266_0_task/queued`。
- 子代理记录 12152 accepted，11:01:03.959Z；12153 started，11:01:03.966Z。
  准入回执在保存前任务已开始，不能把回执保存时刻冒充状态取样时刻。
- 意识记录 70488，11:41:52.987Z：脚本已完成，完整结果页返回同一任务
  `running` 和三个 `childJobIds`。旧索引只记外层脚本已完成，遗漏内层任务更新。
- 后续生活接续记录 70495（11:42:55.021Z）、70502（11:44:37.587Z）仍为 `queued`。
  因而不只是两次正常观察的差异，还存在已知回执的展示投影遗漏。

### 历史证据定位

本次直接读取 `/var/lib/eve-rebuild/backend/` 下的真实 journal 文件，不调用在线状态接口，
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

这些是本次实际核对到的原件，不是仅引用外部结论。70268、70488 的 `data.sourceCommit`
均为 `049dcf6c63c290193366a1e9940a5b21f755072b`；70495、70502 本身无 sourceCommit，
不把上下文记录冒充独立版本证明。另用只读
`git show 049dcf6c63c290193366a1e9940a5b21f755072b:plugins/eve/runtime.ts`
核对基线 `#rememberJobSummary/#rememberCompletions/#rememberJob`（约 173–202 行）确实只解析外层 JSON。
脱敏定位摘要和文件 SHA-256 保存在仓库外
`/var/lib/eve-workspace/tmp/pending-job-review-verification/history-evidence.json`；fixture 仍只保留原来的两条。

新展示单独维护观察索引，不改用于完成去重/图像关联的原索引、执行或唤醒语义，不新增查询。
保留 `jobId/status`，增加 `observation.record/recordedAt/source`；`recordedAt` 仅是意识回执保存
时刻，不是服务端实时快照时刻。来源缺失时为 null，queued 明示可能只是准入状态。
只消费已有完整脚本第一页、合法且已跟踪的 AgentJob-shaped 返回值，不递归搜索任意嵌套输出，
不读取/推断子任务进度。缺失、截断、未知或非任务返回值不推测 running。
终态退出 pendingJobs，unknown 保留在未知计数并按原契约不复活；终态不能被迟到回执复活，
已运行不能退回迟到的 queued/checking。重启用已接受的原件重建相同展示。

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

## 修改前后对照

以下是对同一份 `tests/fixtures/continuity-agent-status.json` 顺序回放意识记录
70268、70488 后，实际调用 `continuityMessage` 所得的 `pendingJobs` JSON，
不是手写期望值，也不是两次不同时间的在线状态查询。fixture SHA-256：
`d5fc9664670d38d3f02a1bf86dec34bc59bb000821edd31a34d068c96acf3f55`。

基线以 detached worktree 只读核对，未修改源码，运行前 `git status --short` 为空：

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

### P2/P3 复核：合法 fixture 不变，非法形状不再当成状态

本次重新运行上面的 `current` 命令，并与原 `current-output.json` 用 `cmp` 比较：
输出逐字相同（退出码 0），仍为 record 70488 / running / script_result。
基线 049dcf6 的原输出保留；本次未重新创建基线 worktree。

下面是**本次 P2 修复前的展示实现与修复后实现**的同输入对照，不是 049dcf6 与当前版本对照。
一次性脚本 `/var/lib/eve-workspace/tmp/pending-job-review-comparison.ts` 从真实 fixture 中取
准入及完整脚本/AgentJob 形状，只在内层 status 注入三个非法数组；之后重放一条合法 queued。
70510/70511 及其保存时刻是离线合成测试事件，不是历史原件编号。
修复前实现来自本轮编辑前保存在仓库外的 `job-observations.ts` 副本，未改动仓库来回切换版本。
两次均只读、无网络或服务调用。

真实命令：

```bash
/tmp/eve-agent-recovery-tools/deno run --no-config --no-lock --allow-read=/var/lib/eve-workspace/tmp/pending-job-review-verification,/var/lib/eve-workspace/projects/eve-pending-job-observation /var/lib/eve-workspace/tmp/pending-job-review-comparison.ts before | tee /var/lib/eve-workspace/tmp/pending-job-review-verification/p2-before-output.jsonl
/tmp/eve-agent-recovery-tools/deno run --no-config --no-lock --allow-read=/var/lib/eve-workspace/tmp/pending-job-review-verification,/var/lib/eve-workspace/projects/eve-pending-job-observation /var/lib/eve-workspace/tmp/pending-job-review-comparison.ts after | tee /var/lib/eve-workspace/tmp/pending-job-review-verification/p2-after-output.jsonl
```

修复前真实标准输出（JSON Lines，退出码 0）：

```jsonl
{"inputStatus":["running"],"afterInvalid":[{"jobId":"eve_70266_0_task","status":"running","observation":{"record":70510,"recordedAt":"2026-11-18T12:01:10.510Z","source":"script_result"}}],"afterLegalQueued":[{"jobId":"eve_70266_0_task","status":"running","observation":{"record":70510,"recordedAt":"2026-11-18T12:01:10.510Z","source":"script_result"}}]}
{"inputStatus":["completed"],"afterInvalid":[],"afterLegalQueued":[]}
{"inputStatus":["unknown"],"afterInvalid":[],"afterLegalQueued":[]}
```

修复后真实标准输出（JSON Lines，退出码 0）：

```jsonl
{"inputStatus":["running"],"afterInvalid":[{"jobId":"eve_70266_0_task","status":"queued","observation":{"record":70268,"recordedAt":"2026-11-18T11:01:04.508Z","source":"tool_receipt"}}],"afterLegalQueued":[{"jobId":"eve_70266_0_task","status":"queued","observation":{"record":70511,"recordedAt":"2026-11-18T12:01:10.511Z","source":"tool_receipt"}}]}
{"inputStatus":["completed"],"afterInvalid":[{"jobId":"eve_70266_0_task","status":"queued","observation":{"record":70268,"recordedAt":"2026-11-18T11:01:04.508Z","source":"tool_receipt"}}],"afterLegalQueued":[{"jobId":"eve_70266_0_task","status":"queued","observation":{"record":70511,"recordedAt":"2026-11-18T12:01:10.511Z","source":"tool_receipt"}}]}
{"inputStatus":["unknown"],"afterInvalid":[{"jobId":"eve_70266_0_task","status":"queued","observation":{"record":70268,"recordedAt":"2026-11-18T11:01:04.508Z","source":"tool_receipt"}}],"afterLegalQueued":[{"jobId":"eve_70266_0_task","status":"queued","observation":{"record":70511,"recordedAt":"2026-11-18T12:01:10.511Z","source":"tool_receipt"}}]}
```

修复前 `["completed"]` / `["unknown"]` 隐藏任务且合法 queued 无法恢复；修复后均保留
queued/record 70268，随后合法 queued 可更新至 record 70511。
`["running"]` 也不再误报 running 或更新观察。

新增用例真实运行命令及输出（退出码 0）：

```bash
NO_COLOR=1 /tmp/eve-agent-recovery-tools/deno test --frozen --filter='/non-string statuses|running rejects|qualified same-state/' tests/job_observations_test.ts
```

```text
Check tests/job_observations_test.ts
running 3 tests from ./tests/job_observations_test.ts
non-string statuses cannot advance, retire or replace an observation through any ingress ... ok (141ms)
running rejects late checking without changing its observation ... ok (7ms)
qualified same-state observations refresh record/time and distinguish notification sources ... ok (2ms)

ok | 3 passed | 0 failed | 4 filtered out (167ms)

```

非法 status 用例覆盖全部四个入口（工具外层、完整脚本返回、agents notice、scripts notice），
分别验证 queued/running 及 observation 不变、后续合法回执正常、不新建非法任务，
并检查非法工具/脚本完成信封不能授权内层状态。新增 checking 用例对四入口显式断言不回退；
同状态用例核对记录编号/时刻刷新及两个 notice 来源，拒绝数组来源。
新增非法状态用例在修复前确实失败：`tool_receipt: ["running"]` 被误判为 running/record 70510，
`0 passed | 1 failed`；失败原始日志为 `p2-before-test.log`，未放宽既有断言。

## 本次验证

使用 `/tmp/eve-agent-recovery-tools/deno`（Deno 2.9.6），新增三项严格回归用例，未放宽既有断言，未改执行/唤醒代码。

```bash
NO_COLOR=1 /tmp/eve-agent-recovery-tools/deno check --frozen plugins/eve/continuity.ts plugins/eve/runtime.ts plugins/eve/job-observations.ts tests/job_observations_test.ts tests/continuity_test.ts tests/agents_wake_test.ts
NO_COLOR=1 /tmp/eve-agent-recovery-tools/deno test --frozen --allow-read=/var/lib/eve-workspace/projects/eve-pending-job-observation,/tmp --allow-write=/tmp tests/job_observations_test.ts tests/continuity_test.ts tests/agents_wake_test.ts
```

类型检查六个入口均通过（退出码 0）；指定三个测试文件完整运行：
`job_observations_test.ts` 7 passed、`continuity_test.ts` 12 passed、
`agents_wake_test.ts` 2 passed；合计 `ok | 21 passed | 0 failed (4s)`（退出码 0）。
完整原始日志保存在仓库外的 `/var/lib/eve-workspace/tmp/pending-job-review-verification/deno-check.log`
和 `tests.log`。测试使用本地 mock 和 `/tmp` 临时日志，不接触在线任务服务。

对照完成后通过 `git worktree remove /var/lib/eve-workspace/tmp/pending-job-baseline`
清理临时基线 worktree；未提交、推送或部署。
