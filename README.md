# FluxAxiom

 Agent 系统。当前阶段是 P0a：只读仓库分析 CLI。

## 构建检查

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## 运行

```bash
cargo run -p fluxaxiom-cli
```

不带参数时打印 `FluxAxiom`。`run`、`status`、`events`、`artifacts` 尚未实现。
