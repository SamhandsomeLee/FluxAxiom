# FluxAxiom

[English](README.md)

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
| `flux trace <run_id> [--json] [--verify] [--show-request N] [--show-response N] [--include-reasoning]` | 按模型轮次查看调用时间线；`--verify` 用 context 记录重放请求，再和当时的 `messages` 比较 |
| `flux sample export <run_id> [--out FILE]` | 导出引用/摘要式 Sample JSON，含重建结论与反馈 |
| `flux sample feedback <run_id> --verdict usable\|needs-edit\|redo [--note TEXT]` | 为一次 Run 追加人工评价（实际 CLI 选项可用 `--help` 校对） |
| `flux ctx replay <run_id> --call N [--samples 5] [--strategy identity@1] [--engine fake\|deepseek]` | 对已保存的第 N 次请求再采样，打印下一步一致率。不写 Run。目前只接受 `identity@1` |

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
| `crates/engine-custom` | Engine A：DeepSeek 客户端、Loop、只读工具、请求链校验 |
| `crates/context` | 上下文模块：档案、估算、L0–L2 策略、context 记录与 `replay`。不依赖 Engine 或 Host |
| `crates/run-host` | SQLite 存储、事件、预算、租约与取消 |
| `docs/research/context/` | EA-CTX 实测记录。只写方法、计数和用量，不写密钥、prompt 全文、思维链或工具正文 |
| `docs/` | 架构、阶段执行方案、ADR |

## 进度与路线（2026-11-05）

`main@2aed418` 已有 P0a A0～A4b、A5 只读安全回归。**2026-10-29 冻结 G-A**：5 个实际使用日和至少 10 次真实任务不启动，正式签收未完成，不得写成通过。该未完成项不再阻塞 EA。最新状态见 [G-A 验收报告](docs/development/G-A验收报告.md)。

**新决策：停止 Engine B/Codex app-server 对照实验**。后续直接深化唯一自研 Engine A（EA-01～04/EA-G），再核对独立 Agent Memory Engine 的真实 API（M0）、接入默认关闭的只读召回（M1）、开展 Memory 对照评测（M-EVAL）；有授权、幂等和对账证据后才考虑 M2/M3 记忆写回。冻结期间 Memory 不改写无记忆基线。B3/B4/B6-A 安全 Effect 支线仅服务 Engine A，不因本次冻结开工，M1 不需要 B3。Memory 尚未接入。

- [ADR-025：Engine A 与独立 Memory 的边界](docs/adr/ADR-025-engine-a-first-memory-integration.md)
- [ADR-026：上下文管理独立模块](docs/adr/ADR-026-context-module.md)
- [Engine A 与 Agent Memory Engine 协同演进方案](docs/development/EngineA与AgentMemoryEngine演进方案.md)
- [P0 阶段执行方案 v1.1（含新路线）](docs/development/P0阶段执行方案%20v1.1.md)
- [EA-CTX 上下文压缩专题执行方案](docs/development/上下文压缩专题执行方案（EA-CTX）.md)

## 上下文压缩专题（EA-CTX，2026-11-05 暂时结项）

上下文管理是独立模块。Engine 每次模型调用前调用 `ContextManager::prepare`，得到要发送的视图和一条 context 记录。校验器调用 `ContextManager::replay`，再和当时发出的请求比较。事实原文只追加：系统提示、用户文本、assistant 消息和工具结果的 blob 不因压缩被改写或删除。编辑只记在 context 记录里，事件只存 `context_ref` 和 `context_strategy`。

模块不读调用预算，不发网络请求，不打开 blob，也不依赖 `engine-custom` 或 `run-host`。

### 准备一条请求

`prepare` 按这个顺序工作：

1. **L0 前向去重**，每次都跑。新的工具结果若与本 Run 里更早的某条完全相同（同工具、同规范化参数、同结果引用），新正文换成指针存根，例如 `[unchanged: same result as round #3 call 1]`。历史正文不动。
2. **估算**。有完整用量时，用上一轮的 `prompt_tokens + completion_tokens`，再加这次新出现的工具正文和用户文本。只有 `prompt_tokens` 已知时，它只作为下界，不把缺失的 completion 补成 0。没有用量时，对可计数字符做全量估算。档案 v2 计入汉字、ASCII 英文字母和 ASCII 数字；空白和标点不计入。`estimate_error` 仍是未校准的 0.30。
3. **L1 作废遮蔽**。估算超过未校准的软触发 128K 才试。同一文件后来的 `read_file` 盖住了更早的一次读取时，更早的正文换成 `[superseded: ...]`。最近 2 个模型轮里的工具结果不动。没超过硬上限时，至少要腾出 `max(32K, 当时估算 / 4)` 才留下；已经超过硬上限时，即使腾出不够也留下。腾出不够就整段退回。
4. **L2 陈旧遮蔽**。L1 留下或退回之后，估算仍高于目标 64K 才试。64K 是软触发的一半，同样未校准。从最旧的工具结果开始，把保护窗口外、正文仍是原文、且不是本轮 L0 指针目标的工具结果换成 `[stale: 工具名 参数, 大小 B, 引用; call this tool again if needed]`。去留规则与 L1 相同：一批做完再决定，不在遮到 64K 时中途停下。
5. **硬上限**。估算仍超过硬上限，或硬上限为 0，就不发这次模型请求。记录仍然保存，Run 以 `Failed(context_exhausted)` 结束，可以带上已有的部分报告。写记录或诊断失败则是 `Failed(engine_error)`。

