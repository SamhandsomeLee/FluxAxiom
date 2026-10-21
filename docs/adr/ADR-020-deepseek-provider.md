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
4. 请求带 `tools` 时，历史 assistant 消息必须回传 `reasoning_content`。内容为空也保留该字段。它写入 Host blob，供重建上下文。它不进入 durable event payload，也不进入普通日志。保留期限与样本保留策略一致。
5. 思考强度使用 `reasoning_effort`，取值为 `low`、`high`、`max`，默认 `high`，并记入样本。P0a 不把 `temperature` 与 penalty 当作实验变量。
6. `prompt_cache_hit_tokens` 与 `prompt_cache_miss_tokens` 按返回值记录，缺失记为 `unknown`。系统提示与工具 schema 固定在前并版本化为 `prompt_layout_version`，历史只追加。
7. 样本保存原始 usage 与调用时间。费用用版本化价格表离线计算，运行时不写死单价。
8. 只发送已授权工作区的内容。敏感仓库在确认服务条款前不作为 P0a 样本。

## 后果

A2.1 再实现薄客户端、流式响应和 Fake Provider。本 ADR 不包含 HTTP 客户端、密钥存放或真实 API 调用。

客户端需要覆盖的失败场景仍以 §4.7 为准：缺少 `reasoning_content` 的 400、空 `reasoning_content` 字段、429、5xx、流中断、非法工具参数，以及 `finish_reason = length`。400 不重试。429 与 5xx 退避后重试。流中断视为该次调用失败。

## 复审触发器

官方模型列表或 `deepseek-flash` 所指向的版本发生变化，或者 Chat Completions 不再回传本 ADR 依赖的 `reasoning_content` 与缓存用量字段。
