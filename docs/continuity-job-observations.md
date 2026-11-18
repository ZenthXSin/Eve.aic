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

### 准入身份与脚本报告（P2-A / P2-B）

**AgentJob 外形不等于来源证明。** 工具外层的服务端准入回执（agents.send/delegate_task）
和 agents/scripts notice 可记录首次完整的 `{agentId, requestSeq}` 身份绑定：agentId 是非空字符串，
requestSeq 是正安全整数。较早回执缺字段时，后续完整服务端回执可补建绑定；已有绑定不被冲突
身份覆盖。服务端状态回执仍按原状态机消费，身份绑定不能由用户脚本建立或替换。

`script_result` 只接受完整第一页的合法 AgentJob 值，而且必须与已保存准入身份的
agentId、requestSeq **完全一致**，才进入正常状态机。没有绑定、没有已跟踪任务或身份不匹配，
不推进、不退休、不创建任务；缺字段、非整数身份等不完整形状直接忽略。
真实 70268→70488 的两个身份一致（requestSeq=12152），所以 pendingJobs 仍是
running / script_result / record 70488，`scriptReports=[]`。

完整合法形状但未通过绑定核对的值单列为 `scriptReports`：jobId、reportedStatus、
observation.record/recordedAt/source。最多保留最近五个 jobId 的最后报告，重复报告按保存顺序移动；
报告可与后续合法观察并存，**只作脚本自述、未经服务端证明的线索，不据此退休或更新 pendingJobs**。
它不会进入 unknownJobCount，不触发执行/唤醒/查询；重启按活动原件重建相同报告列表。
展示 map 携带报告列表，因此已有 runtime 的 observe 和 continuityMessage 接线无需修改。

身份匹配只是本次要求的关联校验，不是密码学来源认证：脚本若复制正确身份，仍可自述合法状态。
这里不额外推断 actions 数量或服务端实时性，也不把零动作、AgentJob 外形当作来源证明。
后续若要更强证明，应在服务端回执契约中携带可验证来源；本次没有新增查询来补证。

P2-B 只增加 checking→迟到 queued 的拒绝分支，沿用 running 防回退写法；被拒后状态及
observation.record/recordedAt 均不变。终态/unknown 保护保持原契约，unknown→unknown
也不刷新冻结观察；没有扩大状态机。

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

### 本次 P2-A/P2-B：伪造身份与 checking 回退对照

比较对象为父提交 `b6ca57719434c6ad5db7bd023b89916f026921c7` 的编辑前副本和当前修复，
副本只保存在仓库外；没有切换分支、修改基线或创建新 worktree。
一次性脚本 `/var/lib/eve-workspace/tmp/pending-job-binding-comparison.ts` 从同一真实 fixture
读取准入身份、脚本信封和 AgentJob，再合成两类离线输入：
1. 零动作脚本自述错误 agentId=`forged_agent`、requestSeq=1，分别报告 running/completed/unknown；
2. 已知 checking 的任务收到迟到 queued。
71000、71300、71301 及其时刻均为合成测试事件，不是历史 journal 原件。

真实命令（相同工作目录与只读权限，不实例化 runtime、不调用服务）：

```bash
/tmp/eve-agent-recovery-tools/deno run --no-config --no-lock --allow-read=/var/lib/eve-workspace/tmp/pending-job-binding-verification,/var/lib/eve-workspace/projects/eve-pending-job-observation /var/lib/eve-workspace/tmp/pending-job-binding-comparison.ts before | tee /var/lib/eve-workspace/tmp/pending-job-binding-verification/before.jsonl
/tmp/eve-agent-recovery-tools/deno run --no-config --no-lock --allow-read=/var/lib/eve-workspace/tmp/pending-job-binding-verification,/var/lib/eve-workspace/projects/eve-pending-job-observation /var/lib/eve-workspace/tmp/pending-job-binding-comparison.ts after | tee /var/lib/eve-workspace/tmp/pending-job-binding-verification/after.jsonl
```

修复前真实输出（退出码 0；旧版没有 scriptReports，比较脚本仅将缺失字段打印为 []）：

