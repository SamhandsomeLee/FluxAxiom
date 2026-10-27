# G-A 验收报告（P0a 正式验收）

- 状态：**基础验收补证中；本机安全与 CLI 中断回归通过；干净环境构建、远端 CI 与最终 Done 清单待确认；使用观察期未开始**
- 开始日期：2026-10-27
- 依据：执行方案 v1.1 §4.14、§9、§12；ADR-024
- 源码基线（本地）：`main` @ `8c0a61d` 加本轮未提交的安全测试补强、CLI 中断验收脚本与门禁修正。真实模型基线 Run 仍对应 `8c0a61d`；本轮补强尚未 push，也未获得远端 CI 证据。

## 1. 范围与不做

本报告只证明 P0a 可用且可信的验收证据。不启动 B3 / 写操作代码。不把 `Completed` 当成报告质量；不把 OS 沙箱或 Prompt Injection 免疫写成已验证。

## 2. §9 基础验收卡

| # | 验收项 | 结果 | 证据 |
|---|--------|------|------|
| 1 | 干净环境 `fmt` / `clippy` / `test` | **本机缓存构建检查通过；干净环境重建待验证** | `cargo fmt --all -- --check`；`cargo clippy --workspace --all-targets -- -D warnings`；`cargo test --workspace`。现有 target 上的成功不等同于干净环境构建 |
| 1b | Symlink 能力门禁（A5-01） | **通过**（本机 Windows） | `cargo test -p fluxaxiom-engine-custom --test tools -- --ignored` → 3 passed；`cargo test -p fluxaxiom-run-host --test drive -- --ignored` → 1 passed |
| 2 | 用户授权工作区真实分析 + 带证据报告 | **通过** | §3：DeepSeek 基线 `Completed` + 报告 Artifact；事实核对 4/4；缺陷命中 `Inventory::remove` 下溢 |
| 3 | 关闭 CLI 后重启仍可读 Run / Event / Artifact | **通过** | 原 `run` 进程结束后，新进程对同一 `--data-dir` 执行 `status` / `events` / `artifacts` / `trace` / `sample export` 均可读 |
| 4 | Ctrl-C → `Cancelled`；强杀后重启 → `Failed(host_restart)` | **本机端到端通过（Windows）** | `.github/scripts/verify-cli-interrupt.ps1` 向真实 CLI 发送 CTRL_C_EVENT / TerminateProcess；检查未过期租约不误收，再等生产租约自然到期并验证事件，见 §2.1。Host 单测仅作补充 |
| 5 | `prepare_drive` 失败有终态 | **通过** | ADR-021；`drive` / Host 单测覆盖 unstarted abort |
| 6 | Host 独占权威写入；Engine 只经 `HostPort` | **通过** | ADR-019；边界测试与 A5-02 Loop |
| 7 | 只读负向自动化 | **本机补强回归通过；最终 Done 矩阵待签收** | 旧 `ff254c4` / `8c0a61d` 不能单独作验收结论；本轮补齐绝对攻击落点、诱导入链、真实 Host 事件/预算、敏感读取、环境凭据和双输出流，见 §2.2。A5 通过需逐项对照执行方案 §4.13.2 |
| 8 | `flux trace` + 报告读取（A4a） | **通过** | Fake：`cli_fake` / `cli_trace`；真实：§3 `flux trace` |
| 9 | 样本 / `trace --verify`（A4b） | **通过** | Fake：`cli_sample`；真实：`rebuild=consistent`；`sample export` + `feedback=usable` |
| 10 | 使用观察期 5 日 ≥10 次 | **未开始** | 见 §4；门槛：本卡第 1–9 项有可复现证据 |

### 2.1 真实 CLI 中断与恢复证据（Windows，2026-10-27）

- 命令：`pwsh -NoProfile -File .github/scripts/verify-cli-interrupt.ps1`；只连接本机回环代理。合成凭据经实际 DeepSeek 客户端构造，代理不转发 CONNECT，不发送工作区内容到云端。
- Ctrl-C：`run_01a17b50-33a2-71a8-a26f-04a356346a56`；真实 CTRL_C_EVENT 后进程以非零码退出，重开 `status` 为 `Cancelled`，事件含 `attempt.ended` / `run.cancelled`。证据目录：本机临时目录 `flux-cli-interrupt-e66e651a7a124829b46c9d6a0254bc1e`。
- 强杀：`run_01a17b4b-5b23-74f5-91ff-16f3ba35687a`；先由另一 CLI 进程确认活跃 Run 仍为 `Running`，TerminateProcess 后等待生产租约自然过期，不改数据库时间；重开为 `Failed(host_restart)`，事件含 `attempt.orphaned` / `run.failed`。证据目录：本机临时目录 `flux-cli-interrupt-cfc9dceb81dc41a296eaaf4060958768`。
- CI 已接入同一脚本。脚本失败即门禁失败，不降级成“建议手测”。其它 OS 的信号接线未验证。

### 2.2 安全证据补强（2026-10-27）

