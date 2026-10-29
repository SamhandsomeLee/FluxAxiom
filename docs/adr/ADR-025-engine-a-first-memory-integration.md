# ADR-025：Engine A 优先深化与独立 Agent Memory Engine 集成

**日期**：2026-10-29　**状态**：已采用（规划与边界决定；Memory 接口待核验）　**源码基线**：FluxAxiom `main@2aed418`

## 决策

1. **停止 Engine B / Codex app-server 实验**。B1、B5-T/H/D、B6-B、B2/E-001 退出正式 P0/P1 路线，不建立 `engine-codex`，旧 V0.3 第 8–9、16 章只作为历史研究。
2. **专注自研 Engine A**，以 `engine-custom` 的 Rust Loop 为唯一正式 Agent Engine；保留现有 `EngineAdapter` / `HostPort` 作为清晰的测试、Provider 与 Host 隔离边界，不为了第二引擎建空抽象。
3. **新阶段门禁**：G-A 真实验收 → EA-01～04 和 EA-G（Loop/Context/工具/可靠性与质量）→ M0 核对独立 Agent Memory Engine 真实源码/API → M1 默认关闭的只读召回 → M-EVAL 对照 → 有条件 M2/M3 记忆写回与联合回归。G-A 仍要求 5 个实际使用日、至少 10 次真实任务；不得提前让 Memory 改变 G-A 基线。
4. **保留 B3/B4/B6-A**：Host-owned Effect、一次性审批、沙箱、取消和 Unknown 对账仍作为 Engine A 的安全写操作支线，但不再为 Engine B 服务。G-A 和 OS 沙箱实机证据未完成时不实施不可信写入；M1 只读召回不以 B3 为前置。
5. **Memory 独立部署、独立存储、共同迭代**。Engine A 仅通过薄 `MemoryClient` 调 API，不直接访问外部 LanceDB；Memory 服务不写 FluxAxiom SQLite，也不替代 Run/Attempt/Event/Blob 的权威事实。RunHost 不执行 Memory 排序/提取策略。

## 当前证据与待核验项目

FluxAxiom 当前有 DeepSeek/Fake ChatTransport、只读 Agent Loop、三个受限读取工具、HostPort 的预算/取消/观测、SQLite durable events、Blob、Trace/Verify、Sample/Feedback；`propose_effect`、`request_input` 仍为 `Unsupported`。本次不实现 MemoryClient 或 Effect。

此前讨论的 Memory 工程名 `agent-memory-engine-stage9`，历史架构为 FastAPI → MemoryService → Domain/Repository → Retrieval Pipeline → Adapter → LanceDB/Qwen；曾出现 `POST /v1/memories/search`、`POST /v1/memories`、EmbeddingProvider、Reranker 与混合检索/RRF。**本次未取得其最新源码**：以上均为 M0 待核验线索，不代表现行接口、认证、数据模型已验证。M0 必须读实际项目源码，产出版本化 HTTP Schema、Auth/scope、错误、限流/超时、数据保留与删除语义及 Mock fixtures。

## 接口与权限边界

- **RunHost**：独占 Run/Attempt/Artifact/事件与预算/取消等事实；Memory 数据不可改写 Host 权威状态。
- **Engine A**：Agent Loop 和 Context 组装；记忆召回只作为带来源的低信任输入，不能覆盖 system 指令、工具白名单、`WorkspaceRoot` 或批准策略。
- **MemoryClient（M1 候选接口）**：带可信 principal/project scope 的只读查询、最大条目/字节/token、超时、来源/版本、内容过滤与明确的失败降级。默认关闭；允许降级的只读分析可以在服务故障时无记忆继续，未授权/跨项目时必须拒绝请求。绝不默认上传 Secret、仓库全文和 reasoning。
- **Memory Engine**：独立维护语义记忆、检索、候选和删除；独立演进评测，FluxAxiom 不直接碰其 Repository/向量数据库。
- **M2 Write Path**：只能 Candidate → 去敏/去重 → 明确授权 → 持久幂等提交 → 回执/Unknown 对账 → 可删除/撤销。记忆写回是**外部副作用**，不能由模型 Tool Call 绕过治理；没有安全与故障证据就保持 M1 只读。

未来如需新增 Memory Event，必须扩版本化 Contract，存最小引用/摘要，不把记忆正文、Prompt、secret 或 reasoning 混进 durable events。SQLite 与外部 Memory API 不存在跨系统原子事务；不能宣称 exactly-once。

## 退出标准

EA-G：同一模型/仓库 commit/任务/预算可重现的事实正确率、证据引用、Token/延迟/成本、工具次数、人工返工、错误/取消/恢复与安全回归。`Completed` 和 `trace --verify=consistent` 不是质量正确证明。

M0/M1：API/scope/授权核验、开关两态、过滤与注入、跨项目、超时/断网/陈旧记忆、降级和预算测试。M-EVAL 用 `none` / `manual` / `memory-recall` 固定任务比较净增益；召回命中率不能取代任务质量。M2/M3 需用户同意、去重/幂等/回执对账、删除/撤销与双系统故障注入通过。

**实施 PR 分解**：[Engine A 与 Agent Memory Engine 协同演进方案](../development/EngineA与AgentMemoryEngine演进方案.md)。
