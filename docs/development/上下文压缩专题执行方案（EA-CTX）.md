# EA-CTX：上下文压缩专题执行方案

> 日期：2026-10-30　源码基线：`main@0a29a86`　架构决策：[ADR-026](../adr/ADR-026-context-module.md)　设计依据：[专题方案 v0.1](../research/上下文压缩专题方案（EA-CTX）v0.1.md)
> 性质：专题执行计划。EA-02 只交付本专题 P1；P2 起在专题内迭代。
> 原则：上下文压缩是可独立迭代的模块，尽量不与现有代码耦合，保留以后引入其他技术的余地。

## 1. 目标与非目标

主目标：上下文变长时，模型的下一步决策不因历史膨胀而变差。容量与成本只需守住边界。

1. **容量**：任何模型、任何预算下，请求都不会因超出窗口被服务端拒绝；超出时以 `context_exhausted` 结束，不发请求。
2. **质量**：压缩前后，模型在同一位置做出相同决策的比例可测量，并达到约定门槛（§7）。
3. **成本可预期**：每次编辑历史的缓存代价可计算、被记录，不出现频繁的缓存失效。
4. **可重建**：压缩后的每次请求仍能从事件与 blob 重建，`trace --verify` 能区分合法编辑与不一致。
5. **多模型**：接入新模型只加档案文件，策略和代码不改。

**非目标（v1 不做）**：LLM 摘要；模型主导的折叠（AgentFold、MEM1 一类）；厂商不透明的服务端压缩；跨任务记忆（M 线）。它们以后都以策略形式接入（ADR-026 第 10 条），按证据启用。

## 2. 原理摘要

- **推理层已在压缩。** DeepSeek V4 公开版的注意力分三档：128 token 原始窗口、每 4 token 压一条并按查询选 top-512、每 128 token 压一条近乎全量参与。Agent 层不重做注意力，只做推理层做不到的事：判断哪份观察已作废、把内容移出上下文并留地址、按计费结构安排编辑。
- **四种成本**：容量（守硬上限）、钱（只算清楚，不作为动机）、注意力（唯一主动优化的目标）、协议（限定可编辑内容）。
- **编辑历史的回本条件**：当前 prompt 为 P，从深度 d 删去 S，之后还剩 R 轮，则需 `R > (cache_ratio − 1) × (P − S − d) / S`。DeepSeek Flash 的 `cache_ratio` 为 50，从 100K 删 50K 约需 47 轮回本。编辑点越靠近末尾越便宜，摄入时为零。
- **组织原则**：只在前沿编辑；压缩尽量在摄入时做；先清作废、再清陈旧；可取回的观察可移走，模型自己的动作、推理与失败不动；移走的内容留地址；模型参与必须成为可重建的事实。
- **质量定义**：决策等价，即压缩后的上下文让模型做出与完整上下文相同下一步的程度（§7）。

## 3. 模块与接口

见 ADR-026 第 1、3、10、11 条。要点：

- `crates/context`（`fluxaxiom-context`）只依赖 `agent-contract` 基础类型。
- 对外只有 `ContextManager::prepare` 与 `ContextManager::replay`；每个策略实现 `id`、`apply`、`replay`。
- 策略链按顺序执行，降到目标即停；链跑完仍超硬上限才返回 `ContextExhausted`。
- P1 的链为空（恒等策略 `identity@1`）；v1 的链为 `mask@1`（L0 → L1 → L2）。

## 4. 模型档案 v1

文件：`crates/context/profiles/<provider>-<alias>.toml`，编译时嵌入。未知字段取保守值并记录所取的值。

