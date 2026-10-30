# ADR-026：上下文管理独立模块与可重建编辑

- 状态：已采用（规划与边界决定；实现按 EA-CTX 分期）
- 日期：2026-10-30
- 依据：[EA-CTX 上下文压缩专题执行方案](../development/上下文压缩专题执行方案（EA-CTX）.md)；ADR-020 第 3/4/6 条；ADR-023 第 2 条；ADR-025；执行方案 §4.12.1、§6；boundaries.mdc；scope.mdc

## 背景

EA-02 要求裁剪历史与工具返回，但 ADR-020 第 6 条规定“历史只追加”，执行方案 §4.12.1 规则 1 把它落成逐字前缀检查。任何压缩都会被 `trace --verify` 判为 `inconsistent`。

现有上下文约束只是副作用：默认预算 8 次模型、32 次工具，同时兼管成本与上下文，数值按单一模型拍定。换模型、放大预算或接入记忆召回后就失效。

上下文压缩的技术仍在快速变化（遮蔽、摘要、折叠、厂商服务端压缩、训练模型自管上下文）。本项目要求它作为独立模块迭代，尽量不与现有代码耦合，为以后引入其他技术留余地。

## 决定

1. **上下文管理是独立 crate。** 目录 `crates/context`，包名 `fluxaxiom-context`。只依赖 `agent-contract` 的基础类型，不依赖 `engine-api`、`engine-custom`、`run-host`。`engine-custom` 依赖它；Host 不知道它存在，只保存 blob 和事件里的引用。P1 起即有使用点（估算与硬上限），不属于 scope.mdc 所禁止的空 crate。

2. **三层分离，阈值不手写。**
   - **模型档案 `ModelProfile`**：“模型 × 服务”的客观事实。每份一个数据文件，归上下文模块，编译时嵌入，运行时不猜。每个字段带来源（文档链接与核对日期，或实测记录）与可信度（文档 / 实测 / 假设 / 未知）；未知字段取保守值并记录所取的值。版本为 `profile_version` 加文件 hash。
   - **上下文策略 `ContextStrategy`**：取舍，按有序策略链执行。版本为 `context_strategy`（如 `mask@1`）。
   - **派生规格 `ContextSpec`**：硬上限、软触发、目标、保护范围，由前两层在每次调用前计算并记录。
   - 调用次数预算只管成本与失控。上下文模块不读、也不改 `Budget`。

3. **接口。** 模块对外只有两个入口：

   ```rust
   impl ContextManager {
       /// 估算、按链执行策略、检查硬上限。
       pub fn prepare(&self, t: &Transcript) -> Result<Prepared, ContextExhausted>;
       /// 校验入口：由事实与记录确定性地重建视图。
       pub fn replay(t: &Transcript, r: &ContextRecord) -> Result<View, ReplayError>;
   }

   pub trait ContextStrategy: Send + Sync {
       fn id(&self) -> &str;
       fn apply(&self, view: &mut View, spec: &ContextSpec, record: &mut ContextRecord);
       fn replay(&self, view: &mut View, record: &ContextRecord) -> Result<(), ReplayError>;
   }
   ```

   `Transcript` 与 Provider 协议无关，由已提交的事实构造：system、user、各轮 assistant（文本、思维链、工具调用）、工具结果（调用 ID、工具名、参数、`result_ref`、字节数）、各轮 usage。`View` 是发给模型的视图。Chat Completions 序列化留在 `engine-custom`；思维链回传、调用配对等协议约束由档案的能力字段表达。

4. **不变量从请求层移到事实层。** 事件与 blob 只追加，原始内容不因压缩而删除或改写（保留期限仍按样本保留策略）。每次请求是事实与 context 记录的确定函数：

   `req_k.messages == serialize(replay(T_k, R_k))`

   `T_k` 是由第 1 至 k−1 轮事实构造的 `Transcript`，`R_k` 是第 k 次调用的 context 记录。本条修订 ADR-020 第 6 条的“历史只追加”。恒等策略下 `replay(T) = T`，上式与原规则 1 等价。

