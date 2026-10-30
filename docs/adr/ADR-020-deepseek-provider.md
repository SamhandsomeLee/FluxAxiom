# ADR-020：DeepSeek 作为 P0 推理 Provider

- 状态：已采用
- 日期：2026-10-21
- 依据：P0 执行方案 v1.1 §4.7、§8.2

## 背景

P0 需要一个云端模型完成只读仓库分析。执行方案已经排除 Anthropic 兼容端点、Responses API，以及 DeepSeek 服务端的 Search/Fetch。型号会改指新版本，样本必须能说明当时调用的是哪一个模型。

2026-10-21 核对的官方模型列表只有两项：`deepseek-flash`（由 DeepSeek-V4.1-Flash-1017 提供）和 `deepseek-v4-pro`（执行方案记录的版本为 DeepSeek-V4-Pro-0813）。V4.1 Flash 的发布日期晚于该 Pro 版本。

## 决定

1. P0 唯一云 Provider 为 DeepSeek。协议使用 OpenAI 兼容的 Chat Completions，`base_url` 为 `https://api.deepseek.com`。
2. 型号仍是配置项 `model`。P0 默认采用核对时的最新模型：`deepseek-flash`，对应 DeepSeek-V4.1-Flash-1017。官方把别名改指新版本时，更新本 ADR 和样本中的版本记录，不在运行时猜测“最新”。
3. 每个样本记录请求的模型别名、调用时间，以及当时文档所列的模型版本。评测不把不同模型版本混在一起计算。
4. 请求带 `tools` 时，历史 assistant 消息必须回传 `reasoning_content`。要回传时字段用字符串并始终序列化，空内容写成 `""`，不省略，也不写成 `null`。不带 `tools` 的请求不把思维链放进下一轮。它写入 Host blob，供重建上下文。它不进入 durable event payload，也不进入普通日志。保留期限与样本保留策略一致。失败或流中断的响应不进入历史。
5. 思考强度使用 `reasoning_effort`，取值为 `low`、`high`、`max`，默认 `high`，并记入样本。P0a 不把 `temperature` 与 penalty 当作实验变量。
6. `prompt_cache_hit_tokens` 与 `prompt_cache_miss_tokens` 按返回值记录，缺失记为 `unknown`。系统提示与工具 schema 固定在前并版本化为 `prompt_layout_version`。事实只追加；每次请求是事实与 context 记录的确定函数（2026-10-30 由 ADR-026 修订，原文为“历史只追加”）。
7. 样本保存原始 usage 与调用时间。费用用版本化价格表离线计算，运行时不写死单价。
8. 只发送已授权工作区的内容。敏感仓库在确认服务条款前不作为 P0a 样本。

## API 观察（2026-10-22）

核对页面：<https://api-docs.deepseek.com/zh-cn/guides/thinking_mode>。这次没有调用真实 API。

