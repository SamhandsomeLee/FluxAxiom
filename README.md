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

## 进度

截至 2026-10-26（`main` 基线 `d6f1a384`），P0a 的 A0–A3、A4a（Trace / 报告读取）、A4b（请求链校验、Sample / Feedback）已实现并通过 CI；真实模型验收另计。**下一阶段是 A5 只读安全负向测试 → G-A 基础验收和至少 5 个实际使用日、10 次有记录的任务 → G-A 通过后才进入 P0b Effect Gateway。** B3 前还需确认宿主 OS 的沙箱/网络/凭据隔离方案。详细 PR、数据证据与阶段门禁见 `docs/development/P0阶段执行方案 v1.1.md` §4.13、§4.14、§5.4、§12；总体边界见 V0.3 §14。