| 组 | 字段 | 含义 | 推出什么 | deepseek-flash 当前值 | 可信度 |
| --- | --- | --- | --- | --- | --- |
| 身份 | `provider` / `alias` / `doc_version` | 服务商、别名、文档所列版本 | 样本分组 | deepseek / deepseek-flash / V4.1-Flash-1017 | 文档 |
| 容量 | `context_window` | 窗口 token 数 | 硬上限起点 | 1,048,576 | 文档 |
| 容量 | `window_counts_output` | 窗口是否包含输出 | 是否扣输出预留 | 假设“是” | 未知 |
| 容量 | `max_output` | 单次输出上限 | 输出预留上界 | 393,216 | 文档 |
| 容量 | `reasoning_in_output` | 思维链是否计入输出上限 | 预留是否另算 | 假设“是” | 未知 |
| 测量 | `tokenizer` | 可否离线精确计数 | 估算或精确 | 官方提供离线包，未接入 | 文档 |
| 测量 | `token_ratio` | 每字符 token，英文与中文分开 | 新增内容估算 | 英文 0.3，中文 0.6 | 文档 |
| 测量 | `usage_fields` | 响应可靠返回的用量项 | 能否以服务端为锚 | prompt / completion / 缓存命中与未命中 | 实测 |
| 测量 | `estimate_error` | 估算与实际的偏差（p99） | 硬上限余量 | 暂取 10% | 未知 |
| 经济 | `price_*` | 未命中、命中、输出三种单价 | 绝对成本 | 见官方价格页 | 文档 |
| 经济 | `cache_ratio` | 未命中价 ÷ 命中价 | 编辑回本条件 | 50 | 文档 |
| 经济 | `cache_mode` | 自动前缀缓存或显式断点 | 能否控制断点 | 推测为自动前缀 | 待核 |
| 经济 | `cache_granularity` | 命中的块大小 | 编辑点是否对齐块 | — | 未知 |
| 经济 | `cache_ttl` | 缓存保留时长 | 何时编辑“免费” | — | 未知 |
| 更新语义 | `system_update` | 能否追加 system 而不改首条 | 中途改提示不破缓存 | DeepSeek Harness 在 Messages 接口声明支持；Chat Completions 未验证 | 未知 |
| 更新语义 | `tool_update` | 能否追加工具声明而不改首部列表 | 中途加工具不破缓存 | 同上 | 未知 |
| 注意力 | `effective_context` | 指定任务类型上质量开始下降的长度 | 软触发 | — | 未知 |
| 注意力 | `context_awareness` | 模型是否感知并响应自身窗口 | 是否告知剩余空间 | — | 未知 |
| 协议 | `reasoning_passback` | 思维链回传规则 | 思维链是否可清 | 带 tools 时工具循环内必须回传；新用户消息到来时才丢弃 | 文档 |
| 协议 | `tool_pairing` | 调用与结果配对、结果可否换占位 | 遮蔽的合法形式 | 必须配对；占位内容可接受 | 假设 |
| 协议 | `native_context_mgmt` | 厂商原生压缩及其透明度 | 能否使用、能否重建 | Chat Completions 上未知有 | 假设 |

`effective_context` 必须注明测量所用的任务集。`doc_version` 小表（ADR-023 第 6 条）暂不合并，样本导出逻辑不变。

## 5. 策略 v1（`mask@1`）

全部为确定性规则，不调模型。只有 L0 每次调用都运行；L1、L2 只在触发后批量执行。

- **L0 前向去重**：新工具结果与本 Run 早先某个结果完全相同（同工具、同规范化参数、同字节 hash）时，新消息内容换成指针存根，如 `[unchanged: same result as round #3 call 2]`。只改新消息，不动历史。工具输出的截断仍由工具负责（EA-03），不在本模块。
- **L1 作废遮蔽**（超过软触发后）：同一文件后来再次读取，且后读范围覆盖先读范围，则先读结果换成存根，如 `[superseded: read_file "src/x.rs" L1-200, 6.1 KiB, sha256:…; see round #7]`。
- **L2 陈旧遮蔽**（L1 不足以降到目标时）：从最旧开始，遮蔽保护窗口之外的工具结果。存根写明工具名、参数、大小、hash 与“需要时重新调用该工具”。
- **永不遮蔽**：system、user、所有 assistant 消息（`content`、`reasoning_content`、`tool_calls`）。
- **L3 硬上限**（每次调用前，由管理器检查）：链跑完仍超硬上限，不发请求，返回 `ContextExhausted`。
- **溢出兜底**：服务端报告上下文超限时归类为上下文超限，强制执行一次链（忽略软触发，只保护最近 1 轮）。只有请求确实变化才重试，最多 1 次；否则以 `context_exhausted` 结束。

**派生公式**

