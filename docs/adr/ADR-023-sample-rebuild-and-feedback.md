# ADR-023：样本记录、可重建性与 Artifact 摘要分离

- 状态：已采用
- 日期：2026-10-26
- 依据：P0 执行方案 v1.1 §4.12；V0.3 双平面与事件只存引用；ADR-020 第 3/4/6/7 条；boundaries.mdc

## 背景

A4a 已经能用 `flux trace` 按轮次读出 `request_ref` / `response_ref` 与工具结果引用。方案 §6 还要求「完整请求可从 blob 重建」，并把一次 Run 落成可长期保存的样本；同时 2026-10-26 真实 Run 的 `artifact.created` 事件里塞进了整份报告正文，违背「事件只存引用与摘要」。

离线回放（用记下的响应驱动 Engine 重跑）不在 P0a。A4b 只保证回放以后需要的输入：可校验的消息链、工作区版本、人工总评。

## 决定

1. **样本记录是派生导出物，不是 durable fact。** 不进 `events`；同一 Run 可重复 `flux sample export`，除 `exported_at` 外结果相同。字段契约在 `agent-contract::Sample` 与 `contracts/schemas/sample.schema.json`。只含 `*_ref` 与摘要，不含 prompt、工具正文、`reasoning_content`、报告正文。

2. **可重建性校验放在 `engine-custom::rebuild`。** 输入是已读出的 JSON / 字节，不依赖 `run-host`。规则 1–7 见执行方案 §4.12.1；结论三态 `consistent` / `inconsistent` / `unverifiable`。CLI：`flux trace --verify`；`--json` 时结果在 `rebuild` 字段。

3. **`workspace_revision` 写入 `attempt.started` 的可选字段。** Host 在 `prepare_drive` 打开并核对工作区之后、开启写事务之前采集；直接执行 `git`，不经 shell，环境变量 `GIT_OPTIONAL_LOCKS=0`，单命令 5 秒超时；`-- .` pathspec 限定脏检查在工作区子树。失败记 `{ "vcs": "unknown" }`，不使 Run 失败。旧事件缺字段反序列化为 `None`。

4. **人工反馈只追加。** 迁移 `0003_sample_feedback.sql`；`RunHost::add_feedback`；不写 event、不改 `last_sequence`。`verdict` 为 `usable` / `needs_edit` / `redo`；`note` 可选、限 2000 字符。

5. **`ArtifactDraft` 拆成 `summary` 与 `body`。** Host 只把 `body` 写进 blob；`summary` 由 Engine 取正文首个非空行并截到 200 字符，只进事件与 Artifact 索引。这是 `engine-api` 边界变更。

6. **`doc_version` 小表随 ADR-020 维护。** 别名 → 文档版本与核对日期；初始仅 `deepseek-flash → DeepSeek-V4.1-Flash-1017（2026-10-21）`。未知别名导出为 `unknown`。费用仍离线算，不进样本。

7. **工具事件保留调用身份。** Engine 的 `ToolCall` 观测必须携带非空 `tool_call_id`；Host 将其写入 `tool.call_completed`，时间线与样本逐项保留。事件/导出契约中的字段为可选，以便继续读取旧事件；校验不推断缺失的 ID，而返回 `unverifiable missing=tool_call_id#k:j`。规则 4/6 同时核对事件 ID、工具名和顺序，相同工具名也不能跳过或交换调用。历史 Run 如含无 ID 的工具事件，不再声称已证明一致；§4.12 原有真实 Run 的 `consistent` 验收改由新 Run 完成。

8. **结构损坏不能降级成一致。** 请求/响应解析为 JSON 后还需核对必需字段、类型和每一项工具调用；不接受把坏条目过滤后的剩余列表。结构损坏归 `unverifiable`，结构合法但消息链或事件不相等归 `inconsistent`。兼容只有 `finish_reason` / `usage` 的已记录失败响应。CLI 默认展示坏 JSON 时只输出 `unavailable`；显式 `--include-reasoning` 才允许展示原文。git 采集等待进程时并发读取 stdout，读取与等待共用单命令五秒期限。

## 后果

- Host 多一条 git 子进程路径，但仍不是 Agent 工具；模型不能触发它。
- `flux sample export` 依赖 blob 可读性；缺 blob 时 `rebuild` 为 `unverifiable`，样本仍可导出。
- 已有长 `summary` 事件不改写；新报告不再把全文放进事件。

## 复审触发器

- 需要离线回放或自包含样本包（随记录搬走 blob）。
- 第二套 VCS 或跨机可复现的工作区快照。
- 反馈需要结构化分项或评分制，而不只是三档总评。
- DeepSeek 别名改指或文档版本字段形状变化（与 ADR-020 同次更新 `doc_version` 表）。
