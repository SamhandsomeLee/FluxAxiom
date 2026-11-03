---
name: fa-code-review
description: "对用户实现的 Rust diff 进行只读架构、协议、可靠性、安全和测试覆盖审查"
disable-model-invocation: true
---

# Code Review · 独立审查

## Purpose
独立审查人类代码，不把审查变成 Cursor 自动重写。默认只读；遵守 `../../rules/mentor-mode.mdc`。

## Input / Evidence
优先查看工作区当前 diff（staged + unstaged，必要时相对于目标分支），明确本次审查范围；读取受影响的调用方、Contract、测试、ADR 和 `references/review-checklist.md`。完整核对依据 `../../rules/review-sweep.mdc`，它是手动规则，不作为全局注入。

## Workflow
1. 列出实际改动文件与主要行为，不能只审查用户特别指出的一行。
2. 找到可触发缺陷并给出 **文件 + 已核实的位置 + 触发条件 + 影响 + 修复方向**。
3. 风险分级：P0（安全/数据完整性/核心不变量）、P1（确定或高置信行为错误）、P2（性能/维护性/潜在设计问题）、Suggestion（非阻塞）。
4. 专门核对错误分类、状态与 durable event、隐私/日志、Context Replay、schema、旧数据、取消/并发、失败路径。
5. 将“证据充分的问题”与“建议进一步验证的问题”分开；缺少测试结果则标明未运行。
6. 报告已审查与未审查范围；停下等待用户修复或明确授权，**不自动提交、push 或修复**。

## Output Contract
按严重性排序的 findings（若无，明确“未发现”但不承诺无 bug）→ 验证证据 → 剩余风险/未核对 → 建议验收命令。

## Restrictions
不得编造具体行号或误报未来规划为已实现；不得自行编辑文件；修复前/后均须重新核对相邻同类问题。
