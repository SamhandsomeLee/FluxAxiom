# FluxAxiom

个人 Agent 系统。当前阶段是 P0a：只读仓库分析 CLI（`flux`）。

## 构建检查

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

默认测试使用 Fake Provider，不访问网络。

## 运行

默认的 Fake Engine 不拨号，用来验证 CLI、Host 与事件链路：

```bash
cargo run -p fluxaxiom-cli -- run --workspace <目录> --prompt "分析这个仓库"
```

### 接入真实模型（DeepSeek）

1. 复制模板并填写密钥：

   ```bash
   cp .env.example .env     # Windows: copy .env.example .env
   ```

   在 `.env` 里写 `DEEPSEEK_API_KEY=sk-...`。`.env` 已被 `.gitignore` 忽略，不要提交。

2. 用 `--engine deepseek` 运行：

   ```bash
   cargo run -p fluxaxiom-cli -- run --engine deepseek --workspace <目录> --prompt "分析这个仓库"
   ```

`flux` 启动时从当前目录向上找第一个 `.env`。已设置的环境变量优先于 `.env`。缺少密钥时在创建 Run 之前报错，不会留下孤儿 Run。

**数据出境**：`--engine deepseek` 会把授权工作区的内容发送到 DeepSeek 云端。敏感仓库不要用（ADR-020）。

真实 API 的两个 live 测试默认忽略。它们只读进程环境变量，**不读 `.env`**；没有密钥时直接返回并显示通过，不拨号。需要先在当前 shell 设置密钥再运行：

```bash
# PowerShell: $env:DEEPSEEK_API_KEY = "sk-..."
export DEEPSEEK_API_KEY=sk-...
cargo test -p fluxaxiom-engine-custom --test sse -- --ignored
```

## 命令

| 命令 | 作用 |
|---|---|
| `flux run` | 创建 Run、同步驱动 Engine，边跑边打印已提交事件；收尾打印报告正文。只有 `Completed` 以零退出 |
| `flux status <run_id>` | 打印 Run 快照 |
| `flux events <run_id> [--after-sequence N] [--follow]` | 打印 durable events；`--follow` 轮询到终态 |
| `flux artifacts <run_id> [--show [artifact_id]]` | 列出 Artifact 摘要；`--show` 输出正文 |
| `flux trace <run_id> [--json] [--verify] [--show-request N] [--show-response N] [--include-reasoning]` | 按模型轮次查看调用时间线；`--verify` 检查请求链一致性 |
| `flux sample export <run_id> [--out FILE]` | 导出引用/摘要式 Sample JSON，含重建结论与反馈 |
| `flux sample feedback <run_id> --verdict usable\|needs-edit\|redo [--note TEXT]` | 为一次 Run 追加人工评价（实际 CLI 选项可用 `--help` 校对） |

`run` 的主要参数：`--workspace`、`--prompt`、`--request-id`（幂等键）、`--engine fake|deepseek`、`--max-model-calls`（默认 8）、`--max-tool-calls`（默认 32）。

Ctrl-C 取消当前 Run（`Cancelled`），第二次 Ctrl-C 强制退出。进程崩溃后，租约过期的 Attempt 在下次启动时被标为 `Orphaned`，Run 为 `Failed(host_restart)`。

### 数据目录

按优先级：`--data-dir`，环境变量 `FLUXAXIOM_DATA_DIR`，平台默认（Windows 为 `%LOCALAPPDATA%\FluxAxiom`，其它为 `$XDG_DATA_HOME/fluxaxiom` 或 `~/.local/share/fluxaxiom`）。

## 目录

| 路径 | 内容 |
|---|---|
| `apps/cli` | `flux` 命令行，组合根 |
| `crates/agent-contract` | Run / Attempt / Event / Artifact 契约类型，schema 在 `contracts/` |
| `crates/engine-api` | Engine 与 Host 之间的窄接口、`WorkspaceRoot` |
| `crates/engine-custom` | Engine A：DeepSeek 客户端、Loop、只读工具 |
| `crates/run-host` | SQLite 存储、事件、预算、租约与取消 |
| `docs/` | 架构、阶段执行方案、ADR |

## 进度与路线（2026-10-29）

`main@2aed418` 已有 P0a A0～A4b、A5 只读安全回归。**2026-10-29 冻结 G-A**：5 个实际使用日和至少 10 次真实任务不启动，正式签收未完成，不得写成通过。该未完成项不再阻塞 EA。最新状态见 [G-A 验收报告](docs/development/G-A验收报告.md)。

**新决策：停止 Engine B/Codex app-server 对照实验**。后续直接深化唯一自研 Engine A（EA-01～04/EA-G），再核对独立 Agent Memory Engine 的真实 API（M0）、接入默认关闭的只读召回（M1）、开展 Memory 对照评测（M-EVAL）；有授权、幂等和对账证据后才考虑 M2/M3 记忆写回。冻结期间 Memory 不改写无记忆基线。B3/B4/B6-A 安全 Effect 支线仅服务 Engine A，不因本次冻结开工，M1 不需要 B3。Memory 尚未接入。

- [ADR-025：Engine A 与独立 Memory 的边界](docs/adr/ADR-025-engine-a-first-memory-integration.md)
- [Engine A 与 Agent Memory Engine 协同演进方案](docs/development/EngineA与AgentMemoryEngine演进方案.md)
- [P0 阶段执行方案 v1.1（含新路线）](docs/development/P0阶段执行方案%20v1.1.md)
