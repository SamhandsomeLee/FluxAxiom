# ADR-021：Attempt 租约、中断语义与 drive 失败终态

- 状态：已采用（A3a 落地 §4.7.4；A3b 落地 Ctrl-C、租约续期与孤儿扫描，细则见 §4）
- 日期：2026-10-24
- 依据：P0 执行方案 v1.1 §4.2、§4.7.4、§4.7.6、§4.8；V0.3 §8.1；ADR-019

## 背景

A1 起 Attempt 就带 `owner` 与 `lease_expires_at`，但未续期、未扫描。A2.2 的 `prepare_drive` 在任务 blob 或工作区打开失败时直接返回错误，Run 停在 `Accepted`，无终态事件（§4.7.4 第 1 项）。预算已用量只在单次 `drive_run` 的内存里（§4.7.6），同 Run 再 drive 会重置计数（§4.7.4 第 3 项）。A3 开始前必须把这些缺口写成决定。

## 决定

### 1. `prepare_drive` 失败（§4.7.4 第 1 项）

采用方案 **(1)**：在仍为 `Accepted` / Attempt `Starting` 时，同一 `BEGIN IMMEDIATE` 事务写入：

- Attempt → `Ended`，`outcome = failed`，`error_reason` 为简短类别（如 `workspace root is unavailable`）
- Run → `Failed`，`terminal_reason = engine_error`
- `attempt.ended` 与 `run.failed` 各一条 durable event

不进入 `Running`，不调用 Engine。契约允许 `Accepted → Failed`；本 ADR 同时允许 `Starting → Ended`（从未真正跑过的 Attempt 直接收尾）。

`drive_run` 在完成上述终态后返回 `Ok(EngineOutcome::Failed { reason: EngineError, partial: None })`，便于 CLI 与成功路径一样读快照。存储/锁错误仍返回 `Err`。

**只改写还没开始的 Run。** `prepare_drive` 在读任务 blob、打开工作区**之前**先确认 Run 为 `Accepted`、Attempt 为 `Starting`；写失败终态的事务内再确认一次。任一不成立（已 `Running`、`Cancelling` 或已终态）一律返回 `Err(Invalid("run is not startable"))`，不改库、不写事件。这样别的进程正在 drive、或 Run 早已结束时，工作区被删也不会把它改写成 `Failed(engine_error)`，也不会向调用方报告与库中状态矛盾的 `Ok(Failed)`。

### 2. `read_file` 大文件 I/O（§4.7.4 第 2 项）

A3a **不改**。触发条件（单文件数 MB、工具耗时明显偏大）未到之前保持执行方案原文与工具注释中的已知限制。触发后另开改动，不在本 ADR 预选游标方案。

### 3. 同 Run 再 drive 与预算（§4.7.4 第 3 项）

A3a / A3b **不允许**对同一 Run 再次 `drive_run`（仍仅 `Accepted` 可进入 `Running`；失败已终态化的 Run 不可重启）。因此本阶段不入库已用量，也不把续跑定义为新预算周期。

若以后允许续跑 / 重试同一 Run，必须先另写 ADR，在 (1) 已用量入库 与 (2) 新预算周期告知用户 之间二选一。

### 4. 中断与租约（§4.8；实现时机）

语义仍以执行方案 §4.8 为准：

- Ctrl-C → `Cancelling` → 取消令牌 → `Cancelled`
- `kill -9` / 崩溃 → 启动时扫描租约已过期的 `Running` / `Cancelling` Attempt → `Orphaned` + Run `Failed(host_restart)`

**A3a 不实现** Ctrl-C 接线、租约续期与启动扫描。**A3b 已实现**（见执行方案 §4.10），细则如下。

**取消**

