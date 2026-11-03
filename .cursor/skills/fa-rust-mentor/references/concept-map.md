# Rust × FluxAxiom 概念映射

这是学习导航，**不是**对当前代码细节的保证；回答前读取实际源文件。

| Rust 机制 | 推荐源码入口 | 必须解释的工程问题 |
| --- | --- | --- |
| trait / dyn dispatch | `crates/engine-api/src/host.rs`，`adapter.rs` | 依赖倒置与动态边界，`Send + Sync` 是否必要 |
| 所有权 / 借用 | `crates/engine-custom/src/engine.rs` | 请求、messages、blob 的复制/移动/借用时机 |
| async / cancellation | `crates/engine-custom/src/engine.rs` | await 点、取消传播与资源生命周期 |
| Arc / Mutex | `crates/run-host/src/host.rs` | 谁共享资源；进程内互斥与 SQLite 事务不是同一回事 |
| Result / 错误分层 | `crates/engine-api/src/host.rs` | Invalid、Storage、Unsupported 不可混用 |
| serde / 协议契约 | `crates/agent-contract/src/event.rs` | 字段缺失、空值、输出结构、版本兼容 |
| 策略模式与 Replay | `crates/context/src/strategy.rs` | trait 的扩展边界、确定性重建与策略顺序 |
| 泛型/测试替身 | `crates/engine-custom/src/retry.rs` | 可替换 Transport/Backoff 的测试收益 |

推荐解释顺序：**类型表示什么 → 谁拥有值 → 谁能借用 → 何时跨线程/await → 出错怎么办 → 用哪个测试证明**。
