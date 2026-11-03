# FluxAxiom Code Review Checklist

按变更相关性选择，拒绝无差别粘贴所有条目。具体边界以当前代码、Contract 与 ADR 为准。

## Boundary / State
- RunHost 仍独占 Run/Attempt/Event 的权威写入？Engine 经 `HostPort` 观测？
- 关键状态变更与 durable event 是否在同事务？未知是否错误地归类为成功/失败？
- 预算、租约、取消、恢复、并发交错有没有负向路径？

## Privacy / Safety
- 密钥、prompt 全文、reasoning、文件正文是否意外进入 event、普通日志、Debug？
- 文件工具是否越出已授权 `WorkspaceRoot`、忽视分页/截断或执行未授权 shell？
- 未授权外部 Memory 写入/副作用是否出现？

## Protocol / Replay
- Serde 对 Option、default、null、空字符串、必填键的处理是否一致？
- 外部响应字段、版本与完整 JSON shape 是否有匹配测试？
- Context 变更后 `prepare/replay` 是否能逐字节重建；存根、保护窗口、编辑记录是否一致？
- 旧事件/样本能否解析？是否引入未经批准的行为变更？

## Rust / Tests
- 生命周期、锁范围、await、Send/Sync、错误传播、潜在死锁？
- 是否为编译方便多余复制、默认值、`unwrap` 或 `map_err` 吞错？
- 单元、契约、集成、回归测试里，哪里能证明改动所声称的不变量？
- `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace` 的实际输出状态是什么？

参考 `.cursor/rules/review-sweep.mdc` 执行同类问题扫描，不把清单上未列的可触发缺陷漏掉。