```jsonl
{"case":"forged_identity","reportedStatus":"running","pendingJobs":[{"jobId":"eve_70266_0_task","status":"running","observation":{"record":71000,"recordedAt":"2026-11-18T12:01:11.000Z","source":"script_result"}}],"scriptReports":[]}
{"case":"forged_identity","reportedStatus":"completed","pendingJobs":[],"scriptReports":[]}
{"case":"forged_identity","reportedStatus":"unknown","pendingJobs":[],"scriptReports":[]}
{"case":"checking_then_queued","pendingJobs":[{"jobId":"eve_70266_0_task","status":"queued","observation":{"record":71301,"recordedAt":"2026-11-18T12:01:11.301Z","source":"tool_receipt"}}],"scriptReports":[]}
{"case":"fixture_70268_70488","pendingJobs":[{"jobId":"eve_70266_0_task","status":"running","observation":{"record":70488,"recordedAt":"2026-11-18T11:41:52.987Z","source":"script_result"}}],"scriptReports":[]}
```

修复后真实输出（退出码 0）：

```jsonl
{"case":"forged_identity","reportedStatus":"running","pendingJobs":[{"jobId":"eve_70266_0_task","status":"queued","observation":{"record":70268,"recordedAt":"2026-11-18T11:01:04.508Z","source":"tool_receipt"}}],"scriptReports":[{"jobId":"eve_70266_0_task","reportedStatus":"running","observation":{"record":71000,"recordedAt":"2026-11-18T12:01:11.000Z","source":"script_result"}}]}
{"case":"forged_identity","reportedStatus":"completed","pendingJobs":[{"jobId":"eve_70266_0_task","status":"queued","observation":{"record":70268,"recordedAt":"2026-11-18T11:01:04.508Z","source":"tool_receipt"}}],"scriptReports":[{"jobId":"eve_70266_0_task","reportedStatus":"completed","observation":{"record":71000,"recordedAt":"2026-11-18T12:01:11.000Z","source":"script_result"}}]}
{"case":"forged_identity","reportedStatus":"unknown","pendingJobs":[{"jobId":"eve_70266_0_task","status":"queued","observation":{"record":70268,"recordedAt":"2026-11-18T11:01:04.508Z","source":"tool_receipt"}}],"scriptReports":[{"jobId":"eve_70266_0_task","reportedStatus":"unknown","observation":{"record":71000,"recordedAt":"2026-11-18T12:01:11.000Z","source":"script_result"}}]}
{"case":"checking_then_queued","pendingJobs":[{"jobId":"eve_70266_0_task","status":"checking","observation":{"record":71300,"recordedAt":"2026-11-18T12:01:11.300Z","source":"tool_receipt"}}],"scriptReports":[]}
{"case":"fixture_70268_70488","pendingJobs":[{"jobId":"eve_70266_0_task","status":"running","observation":{"record":70488,"recordedAt":"2026-11-18T11:41:52.987Z","source":"script_result"}}],"scriptReports":[]}
```

可见伪造身份不再推进/退休任务，只产生带 reportedStatus 的脚本报告；checking 保留
record 71300，迟到 queued 不刷新。fixture 的 pendingJobs 在前后两份输出相同，且修复后的
scriptReports 真正为空。

原 70268→70488 的 current 命令也重新运行，并实际比较原输出：

```bash
/tmp/eve-agent-recovery-tools/deno run --no-config --no-lock --allow-read=/var/lib/eve-workspace/tmp/pending-job-baseline,/var/lib/eve-workspace/projects/eve-pending-job-observation /var/lib/eve-workspace/tmp/pending-job-comparison.ts current | tee /var/lib/eve-workspace/tmp/pending-job-binding-verification/current-output.json
cmp /var/lib/eve-workspace/tmp/pending-job-verification/current-output.json /var/lib/eve-workspace/tmp/pending-job-binding-verification/current-output.json && echo 'Original pendingJobs fixture output byte-identical.'
```

cmp 退出码 0，实际输出 `Original pendingJobs fixture output byte-identical.`。
新增 scriptReports/guidance 扩展完整生活接续消息，但没有改变原 pendingJobs JSON。

## 前轮验证（b6ca577）

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
清理临时基线 worktree。前轮证据已收录于 b6ca577；下面是本次同分支续修复，未新建 worktree。

## 本次同分支修复验证

