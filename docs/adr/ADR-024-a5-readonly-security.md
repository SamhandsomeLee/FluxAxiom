# ADR-024：A5 只读安全边界与负向证据

- 状态：已采用（A5-01 / A5-02 / A5-03 落地）
- 日期：2026-10-27
- 依据：P0 执行方案 v1.1 §4.13、§4.13.1、§4.13.2、§12；V0.3 §14.1 A5 保证边界；ADR-022；boundaries.mdc；AGENTS.md

## 背景

A0–A4b 已形成 Run / Event / Trace / Sample 闭环。P0a 还缺「只读安全」的负向证据：路径越界、符号链接逃逸、忽略与凭据文件、以及模型输出不能改权限。执行方案把 A5 拆成三个 PR，并声明不得把通过写成 OS 沙箱或 Prompt Injection 免疫。

现有 `WorkspaceRoot::resolve` 是 `canonicalize` 后再由工具 `open`。二者之间存在 TOCTOU 窗口。并发竞态难以在 CI 上稳定复现，需要一条**失败受控的静态用例**把「路径身份在校验后被换成根外链接」写成可重复断言，同时在 ADR 里划清残余风险。

## 决定

1. **A5 证明范围（P0a）**  
   模型只能调用已列出的只读工具（`list_files` / `read_file` / `search_code`）；路径受 `WorkspaceRoot` 与忽略规则约束；敏感标记与假密钥不得意外进入工具结果、Event、默认 CLI 输出或 Sample。不证明：强 OS 沙箱、并发文件系统竞态已关闭、Prompt Injection 不可能发生。

2. **不可信输入**  
   模型 Tool Call、仓库内 `README` / `AGENTS.md`、工作区其它文本均不是权限来源。权限只来自 Host 已 `open` 的授权根与代码里写死的工具表。

3. **不新建 security crate**  
   负向测试落在现有 `engine-api`（`WorkspaceRoot`）、`engine-custom`（工具与 Loop）、`apps/cli` / `run-host` 测试中。A5-01 入口：`workspace.rs`、`tools.rs`、`tests/tools.rs`。

4. **三个 PR 切片**  
   - **A5-01**（本 ADR 首次落地）：工具层路径 / 忽略 / 符号链接 / 非法参数回归；一条静态竞态用例。依赖 symlink 能力的测试默认显式标为 `ignored`，不计入通过；CI 单独运行，建链失败以 `UNVERIFIED` 和 OS 错误导致验收失败。  
   - **A5-02**（已落地证据）：`engine-custom/tests/loop.rs` 用 FakeChat + 内存 HostPort；模型请求 `write_file` / `shell` / 根外 `read_file` 时无副作用；错误进工具结果与 observe；`AGENTS.md` 诱导不提升权限；blob / 后续请求不含根外密钥标记。  
   - **A5-03**（已落地证据）：`apps/cli/tests/cli_privacy.rs`；Fake Run 扫描 `run` / `events` / `status` / `trace` / `sample export` 默认输出、`events.payload` 与 blob 语料；合成 `.env` 假密钥与 `HIDDEN_ENV_SECRET` 不得出现；Event 无 `reasoning_content` 键、默认 CLI 无思维链原文；`--show-request --include-reasoning` 出原文算有意披露。

5. **静态竞态用例（失败受控）**  
   不用并发线程、不加 `resolve`/`open` 测试缝。步骤固定为：  
   (1) 根内写入 `bait.txt`（正文合成标记 `INSIDE`）；  
   (2) `resolve("bait.txt")` 成功；  
   (3) 删除该文件，在同名路径建立指向根外、正文含 `OUTSIDE_SECRET` 的文件符号链接；  
   (4) 再调 `read_file(..., "bait.txt", 1)`，必须得到 `OutsideRoot`（或等价受控工具错误），结果与错误文本不得含 `OUTSIDE_SECRET`；  
   (5) 根外目标文件字节在测试前后不变。  
   这证明「校验后路径身份若变成根外链接，当前实现须拒绝」。**不**证明单次调用内部并发 TOCTOU 已关闭；该窗口仍为已知残余，留给 P0b / OS 实证（见 ADR-015 时机）。

6. **测试约束**  
   临时工作区、Fake Provider、合成敏感标记；不碰真实用户凭据与真实仓库。仅靠 Run `Completed` 不算安全通过。记录运行 OS、跳过用例与残余风险。

7. **审查补强（2026-10-27）**  
   - 列表与直接读取的可见性判断从授权根遍历到目标分支，继承根内各层 `.gitignore` / `.ignore`，保留锚点与子层否定规则的语义；不读取根外规则，不递归无关兄弟子树。回归同时覆盖列表、读取和搜索，使用合成敏感标记。  
   - Host Git 采集按 ADR-023 强制关闭 fsmonitor；临时仓库测试先证明合成 hook 可执行，再证明采集不触发它，且 index / config 字节不变。该措施不等同于通用进程沙箱。  
   - 工具层三项 symlink 测试与 Host 根替换测试共四项，默认显示为 `ignored`；必须另跑 `cargo test -p fluxaxiom-engine-custom --test tools -- --ignored` 和 `cargo test -p fluxaxiom-run-host --test drive -- --ignored`。仅默认测试通过不能作为链接边界验收证据。

## 后果

- A5-01/02/03 证据已齐；宣称「A5 验收通过」仍须对照执行方案 Done 清单与残余风险（含 TOCTOU、平台 symlink 能力门禁），不得写成 OS 沙箱或 Prompt Injection 免疫。  
- 静态竞态用例依赖创建符号链接的能力；默认不执行且明确统计为 ignored，显式能力验收时 Windows 无权限会失败并标注未验证。  
- 若将来要关闭单次调用内 TOCTOU，须另开改动（例如 openat / `O_NOFOLLOW` / 重新校验），并更新本 ADR 的「残余风险」条款。

## 复审触发器

- A5-02 / A5-03 验收完成或结论变更。  
- 引入 OS 级沙箱或改变 `canonicalize → open` 路径。  
- 第二套工作区标识或跨用户共享 Host（与 ADR-022 联动）。  
- 调整 symlink 能力验收的必需平台或执行策略。