| 派生值 | 计算 | v1 取值 |
| --- | --- | --- |
| 输出预留 | 请求的 `max_tokens`（窗口不含输出时为 0） | 当前 32,768 |
| 硬上限 | (窗口 − 输出预留) × (1 − `estimate_error`) | 约 914K |
| 当前估算 | 上一轮服务端 prompt + completion + 新增内容按 `token_ratio` 估算；无 usage 时全量估算 | 记录估算来源 |
| 软触发 | min(硬上限, `effective_context` × 0.8) | 未测前暂定 128K，标“未校准” |
| 目标 | 软触发 × 0.5 | 暂定 64K |
| 最少腾出 | max(32K, 0.25 × 当前估算)，达不到则不编辑（超硬上限除外） | 一次 Run 只触发一两次缓存失效 |
| 保护窗口 | 最近 K = 2 个模型轮的工具结果 ∪ 协议保护集 | 每条工具结果至少以原文出现一次 |
| 冷缓存窗口 | 距上次响应超过 `cache_ttl` 时允许在软触发以下执行 L1 | `cache_ttl` 测出前关闭 |

以上数值都是 `mask@1` 的配置；任何一项变化都递增 `context_strategy`。

**`mask@1` 的 `replay` 检查**：被编辑的是 `tool` 消息且不在保护窗口内；存根文本等于由记录渲染出的文本；记录中的 `result_ref` 与原消息内容一致；遮蔽单调（遮过的消息之后保持同一存根）；有编辑记录必须有请求变化。

**上下文耗尽与产品式压缩的区别**：Cursor、Claude Code 提示“压缩”后继续对话，对应的是 v1 没有的 LLM 摘要层。v1 的耗尽是 L0–L2 用尽仍超硬上限的时刻。L2 之后剩下的主要是 assistant 消息里的思维链：每轮最多 32,768 token，1M 窗口约容纳 28 轮满额思考，默认 8 次调用到不了。以后需要继续运行时，按代价依次加入策略：清旧轮思维链（依赖 T7）、复用前缀的摘要、交接到新分段。

## 6. 可重建性

见 ADR-026 第 4–9 条。执行方案 §4.12.1 的规则 1、3 并入 `req_k.messages == serialize(replay(T_k, R_k))`，规则 2、4、5、6、7 不变。旧事件按恒等策略校验。

## 7. 评测：决策等价

1. 从真实 Run 的请求 blob 取第 k 次请求，构造完整版与按某策略压缩后的版本。
2. 同一模型、同一参数，各采样 N 次（初定 5）。
3. 比较下一步：工具调用集合（工具名 + 规范化参数）、是否结束、结论引用的路径。
4. **基线**是完整版对完整版的一致率；压缩损失 = 基线一致率 − 压缩版一致率。

**失败挖掘**（参考 ACON）：收集“完整版正确、压缩版不同”的样本，人工归因丢了什么，据此修订策略并递增 `context_strategy`。

| 指标 | 来源 | 用途 |
| --- | --- | --- |
| 决策等价率（相对基线） | 离线回放 | 主指标 |
| 重复调用率 | `trace` 同轮重复调用 | 存根是否诱发重读 |
| 任务事实正确率 | EA-04 固定任务集 | 端到端质量 |
| 缓存命中率、单次 Run 成本 | 事件 usage | 编辑的实际代价 |
| 编辑次数、腾出 token | context 记录 `edits[]` | 是否低频批量 |

前提：现有真实任务约 6 轮、1.2 万 token，触发不了压缩。EA-04 任务集中需单列长任务（更大仓库、更高调用预算）。

工具：`flux ctx replay <run> --call k --strategy <id> --samples N`（暂名）。读 blob 构造请求、调用模型、输出比较结果；属评测工具，不写 Run 事实；默认测试用 Fake。

## 8. 实测计划

| 编号 | 测什么 | 方法 | 成本 | 解锁 |
| --- | --- | --- | --- | --- |
| T1 | 上下文构成与作废比例 | 对已有样本 blob 离线统计各类消息占比、同文件重读与完全重复结果的比例 | 无 API 调用 | L0/L1 是否值得做 |
| T2 | `estimate_error` | 每轮估算值对比下一轮服务端 prompt 用量，分中英文统计 | 无 API 调用 | 硬上限余量 |
| T3 | `cache_ttl`、`cache_granularity`、`cache_mode` | 固定前缀间隔 1/5/15/60 分钟重发；不同前缀长度截断；读缓存命中字段 | 少量调用 | 编辑成本模型、冷缓存窗口 |
| T4 | `window_counts_output`、`reasoning_in_output` | 先查文档；不足时在接近上限处少量探测 | 每次近 1M token | 输出预留 |
| T5 | `system_update`、`tool_update` | Chat Completions 上中途追加 system 或工具声明，看是否遵循、缓存是否命中 | 少量调用 | 中途改提示是否破缓存（优先级低） |
| T6 | `effective_context` | 固定任务上用无关内容与相似干扰内容分别填充到 16K/64K/256K/512K，测决策等价率曲线 | 最贵 | 软触发校准 |
| T7 | 旧轮 `reasoning_content` 置空 | 真实 API 验证可接受性，再测决策等价 | 少量调用 | 是否允许清思维链 |