原 21 项全部保持通过，新增六项专项测试。既有专项测试的 agentJob.agentId 仅改为从真实
70268 fixture 提取；额外授权的 `tests/agents_wake_test.ts` 仅将 queue() 的准入输出改为
从 `receipts[0].data.result.output` 提取的真实身份加上 queue(id) 参数 jobId 与 queued 状态，
不动该文件任何断言（含 running）、执行或唤醒逻辑。两个 jobId 分别跟踪；
eve_restore_task 的 failed 之后迟到 queued 仍指向 eve_restore_task，不误用 fixture jobId。
`plugins/eve/runtime.ts`、两条原 fixture 均未修改。

真实命令：

```bash
NO_COLOR=1 /tmp/eve-agent-recovery-tools/deno test --frozen --allow-read=/var/lib/eve-workspace/projects/eve-pending-job-observation,/tmp --allow-write=/tmp tests/job_observations_test.ts tests/continuity_test.ts tests/agents_wake_test.ts
NO_COLOR=1 /tmp/eve-agent-recovery-tools/deno check --frozen plugins/eve/continuity.ts plugins/eve/runtime.ts plugins/eve/job-observations.ts tests/job_observations_test.ts tests/continuity_test.ts tests/agents_wake_test.ts
NO_COLOR=1 /tmp/eve-agent-recovery-tools/deno test --frozen --filter='/script identity|checking rejects|four ingress/' tests/job_observations_test.ts
```

完整三组结果：`job_observations_test.ts` 13 passed、`continuity_test.ts` 12 passed、
`agents_wake_test.ts` 2 passed；`ok | 27 passed | 0 failed (5s)`，退出码 0。
六入口 deno check 均通过，退出码 0。

专项真实输出：

```text
Check tests/job_observations_test.ts
running 3 tests from ./tests/job_observations_test.ts
script identity mismatches cannot advance or retire and are only unverified reports ... ok (90ms)
checking rejects late queued through all four ingress paths and replay ... ok (3ms)
four ingress × terminal/unknown protections keep live and restart replay identical ... ok (352ms)

ok | 3 passed | 0 failed | 10 filtered out (488ms)

```

新增测试覆盖错误身份、缺字段及非整数；无绑定的完整报告不能建立身份或任务；
三个服务端入口均可建立绑定而不能冲突覆盖；checking 拒绝四入口的 queued；
四入口 × completed/failed/cancelled/unknown 退出保护与全部迟到非终态入口矩阵，
每一步重放同一事件顺序与实时消费结果一致（包括观察和报告）；并核对报告只保留最近五项。
完整日志在 `/var/lib/eve-workspace/tmp/pending-job-binding-verification/tests.log`、`deno-check.log`。
未新增 agents.status/list/子任务查询，未推送、部署或写运行目录。

### Amend 复核：queue(id) 保留任务标识

queue(id) 不再无条件复用 fixture 的 jobId：只复用其真实 agentId/requestSeq，jobId 用参数覆盖，
status 明确为 queued。eve_restore_task 的准入与 failed 后迟到回执因此都指向同一个任务，
现有空 pendingJobs 断言重新覆盖该任务的终态防复活；未改任何 agents_wake 断言或执行/唤醒逻辑。

自查现有 `four ingress × terminal/unknown protections keep live and restart replay identical`：
四个 factory 实际分别产生工具外层、完整脚本返回、agents notice 和 scripts notice；
每入口对 completed/failed/cancelled/unknown 均断言实际状态、record 71401、observation.source
等于入口名，再对四个迟到入口逐项断言整个任务观察不变、pendingJobs 与 scriptReports 为空。
每次用新的 JobObservations 重放原事件顺序，比较 jobs、pendingJobs、scriptReports；不是仅经过
名义入口。`checking rejects late queued through all four ingress paths and replay` 对四入口均断言
checking、record 71300 和整个 pendingJobs 不变，以及新索引重放一致。上述专项无需修改。

Amend 后重新运行同样的三个 --frozen 测试文件和六入口 check：13 + 12 + 2 passed，
`ok | 27 passed | 0 failed (6s)`，六入口 check 退出码 0。
两项矩阵额外单独重跑：`ok | 2 passed | 0 failed | 11 filtered out (593ms)`。
最新完整日志在 `/var/lib/eve-workspace/tmp/pending-job-amend-verification/` 的
`tests.log`、`deno-check.log`、`matrix-tests.log`；仍未推送、部署或修改运行目录。
