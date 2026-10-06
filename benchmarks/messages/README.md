# 消息关系公开样本评估

`cases.json` 是 30 例人工标注的小型回归集，覆盖中文修订、补充、多意图、否定/引用/假设中的取消词、真实取消、有效与缺失的澄清引用、工具状态、UTF-8 原文范围和明确命令。标注是可审查的预期，不是模型真实质量证明，也不代表线上用户分布。

从仓库根目录离线运行规则基线：

```bash
cargo run -p eve-app --bin eve-message-evaluate -- --mode rules --output /tmp/eve-message-rules.json
```

省略 `--mode` 同样使用规则，不读取模型密钥或访问模型服务。省略 `--output` 则将完整 JSON 写到 stdout。报告文件必须尚不存在；样本最多 1 MiB、128 例，报告最多 4 MiB。每次默认使用全新随机临时配置目录，正常停止后删除；`--state-dir` 只能指定尚不存在的新目录，父目录须已存在，完成后保留配置。已有目录一律拒绝，不能指向 QQ 正式状态；不读取会话数据库。停止失败会保留本次评估目录，不自动清空故障状态。

真实模型入口需要用户显式选择，运行会向所选服务发送公开样本中的任务和消息文字。主模型模式复用 QQ 的“明确规则 → 当前主模型”，因此它不是绕过规则的纯模型基线：

```bash
# 在当前 shell 安全地预先设置 EVE_OPENAI_API_KEY；不要把密钥放入命令参数或 JSON。
cargo run -p eve-app --bin eve-message-evaluate -- --mode primary --output /tmp/eve-message-primary.json
```

Jev 模式复用“明确规则 → Jev → 当前主模型”，需要独立 `EVE_JEV_API_KEY`，同时需要主模型密钥以供回退。非敏感配置沿用 [Jev 宿主接线说明](../../docs/Jev判断适配.md)：

```bash
export EVE_MODELS_JEV_ENABLED=true
export EVE_MODELS_JEV_PROVIDER=jev
export EVE_MODELS_JEV_MODEL=jev-latest
export EVE_MODELS_JEV_TIMEOUT_MS=1000
export EVE_MESSAGE_JUDGE_TIMEOUT_MS=4000
cargo run -p eve-app --bin eve-message-evaluate -- --mode jev --output /tmp/eve-message-jev.json
```

以上模型命令只是可显式执行的入口，本变更不默认执行真实收费评估，不自动开启 QQ 自然判断，也不将离线协议通过描述为 Jev 或主模型的质量结论。报告记录 `started_at_unix_ms` 和不含密钥的 `configuration_sha256`。比较时使用同一个 `dataset_sha256`、相同消息期限和置信度阈值，并另外记录实际模型配置；配置指纹可识别同一模式的参数变化，各模式的配置集合不同，不要求跨模式指纹一致。不同供应商或不同参数的结果不可直接归因于模式本身。端点只参与配置指纹，不以明文写入报告；密钥和凭据引用不参与指纹。

每例包含标准 `RelationInput`、唯一 `id`、中文标注意图说明，以及 `accepted` 候选集合。每个候选是完整的预期标签和原文范围组合，允许少量合理歧义。范围是原消息的 UTF-8 字节偏移 `[start,end)`，不是字符索引；修订、补充、新任务和回答必须有非空范围，其他标签无范围。候选内部不得重叠；合法 `answer` 必须具有当前澄清和匹配回复引用。未知字段、重复 JSON 字段、重复 ID、非法标注和版本均拒绝，在任何模型请求之前完成验证。

报告的计分约定：

- `exact_match`：标签及对应字节范围匹配同一个完整候选，忽略 parts 顺序。
- `label_match`：标签多重集合匹配一个完整候选；`raw_span_match` 只比较范围多重集合。没有文字范围的不同标签可能范围匹配，因此不能将此指标单独作为正确率。
- `false_cancellation`：预测出现 `cancel`，而全部可接受候选都没有 `cancel`。这是判断输出的误取消风险，不表示真实任务已取消；低置信度的取消预测也会计入。
- `correction_required`：全部可接受候选都要求修订；此时未输出达到当前阈值的 `correction`，包括超时和错误，都计为 `missed_correction`。
- `low_confidence_or_ambiguous`：成功结果含低于阈值的标签或含混标签。规则对自然文字保守输出含混，因此预期会在该集合上漏掉许多自然修订；这是真实运行的规则基线，不应调整标注来迎合规则。
- `latency_ms`、`timeouts` 和 `errors`：记录本次实际调用及验证的结果；错误仅固定类别。报告不包含模型解释或原始 Provider 诊断，`actual.raw_text` 只来自通过范围验证的公开样本原文。

运行器只装配 ConfigPlugin 和 RelationService，不注册消息路由、Control、QQ 或工具。它没有执行任务，因此不报告任务完成率。当前公开接口也无法精确区分一次成功判断经过了多少个请求、是否发生回退，故不输出请求数或回退率；不得从标签结果或耗时推算。需要这些指标时应先为公开契约增加准确观测能力，再使用同一标注集评估。