每项产出一份实测记录（方法、样本、结果、核对日期），档案字段的来源指向它。记录放 `docs/research/context/`。

## 9. 分期、PR 切片与验收

| 阶段 | 内容 | 进入下一阶段的门槛 |
| --- | --- | --- |
| P0 | ADR-026；修订 ADR-020 第 6 条、ADR-023、执行方案 §4.12.1 与 §6 | 方案与 ADR 定稿（本次） |
| P1（= EA-02） | 独立模块（恒等策略）、档案 v1、估算器、硬上限、溢出归类、context 记录、六个接触点 | 现有样本校验仍为 `consistent`，估算误差有报告 |
| P2 | T1–T3、T7；`flux ctx replay`；自一致基线 | 档案关键字段有来源，基线一致率已测 |
| P3 | `mask@1` 加入策略链 | 决策等价损失不超过门槛（数值待 P2 后定） |
| P4 | T6；软触发改由 `effective_context` 派生；对比最旧优先与作废优先 | — |
| P5 | 按证据加入新策略：清旧轮思维链、复用前缀的摘要、折叠；第二个模型档案 | 每项需独立证据 |

**P1 PR 切片**

1. **P1-1 `context` crate**：`Transcript`、`View`、`ContextStrategy`、`ContextManager`、`identity@1`、档案加载与 `deepseek-flash` 档案、估算器、`ContextSpec` 派生、context 记录 schema。纯函数与单元测试；依赖检查确认不依赖 `engine-api` / `engine-custom` / `run-host`。
2. **P1-2 契约**：`FailureReason::ContextExhausted`、`TerminalReason::ContextExhausted`；`ModelCall` 观测与 `model.call_completed` 的 `context_ref`、`context_strategy`；`event.schema.json` 与样例；旧事件反序列化测试；`run-host` 落库与终态映射。
3. **P1-3 Engine A 接入**：每次调用前 `prepare`，视图序列化为 Chat Completions，写记录 blob 并随观测提交；硬上限返回 `ContextExhausted`（可带部分报告）；服务端超限错误归类（识别规则待真实错误样本，未识别仍为 `ProviderError`）。更新 `p0a-runtime.mdc` 的终态说明。
4. **P1-4 校验与展示**：`rebuild` 改为经 `replay` 逐字节比较；`trace` 显示 `context_strategy` 与每轮估算值、硬上限；`sample export` 保留两个字段。

**各阶段验收**

- **P1**：上下文模块以恒等策略接入 Engine A，现有全部样本的请求逐字节不变；依赖检查通过；`trace --verify` 仍为 `consistent`；硬上限与溢出归类有单元测试（构造超限请求、构造服务端超限响应）；旧事件可读；新事件与样本带 context 记录与两个字段；`trace` 显示每轮估算值与上限。
- **P2**：T1–T3、T7 各有实测记录，档案对应字段指向它；`flux ctx replay` 能在至少一组样本上输出自一致率。
- **P3**：遮蔽后的 Run `trace --verify` 为 `consistent`；篡改存根、遮蔽保护窗口内消息、未记录的删除、有记录但请求未变，均报 `inconsistent rule=1`；长任务集上决策等价损失不超过门槛；记录触发前后的缓存命中率。
- **P4**：`effective_context` 有测量记录与任务集说明；软触发由它派生；两种遮蔽顺序的对比入档。

**与其他开发包的关系**：EA-03 的大文件分页与工具输出设计是 L0 的上游；EA-04 提供固定任务集，P2 起需单列长任务。

## 10. 多模型接入

1. 按文档填写档案文件，未知字段取保守值并标注。
2. 跑 T2（估算误差）与 T3（缓存规则）。
3. 硬上限、软触发等由档案与现有策略算出；`effective_context` 未测时沿用暂定值并标“未校准”。
4. 样本记录 `profile_version`，不同档案的样本不混合统计（延续 ADR-020 第 3 条）。
5. 协议字段不同（思维链回传、配对规则、原生压缩）时，由档案给出可编辑集合；策略只读这个集合。