- CLI 监听 Ctrl-C 并调用 `request_cancel`（放进阻塞线程，不占异步 worker）。第二次 Ctrl-C 直接 `exit(130)`，按崩溃处理，靠租约过期收尾。
- `drive_run` 在 `prepare_drive` **之前**登记取消令牌，并用 `DriveGuard` 托底。因此 `Running` 提交之后不存在「取消到了但没有令牌可触发」的窗口。同一进程内同一 Run 已有登记时，第二个 `drive_run` 返回 `Invalid("run is already being driven")`，不覆盖前一个。
- `request_cancel` 按状态处理：`Running` → 同事务写 `Cancelling` 与 `run.cancelling`，再触发令牌；`Accepted` 且本进程已登记令牌（正在进入 `Running`）→ 只触发令牌，Engine 第一次检查就交回 `Cancelled`；`Accepted` 但本进程没有 drive → `Invalid("run has not started")`；`Cancelling` 或终态 → 不改库，仍触发本进程令牌，让可能已被别处收尸的 drive 醒来。
- 先提交的状态优先：库中是 `Cancelling` 时，`finish_drive` 把任意 Engine 结果（含 `Completed`）收成 `Cancelled`，报告降为 partial。库中已是终态（如扫描写的 `host_restart`）时不再用 Engine 结果改写。
- 取消期间 Engine 收尾提交的观测（`model.call_completed` / `tool.call_completed`）在 `Cancelling` 下**仍被接受**，被打断那次调用的用量不丢。`reserve` 只在 `Running` 发 lease，取消后的 Engine 不该再开新调用。

**租约与回收**

- TTL 60s，运行中每 20s 续期为 `now+60s`。续期**直到 `engine.run` 返回才停**，取消触发不会停掉续期：Engine 收尾可能很慢，Run 仍需要租约。续期任务由 `DriveGuard` 管理，Engine panic 或 `drive_run` 的 future 被丢弃时随之停止并撤销令牌登记。续期出错（如 SQLite busy）记日志并在下个周期重试；Run 已终态或被收尸（`Invalid`）才停止。
- 扫描挂在 `RunHost::open`：任一 `flux` 子命令（含只读 `status`）都会回收租约已过期的 `Running` / `Cancelling` Attempt → `Orphaned`，Run → `Failed(host_restart)`，并触发本进程令牌以免 drive 挂死。
- 候选列表是事务外的旧快照，所以 `orphan_drive` 在 `IMMEDIATE` 事务里**重新读 Attempt** 再判断：已终态、Attempt 不再是 `Running`、或租约已被续期，都不改库。这避免误杀刚续期的活进程。
- 回收失败不阻断 `open`：单个 Run 失败只记日志并继续后面的 Run；扫描整体失败也只记日志，只读命令仍能打开库，下次 `open` 再试。

**CLI 退出码与启动顺序**

- `flux run` 仅当 Run 以 `Completed` 结束时退出 0，`Failed` / `Cancelled` 一律非零；已终态 Run 的幂等路径同样。stdout 的状态行照常打印。
- Engine 在 `create_run` 之前构造，缺 `DEEPSEEK_API_KEY` 在任何落库之前失败，不留下 `Accepted` 的孤儿 Run。
- 复用已有 `request_id` 且 Run 未终态时继续驱动它：库里的工作区、任务和预算保持不变，CLI 在 stderr 提示；只有 `--engine` 采用本次的。

**已知限制**：Host 的 `open` 扫描、`finish_drive` 等入口仍是同步阻塞调用（SQLite 写锁上带 `busy_timeout` 等待）。续期与 CLI 的 `request_cancel` 已放进阻塞线程，其余未异步化。

### 5. CLI 数据目录

- 环境变量 `FLUXAXIOM_DATA_DIR` 优先
- 否则：Windows 为 `%LOCALAPPDATA%\FluxAxiom`；其它平台为 `$XDG_DATA_HOME/fluxaxiom`（未设 XDG 时为 `~/.local/share/fluxaxiom`）
- `flux run --data-dir` 可覆盖单次调用

Host 数据与被分析的 `--workspace` 分离；不把库写进用户仓库。

## 后果

- 创建后工作区被删再 drive：用户看到 `Failed(engine_error)`，可用新 `request_id` 重试；旧 `request_id` 幂等仍返回该失败 Run。对已经 `Running` 或已终态的 Run 再 drive，只得到 `run is not startable`，库中状态不变。
- A3a 验收不声称 Ctrl-C / `host_restart`；A3b 验收见 `run-host/tests/interrupt.rs` 与 `apps/cli/tests/cli_fake.rs`。
- Ctrl-C 之后 Engine 收尾再慢也不会被误收成 `host_restart`，代价是进程若在收尾中被强杀，Run 要等租约（最长 60s）过期后才被下一次 `open` 回收。
- ADR-022「创建之后目录消失停在 Accepted」的缺口由本决定关闭。

## 复审触发器

- 允许同 Run 续跑或显式 retry 命令
- 需要把 prepare 失败从 `engine_error` 拆成更细的 `terminal_reason`
- 租约 TTL / 续期间隔需要随真实工具耗时或慢收尾调整
- Host API 异步化（回收扫描与收尾目前是同步阻塞调用）
