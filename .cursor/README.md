# FluxAxiom · Human-Led Development System v1.0

## 目标与边界

本目录为 **自适应导师模式**：人主导架构决策、Rust 业务实现和主要测试；Cursor 负责源码导航、方案比较、分级提示、调试引导、测试设计与独立审查。工程安全和验收标准不因教学而降低。

- `rules/`：持续有效的权限、边界、阶段与审查约束。Rules 是行为约定，**不是系统级文件写权限**。
- `skills/`：七个需显式执行的教学流程。输入 `/fa-...` 调用；避免与同名 Commands 重复。
- `commands/`：本版本不创建；常用入口已由 Skills 提供。
- 更高优先级的 Cursor 设置、实际工具权限和用户明确指令仍决定能否读写文件；不要把 Rule 当作沙箱。

## 开始使用

推荐在 Cursor **Ask（只读）**模式运行理解、设计、Rust 教学、调试讨论和审查。需要执行测试或有限改动时，切到有相应工具的模式，**单独确认可写文件/命令范围**。模式与 Rule 不能替代审查 diff。

| 任务 | 主动调用 |
| --- | --- |
| 阅读代码 / 绘制调用链 | `/fa-codebase-tour` |
| 比较架构方案 / 设计接口 | `/fa-architecture-coach` |
| Rust 原理、所有权、Trait、异步 | `/fa-rust-mentor` |
| 定位编译 / 运行故障 | `/fa-debug-coach` |
| 设计测试矩阵 | `/fa-test-coach` |
| 审查自己写的代码 | `/fa-code-review` |
| 回顾能力与迁移练习 | `/fa-learning-review` |

### 典型任务流

1. `/fa-codebase-tour`：先找实际入口、调用链、状态和测试；区分现有实现与未来计划。
2. `/fa-architecture-coach`：定范围、方案、接口和失败语义，明确人类尚需决策的点。
3. 人类编写代码；疑问调用 `/fa-rust-mentor`（L1→L4）或 `/fa-debug-coach`。
4. `/fa-test-coach`：优先让人类写测试；执行结果需有可见证据。
5. `/fa-code-review`：独立审查当前 diff，不自动修复或提交。
6. `/fa-learning-review`：简短复盘，只有明确授权才写持久学习笔记。

对小型、明确任务可跳过不必要的阶段，**不可跳过安全边界、明确授权和必要验证**。用户可以直接要求完整解释、参考代码或授权局部辅助实现。

## 自适应提示协议

- **L1** 原理/不变量和方向；默认。
- **L2** 定位相关表达式、类型或数据流。
- **L3** 最小例子、伪代码或接口签名（与生产实现区分）。
- **L4** 用户明确请求时给完整参考实现。**给代码 ≠ 获得写文件权限**。

## FluxAxiom 的证据优先级

- **当前实现事实**：始终以正在工作的分支实际源码、测试及构建结果为准。
- **架构约束**：采用的 ADR 与版本化 Contract；冲突时展示冲突，不私下覆写决策。
- **实施范围**：当前阶段执行文档与 `scope.mdc`；历史 V0.3 仅作背景，不恢复已停止路线。
- **待实现规划**：只表述为计划，不冒充既成代码。

重要入口：`apps/cli/src/main.rs`、`crates/run-host/src/host.rs`、`crates/engine-api/src/host.rs`、`crates/engine-custom/src/engine.rs`、`crates/context/src/manager.rs`（存在于 EA-CTX 分支）。详见 `skills/fa-codebase-tour/references/architecture-map.md`。涉及 Context 编辑时参照 `docs/adr/ADR-026-context-module.md`。

## 配置检查与人工验收

- `rules/mentor-mode.mdc`、`boundaries.mdc`、`scope.mdc`、`git.mdc` 为 Always；`pairing.mdc` 为 Intelligent；`comments.mdc`、`p0a-runtime.mdc` 为按文件触发；`review-sweep.mdc` 为手动。
- 7 个 `SKILL.md` 的 frontmatter 名称与目录一致，均带 `disable-model-invocation: true`；逐一测试手动 `/fa-*` 入口能否在你的 Cursor 版本中出现。
- **只读测试**：说“分析一下错误”，不应发生工作区写入或 Git 提交。
- **参考实现测试**：要求完整代码示例可以得到示例，但工作区应保持不变。
- **授权测试**：仅授权指定文件的局部修改，确认 diff 中无其他文件。
- **审查测试**：仅说“审查”，应得到带证据和风险等级的问题，而不是自动修复。
- **Git 测试**：讨论、认可或审核 diff 不可触发 commit/push。

这是配置与流程变更；**Rust 的 cargo fmt/clippy/test 应在实际业务改动时运行**。是否能在 Ask 模式中直接执行工具取决于实际产品权限与模式设置。
