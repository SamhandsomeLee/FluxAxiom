# ADR-022：授权工作区根的交接

- 状态：已采用
- 日期：2026-10-23
- 依据：P0 执行方案 v1.1 §4.4、§4.5、§4.7.3；V0.3 §4.2（`workspace_ref` 的说明）；boundaries.mdc；ADR-019 第 4 条

## 背景

只读工具只能读 Host 已授权的工作区。工具入口是 `WorkspaceRoot`，不接受模型给出的绝对路径。创建 Run 时库存的 `workspace_ref` 原先只是非空字符串。CLI 示例给的是文件系统路径，架构样例给的是 `ws_readonly_01` 这类标识。两份文档都没有写 ref 如何变成真实目录。

## 决定

1. P0a 的 `workspace_ref` 存调用方给出的目录路径。不另建标识到路径的登记表。
2. `WorkspaceRoot` 放在 `engine-api`。`run-host` 与 `engine-custom` 都依赖它，互不依赖。
3. Host 在 `prepare_drive` 里用 `WorkspaceRoot::open(workspace_ref)` 打开目录，再放入 `EngineRunRequest`。打开失败时不进入 `Running`，与任务 blob 缺失同一类处理。
4. Engine 不读库，不自己解析 `workspace_ref`。模型路径仍只经 `WorkspaceRoot::resolve`。

## 后果

驱动测试必须给出真实存在的目录。长期若需要稳定的 `ws_*` 标识，另写 ADR 增加登记，不改工具入口。

## 复审触发器

出现第二套工作区标识、跨进程必须解析同一路径但路径已移动，或 CLI 需要与 Host 解耦的登记服务。

## 补充（2026-10-23）

### 与 V0.3 §4.2 的关系：有意收窄

> `workspace_ref` 是 Host 预先验证过的资源标识；不得直接把 LLM 输出当绝对主机路径。
> —— V0.3 §4.2

本 ADR 把 P0a 的 `workspace_ref` 定为目录路径，这比原文的「资源标识」窄，是有意的：P0a 单进程、单用户，没有第二个需要登记的工作区来源。原文后半句仍然成立：路径来自调用方（CLI 的 `--workspace`），不来自模型；模型只能给相对路径，由 `WorkspaceRoot::resolve` 收进根内。契约示例 `contracts/examples/run.json` 里的 `ws_readonly_01` 是文档样例，与 P0a 实现不一致；它在出现登记表之前不更新，避免让契约看起来已经支持标识解析。

### engine-api 现在依赖文件系统

`WorkspaceRoot::open` 调用 `std::fs::canonicalize`，所以 `engine-api` 不再是纯类型 crate。这符合 ADR-019 第 4 条（放「两个 Engine 实现都要使用的类型」），但划一条线：

- 放进 `engine-api` 的只有 `WorkspaceRoot` 与 `WorkspaceError`，即「授权根」和「根内路径解析」。
- `list_files`、`read_file`、`search_code`、忽略规则、分页和渲染留在 `engine-custom`。它们是 Engine A 的实现，不是端口。
- `engine-api` 不引入 `ignore`、`grep-*` 等遍历依赖。`tempfile` 只作为 dev 依赖。
- 以后别的 Engine（例如 Codex Adapter）需要的是同一个「授权根」，而不是同一套工具。

### 事件不带路径，快照带

`run.accepted` 与 `run.started` 的载荷是空的（`EmptyPayload`，见 `crates/agent-contract/src/event.rs`、`crates/run-host/src/events.rs`），所以 `workspace_ref` 不进 durable event，符合 `boundaries.mdc`「事件只存引用和摘要」。

但 `workspace_ref` 在 Run 记录和 `run-snapshot` 里，值是调用方给的路径，可能含用户名或家目录结构。A3 的 CLI `status` 或任何导出样本的命令输出快照时，要决定是否原样显示。另外 `WorkspaceRoot` 的 `Debug` 会打印规范化后的绝对路径，写日志时同理。

### 已知缺口

1. **路径在创建时没有规范化。** `create_run` 原样存下字符串，`prepare_drive` 才调用 `WorkspaceRoot::open`。传相对路径时，它按**驱动那一刻的进程当前目录**解析。如果创建 Run 和驱动 Run 不在同一个目录，打开的会是另一个目录，授权发生在创建时，读取却对着别的目录。A3 之前要么让 `create_run` 就 `open` 一次并存规范化后的绝对路径，要么让 CLI 保证只传绝对路径。
2. **打不开时 Run 停在 `Accepted`。** 工作区目录打不开会让 `prepare_drive` 失败，Run 没有终态也没有事件。同一个 `request_id` 重新提交会返回旧 Run，连同旧的错误路径，所以用修正后的路径重试也无效。这一条已写入执行方案 §4.7.4，A3 开始前必须决定处理方式。
3. **驱动时才打开，打开后到读取之间仍有窗口。** 规范化只在 `open` 与每次 `resolve` 时做一次，随后 `File::open` 用的是路径而不是句柄。符号链接在这个窗口里被替换，不会被发现。执行方案已把路径 TOCTOU 归入 B6，P0a 不处理。

## 复审触发器（追加）

- `engine-api` 里出现第二个需要文件系统的类型，或开始引入遍历、搜索类依赖。
- 要把规范化路径写进 durable event 或对外导出的样本。