- 官方写明：请求携带 `tools` 时，后续必须回传 `reasoning_content`，否则 400。未携带 `tools` 时不必回传；传了也会被忽略，不拼进上下文。
- 官方没有定义“省略键”“空字符串”“`null`”哪一种算回传，也没有定义 `tools: []`。
- 同一页的工具样例把工具声明写成 `{"type":"function","function":{"name","description","parameters"}}`。样例回传的 `tool_calls` 是 SDK 对象，含 `id`、`type: function` 和嵌套的 `function.name` / `function.arguments`。工具结果消息是 `{"role":"tool","tool_call_id","content"}`。这是样例形状，不是这次对真实 API 的调用。
- [Create Chat Completion](https://api-docs.deepseek.com/zh-cn/api/create-chat-completion) 写明：不设置 `include_usage` 时，中间块没有 `usage`，但 `[DONE]` 前的最后一块仍带整次用量。设为 `true` 时，中间块的 `usage` 为 `null`，统计仍在最后一块。`stream_options` 必须与 `stream: true` 一起发送，否则 400。这次没有调用真实 API。
- 空字符串能通过，来自 2026-04-24 对真实 API 的社区核对，不是官方条文：没有该键会 400，`"reasoning_content": ""` 能通过。`null` 没有这份核对。
- Fake 按上述假设拒绝“带 tools 且 assistant 没有该键”的原始 JSON。Fake 通过不证明当前 DeepSeek 版本也这样。真实端到端仍需覆盖：带 tools / 不带 tools，以及字段缺失 / 空串。

## API 观察（2026-10-23）

实际读取的页面（英文站，由子代理抓取后逐字转述，我方没有再复读）：[Create Chat Completion](https://api-docs.deepseek.com/api/create-chat-completion)、[思考模式](https://api-docs.deepseek.com/guides/thinking_mode)、[Pricing](https://api-docs.deepseek.com/quick_start/pricing)。`guides/function_calling` 这个地址返回的是无关页面（标题为 Your First API Call），没有工具调用内容，所以**没有**核对。没有调用真实 API。

- `max_tokens` 已写明的：合法范围 1 到 393216；不设置时默认 8K（非思考模式）、64K（思考模式）、128K（思考模式且 `reasoning_effort` 为 `max`）。默认档位按**模式**和 `reasoning_effort` 区分，不是按型号。Pricing 页的最大输出为 384K。触顶时 `finish_reason` 为 `length`，`response_format` 说明里写到 `content` 可能被截断。客户端默认写入未校准的暂定值 32768，显式省略键才用服务端默认。
- `max_tokens` **文档没有写**的（对 32768 这个暂定值影响最大）：思考模式下 `max_tokens` 是否把思维链与最终回答合计（64K 的默认值只是暗示）；触顶时 `reasoning_content` 如何处理；触顶时被截断的 `tool_calls` 是什么状态；是否存在 `max_completion_tokens`（请求参数表里没有）。因此 32768 可能被思维链耗尽而得到空的 `content`，必须用真实调用的 `completion_tokens` 与 `reasoning_tokens` 校准。
- 流式 chunk schema：每个 `tool_calls` 元素的 `index` 为 required；`id`、`type`、`function.name`、`function.arguments` 无 required 标记。页面说明首块常带 id/type/function，后续块常只带 arguments 片段。页面没有给出真实工具分片字节样例，也没有写明 `finish_reason: tool_calls` 落在哪一块。客户端按 index 累积的规则标「待真实响应替换」。
- 非流式 schema 里工具调用的 `id` 与 `name` 为 required。流拼完后仍缺 id 或 name 为空，按服务端违约处理为 `MalformedToolCall`，不用 `StreamInterrupted`。

## 后果

A2.1 再实现薄客户端、流式响应和 Fake Provider。本 ADR 不包含 HTTP 客户端、密钥存放或真实 API 调用。

客户端需要覆盖的失败场景仍以 §4.7 为准：缺少 `reasoning_content` 的 400、空 `reasoning_content` 字段、429、5xx、流中断、非法工具参数，以及 `finish_reason = length`。400 不重试。429 与 5xx 退避后重试。流中断视为该次调用失败。

## 复审触发器

官方模型列表或 `deepseek-flash` 所指向的版本发生变化，或者 Chat Completions 不再回传本 ADR 依赖的 `reasoning_content` 与缓存用量字段。

## 补充（2026-10-26 / A4b）

样本导出所需的「别名 → 文档版本、核对日期」落在 `engine-custom::doc_version` 小表，初始仅 `deepseek-flash → DeepSeek-V4.1-Flash-1017（2026-10-21）`。别名改指或增行时，与本 ADR 同一次提交更新该表；未知别名导出为 `unknown`。详见 ADR-023。

## 补充（2026-10-30 / ADR-026）

第 6 条的“历史只追加”改为“事实只追加”：事件与 blob 不因压缩而删除或改写，请求允许是对事实的确定性编辑，编辑记录存为 context 记录 blob。`prompt_layout_version` 只覆盖 system 与工具 schema，上下文策略另用 `context_strategy` 版本。第 4 条的 `reasoning_content` 回传规则不变；是否允许清除旧轮思维链，待 EA-CTX 的 T7 实测后另行决定。详见 [ADR-026](ADR-026-context-module.md)。