5. **校验器不理解编辑。** `engine-custom::rebuild` 由 blob 构造 `Transcript`，调用 `replay`，与请求逐字节比较。执行方案 §4.12.1 的规则 1、3 并入此式，规则 2、4、5、6、7 不变。记录中的策略标识在当前代码中不存在，或记录标记 `replayable = false` 时，结论为 `unverifiable`，不猜。

6. **context 记录是独立 blob。** 结构归上下文模块所有，带自己的 `schema_version`，不进入 `agent-contract`。内容只有引用与数值：`profile_version`、`profile_hash`、策略链标识、`spec`、`estimate`（数值、来源 `usage` / `estimated`、锚点轮次）、`edits[]`（各策略自定义）、`replayable`。不含 prompt、工具正文或 `reasoning_content`。

7. **事件与观测只加两个可选字段。** `EngineObservation::ModelCall` 与 `model.call_completed` 增加 `context_ref`、`context_strategy`。旧事件缺字段读出为 `None`，按恒等策略校验。

8. **上下文耗尽是独立的终态原因。** `engine-api` 的 `FailureReason` 与 `agent-contract` 的 `TerminalReason` 各新增 `ContextExhausted`（线上为 `context_exhausted`）。不复用 `BudgetExhausted`，避免把上下文和调用预算重新绑在一起。具体是哪条策略、估算多少，写在 context 记录里。可带部分 Artifact，与预算耗尽一致。

9. **版本号分开递增。** `prompt_layout_version` 只覆盖 system 与工具 schema；上下文策略变化递增 `context_strategy`；档案变化递增 `profile_version`。单次编辑是事实，记在 `edits[]`，不递增任何版本。

10. **新技术以策略接入。** 清思维链、摘要、折叠、记忆召回注入、厂商原生压缩都实现为新的 `ContextStrategy` 加入链中。输出不确定的策略（如摘要）把输出存为 blob、在记录里引用，重放仍是确定的；无法重放的策略标记 `replayable = false`。新增策略不改动第 11 条所列接触点。

11. **对现有代码的固定接触点（一次性改动，与具体技术无关）：**

    | 位置 | 改动 |
    | --- | --- |
    | `engine-custom` Loop | 每次模型调用前 `prepare`，序列化视图，写记录 blob；耗尽时返回 `ContextExhausted` |
    | `engine-api` | `FailureReason::ContextExhausted`；`ModelCall` 观测两个可选字段 |
    | `agent-contract` | `TerminalReason::ContextExhausted`；`ModelCallCompletedPayload` 两个可选字段；`event.schema.json` 与样例 |
    | `run-host` | 落库新增字段；新终态原因映射 |
    | `engine-custom::rebuild` | 规则 1、3 改为与 `replay` 结果逐字节比较 |
    | CLI `trace` / `sample export` | 显示 `context_strategy` 与估算值；样本保留两个字段 |

## 后果

- 每次模型调用多写一个小 blob（context 记录）。
- 契约有一次性变更；旧二进制读不了带新字段的事件，与 `workspace_revision` 的兼容方式相同。
- `rebuild` 依赖上下文模块的 `replay`；校验正确性依赖各策略 `replay` 的实现，策略须自带篡改用例（改存根、动保护窗口、无记录删除、有记录无变化）。
- 现有样本按恒等策略校验，结论不变。

## 复审触发器

- 第二种 Provider 协议需要扩展 `Transcript` 的条目种类。
- 采用厂商不透明的服务端压缩，或需要在请求里写入无法从事实重建的内容。
- 某策略需要 `Transcript` 以外的 Host 事实（如 Memory 召回的来源与授权）。
- 引入离线回放（用记录的响应驱动 Engine 重跑）。