system、user 和全部 assistant 消息（正文、思维链、工具调用）不被这三层改写。

`prepare` 写下的策略名：

| 留下的层 | 策略名 |
| --- | --- |
| 只有 L0 | `l0@1` |
| L0 + L1 | `l0l1@1` |
| L0 + L2 | `l0l2@1` |
| L0 + L1 + L2 | `l0l1l2@1` |

`identity@1` 用于还没有 context 记录的旧事件。`l1@1` 和 `l2@1` 只用于按单层记录重放，`prepare` 不把它们当作留下后的名字。方案里的整条链曾叫 `mask@1`；这条分支按实际留下的层拆开命名。

### 记录与校验

新的 `prepare` 写出 schema 3。`spec` 含硬上限、软触发、最少腾出、保护窗口和目标。`replay` 接受 schema 1、2、3，但不读这四个派生值，只按 `edits` 还原视图。schema 1 只有硬上限；schema 2 没有目标。记录里没有 prompt、工具正文或思维链。

`flux trace --verify` 检查当时的 `messages` 是否等于 `replay` 再序列化的结果。比较的是 JSON 值，键序和空白不算差异。合法的 L2 存根为 `consistent`。篡改存根、把陈旧编辑写进最近 2 轮、请求已是存根但记录没有对应编辑、记录有编辑但请求仍是原文，都是 `inconsistent rule=1`。未知策略或不可重放的记录是 `unverifiable`。

### 本轮做完的范围

| 阶段 | 结果 |
| --- | --- |
| P1 | 独立 `context` crate、DeepSeek Flash 档案、估算器、硬上限、`context_exhausted`、context 记录，以及 Engine / Host / `trace` 的接入 |
| 实测轮 | T0–T11 各有记录，未知字段保持未知。T6 到 512K 没有出现质量下降点，`effective_context` 不写数值，软触发保持 128K |
| P2 | 一次 Run 的离线构成统计（T1），以及 `flux ctx replay` |
| P3 机制 | L0、L1、L2 接入 `prepare`。`l0l2@1` 与 `l0l1l2@1` 的校验用例已覆盖合法存根和四类规则 1 失败 |
| P3 测量 | 2026-11-05，任务 `mini-inventory-k5` 的第 5 次请求。保护窗口外 5 条工具结果、873 字节换成存根。5 次采样的下一步都与未填充基线相同，决策等价率 1，损失 0。`prompt_tokens` 从 2964 降到 2907 |

这次测量的缓存用量：

| 阶段 | prompt | hit | miss |
| --- | ---: | ---: | ---: |
| 改写前，两次相同 | 2964 | 2816 | 148 |
| 遮蔽后第 1 次 | 2907 | 384 | 2523 |
| 同一条遮蔽请求再发 | 2907 | 2688 | 219 |

记录见 [P3-mask-decision-2026-11-05.md](docs/research/context/P3-mask-decision-2026-11-05.md)。

### 暂时结项时仍未完成

这些不写成通过：

- 决策等价门槛没有通用数值。上面那次请求约 2964 token，低于 128K，`prepare` 不会触发 L2。换掉的是更早的 873 字节，最近两轮里较大的工具结果还在。
- 触发前后的缓存只来自这一次改写，不能外推成缓存策略。
- 服务端报告上下文超限时，尚未拿到可用于归类的真实错误样本。认不出的响应仍是 `ProviderError`，不会强制再跑一层并重试。
- P4 不做。软触发继续用未校准的 128K。
- P5 不启动。不清旧轮思维链，不做摘要或折叠，也不加第二个模型档案。
- G-A 仍是 2026-10-29 的冻结状态。Memory 仍未接入，也不改这条无记忆基线。

以后用更长的任务重测时，结果另存记录，不改 2026-11-05 这次结论。
