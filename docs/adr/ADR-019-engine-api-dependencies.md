# ADR-019：engine-api 依赖方向

- 状态：已采用
- 日期：2026-10-21
- 依据：P0 执行方案 v1.1 §3、§8.2；V0.3 §13.3

## 背景

Host 必须能替换 Engine，但不能反向依赖某个 Engine 的具体实现。架构文档允许把 Engine trait 暂留在 `run-host`，等 Engine B 再提取。执行方案把提取提前到 A1.1：现在就建立 `engine-api`，让 Fake Engine 和以后的 Engine A 走同一条端口。

## 决定

1. 依赖方向为：

```text
apps/cli ──> run-host ──> engine-api ──> agent-contract
         └─> engine-custom ──> engine-api
```

2. `apps/cli` 是组合根。它选择具体 Engine 并注入 Host。`run-host` 与 `engine-custom` 互不依赖。
3. 包名与目录为 `fluxaxiom-engine-api`（`crates/engine-api`）和 `fluxaxiom-run-host`（`crates/run-host`）。
4. `engine-api` 只放内部 trait 与这两个 Engine 实现都要使用的类型。它不进入 `contracts/schemas/`。对外契约仍只由 `fluxaxiom-contract` 和 JSON Schema 表达。
5. DeepSeek 客户端先放在 `engine-custom`。出现第二个 Provider 时再拆出 `inference` crate。
6. `storage`、`tool-gateway`、`engine-codex`、`research/*` 在有真实使用点时再创建。正式 crate 和 app 不依赖未晋升的 `research/`。
7. 不建立循环依赖，也不引入万能 Context 或通用插件总线。

## 后果

A1.1 的后续提交实现 `EngineAdapter` 与 `HostPort`，再由 `run-host` 提供端口实现。Fake Engine 只出现在测试中，不成为正式 crate。

租约、中断和重启语义属于 ADR-021，时机仍是 A1.1 的后续实现与 A3，不在本决定中展开。

## 复审触发器

正式模块依赖了 `research/`，依赖图出现环，或一次实验晋升后无法回退。
