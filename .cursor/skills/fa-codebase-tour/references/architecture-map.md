# FluxAxiom 阅读导航（路径索引，非实现真相）

使用前先确认当前 checkout/branch 和目录存在；符号、文件与架构可能已变动。

| 子系统 | 从这里开始 | 进一步核对 |
| --- | --- | --- |
| CLI 组合与任务入口 | `apps/cli/src/main.rs` | `apps/cli/tests/` |
| Run 生命周期、租约和 durable event | `crates/run-host/src/host.rs` | `crates/run-host/src/port.rs`、`events.rs`、`tests/` |
| Engine / Host API 边界 | `crates/engine-api/src/adapter.rs` | `host.rs`、`types.rs` |
| Engine A 只读 Loop | `crates/engine-custom/src/engine.rs` | `chat.rs`、`retry.rs`、`tools.rs` |
| Trace / Replay / Sample | `crates/run-host/src/trace.rs` | `crates/engine-custom/src/rebuild.rs` |
| 持久化 / Schema | `crates/run-host/src/store.rs` | `contracts/schemas/`、`crates/agent-contract/src/` |
| Context（EA-CTX 分支） | `crates/context/src/manager.rs` | `strategy.rs`、`transcript.rs`、`record.rs`、`profile.rs` |

关键政策来源（按当前分支核实）：`docs/adr/ADR-025-engine-a-first-memory-integration.md`，`docs/adr/ADR-026-context-module.md`，`docs/development/上下文压缩专题执行方案（EA-CTX）.md`。

阅读问题顺序：**入口是谁 → 状态归谁 → 哪些能力来自 trait/port → 外部副作用何处发生 → 哪些测试能证明约束**。