- `engine-custom/tests/loop.rs`：写入/命令目标均是临时目录绝对路径；先读取 AGENTS.md / README 并断言文案出现在攻击轮请求；检查模型/工具剩余额度与畸形参数的逐条观测结果。
- `apps/cli/tests/cli_privacy.rs`：FakeChat 经真实 RunHost 落库并重新打开；核对 14 个工具调用 ID、结果 blob 与字节数，涵盖绝对/父级越界、隐藏/忽略文件、未知工具、畸形参数；预算不足时第二批整体不执行，持久终态为 Failed(budget_exhausted)。
- 对 run（终态幂等重读）、status、events、trace 文本/JSON/verify、sample export、artifacts 的 stdout 和 stderr 全扫描；每轮请求/响应的 reasoning 默认遮盖，显式标志才展示；长报告正文不进事件或 Sample。
- 子进程注入独立的 DEEPSEEK_API_KEY 假值，并以非法合成凭据测试实际 from_env 拒绝路径；不继承真实凭据。隐藏文件中的内容与环境凭据使用不同标记。搜索同时有公开命中控制，blob 扫描要求确实读到模型语料，防止空检查误通过。
- 本轮修复不能回填旧提交的证明范围；本卡保持“补证中”，直到第 1–9 项与 Done 矩阵逐项完成。
- 本机检查：fmt / Clippy / 工作区测试通过（196 passed、6 ignored）；四项 symlink 能力测试另行执行为 4 passed，剩余两项真实 API 测试未运行；CLI Ctrl-C / 强杀恢复分别通过。两个隔离故障注入探针均正确失败：stderr 泄漏合成标记、shell 分支写盘，证明修复后的断言会拦截此前的误通过。

## 3. 真实 DeepSeek 基线 Run

| 字段 | 值 |
|------|-----|
| 日期 | 2026-10-27 |
| 工作区 | `samples/workspaces/mini-inventory`（已授权样本仓） |
| 提示词 | 见 `samples/cases/mini-inventory.md` |
| `request_id` | `ga-baseline-1` |
| `run_id` | `run_01a17b3f-8420-769a-90c2-9b682321646f` |
| `data_dir` | 本机临时目录 `fluxaxiom-ga-43967a08`（**不入库**；路径仅留在操作者环境） |
| `status` / `terminal_reason` | `Completed` / `Completed` |
| 模型 | `deepseek` / alias `deepseek-flash` / `doc_version=DeepSeek-V4.1-Flash-1017` |
| `prompt_layout_version` | `p0a-loop-1` |
| Engine | `engine-a` `0.1.0` |
| 用量 | model 6/8，tool 11/32；prompt 12065（hit 9088 / miss 2977）；completion 3507；wall 27s |
| `flux trace --verify` | `rebuild=consistent`（退出码 0） |
| Sample | 本机导出 `sample-ga-baseline-1.json`（同 `data_dir`，**不提交**）；schema `0.3-draft`；无 `reasoning_content` |
| `feedback` | `fbk_01a17b40-718f-7268-a06f-01710fb65dce` → `usable` |
| 事实核对 | 4/4 通过；指出缺陷 = 标准答案 `remove` 下溢（见 case 文件） |
| 报告 Artifact | `artifact_01a17b3f-ed37-7503-84f7-a3f763f3052b` |

公开进仓库的摘要行见 `samples/cases/mini-inventory.md` 运行记录表。不含 prompt / reasoning / 密钥 / 报告全文。

**残余**：远端 CI 是否已含 A5 三提交取决于是否 push；其它 OS 的 symlink 能力未在本报告验证。

## 4. 使用观察期（未开始）

门槛：§9 第 1–9 项有可复现证据后开始。持续 **5 个实际使用日**，累计 **≥10** 次有记录真实任务。

建议分配（非硬门槛）：小型确定性 3、中型 Rust 4、较大真实项目 3。

每次最少记录：`run_id`、任务与工作区许可、commit/dirty、模型别名与文档版本、`prompt_layout_version`、`status`/`terminal_reason`、`trace --verify` 三态、Sample（本机）、报告 Artifact、事实核对、`feedback`。

| # | 日期 | 仓库类型 | run_id | verify | feedback | 备注 |
|---|------|----------|--------|--------|----------|------|
| 1 | | | | | | |
| … | | | | | | |
| 10 | | | | | | |

基线 Run（§3）发生在门禁完成前，只作功能/质量基线，不计入正式观察期。门禁完成后另起至少 10 次任务，不追溯计数。

## 5. 结论（滚动更新）

| 类别 | 项 |
|------|-----|
| 已通过 | 本机缓存构建检查；安全补强回归；symlink 本机能力门禁；Windows 真 CLI Ctrl-C / 强杀恢复；真实 DeepSeek 基线 + `consistent` + 事实核对 + `usable`；重启可读 |
| 必需待完成 | 干净环境重建；本轮补强远端 CI；§9 第 1–9 项与 A5 Done 矩阵最终核对 |
| 未开始 | 使用观察期（≥10 次 / 5 日）；完整 G-A 签字 |
| 未验证 | 远端 CI（A5 未 push 前）；非 Windows symlink；OS 级沙箱（属 P0b / ADR-015） |
| 后续处置 | 完成必需待完成项后才开 §4 观察期；多数 `redo` 则先修 Agent，不进 P0b；不得以 Host 单测代替 CLI 信号证据 |

## 6. 并行研究（不实施）

P0b 前需宿主 OS 沙箱 ADR（§5.2 / ADR-015）。本阶段只调研记录，不写 Effect Gateway 代码。