厂商原生压缩只作为可选策略；启用后对应调用的校验结论为 `unverifiable`，并在样本中标注。

## 11. 风险

| 风险 | 表现 | 应对 |
| --- | --- | --- |
| 存根在训练分布之外 | 模型反复重读被遮文件，或把存根当内容 | 监控重复调用率；存根措辞纳入失败挖掘 |
| 前向去重的指针离原文太远 | 模型回看很远的内容，注意力分散 | T1 先看重复比例；评测中单独对比开关 |
| 决策等价有随机性 | 小样本下结论不稳 | 自一致基线做相对比较；N 按方差调整 |
| T6 成本高 | 填充到 512K 的采样费用可观 | 先做 16K–128K 段，按需扩展 |
| 硬上限触发即失败 | 长任务在仍有预算时被中断 | 原因明确记录；后续手段以新策略加入策略链 |
| 中文估算偏差 | 固定比例低估或高估 | 按字符类别估算；T2 分语种统计 |

## 12. 决策记录（2026-10-30）

只有与具体技术无关的事项升为系统决策；其余都是策略内部参数，改动只产生新的策略版本。

| # | 事项 | 决定 | 依据 |
| --- | --- | --- | --- |
| 1 | 专题范围 | EA-02 只交付 P1，其余在专题内迭代 | 已确认 |
| 2 | 硬上限的失败原因 | 新增通用 `ContextExhausted`；策略与估算细节写在 context 记录 | 复用 `BudgetExhausted` 会绑定调用预算；枚举只加一次 |
| 3 | 保护窗口 | 策略参数；v1 按模型轮数 K = 2 | 改动只递增 `context_strategy` |
| 4 | 软触发 128K / 目标 64K / 最少腾出 32K | 未校准暂定值，写在 `mask@1` 配置；派生值随记录保存 | 同上 |
| 5 | context 记录位置 | 独立 blob；事件只加 `context_ref`、`context_strategy` | 记录结构归模块、独立版本化；事件只存引用 |
| 6 | 档案形式 | 每个“模型 × 服务”一份数据文件，归上下文模块，编译时嵌入；`doc_version` 暂不合并 | 接入新模型只加文件；不动样本导出 |
| 7 | 调用预算形式 | 移出本专题；上下文模块不读不改调用预算 | 两者解耦后各自演进 |
| 8 | Run 是否“一直跑下去” | 由策略链提供；v1 链到 L2 为止，耗尽即停并交出部分报告；清思维链、摘要等以新策略加入，按证据启用 | 继续运行是链的组成问题，不是核心结构问题 |

仍待定：P3 门槛的数值，等 P2 测出自一致基线后再定。

## 13. 参考

- 行业做法：[Anthropic context editing](https://platform.claude.com/docs/en/build-with-claude/context-editing)、[Anthropic compaction](https://platform.claude.com/docs/en/build-with-claude/compaction)、[OpenAI compaction](https://developers.openai.com/api/docs/guides/compaction)、[Google ADK compaction](https://adk.dev/context/compaction/)、[OpenHands condenser](https://docs.openhands.dev/sdk/arch/condenser)、[Cursor dynamic context discovery](https://cursor.com/blog/dynamic-context-discovery)、[Amp handoff](https://ampcode.com/news/handoff)、[Manus context engineering](https://manus.im/en/blog/Context-Engineering-for-AI-Agents-Lessons-from-Building-Manus)
- DeepSeek：[deepseek-harness](https://github.com/deepseek-ai/deepseek-harness)（`f3803c9`，`packages/compaction/*`、`packages/llm/token-meter`）、[DeepSeek-V3.2 技术报告](https://arxiv.org/pdf/2512.02556)、[模型与价格](https://api-docs.deepseek.com/zh-cn/quick_start/pricing/)、[Token 用量计算](https://api-docs.deepseek.com/zh-cn/quick_start/token_usage)、[vLLM: DeepSeek V4 attention](https://vllm.ai/blog/deepseek-v4)
- 研究：[The Complexity Trap](https://arxiv.org/html/2508.21433v3)、[Chroma Context Rot](https://trychroma.com/research/context-rot)、[ACON](https://arxiv.org/abs/2510.00615v2)、[AgentFold](https://arxiv.org/html/2510.24699v1)、[Context-Folding](https://huggingface.co/papers/2510.11967)、[MEM1](https://arxiv.org/html/2506.15841v2)、[ACE](https://arxiv.org/html/2510.04618v2)
