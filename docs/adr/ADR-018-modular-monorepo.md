# ADR-018：模块化 Monorepo 与项目名

- 状态：已采用
- 日期：2026-10-20
- 依据：V0.3 §0.2.1、§13.1、§13.8、附录 D；P0 执行方案 v1.1 §8.2

## 背景

Host、Engine 与 Contract 的边界仍要靠 E-001 和故障注入来检验。现在把 Memory、GPU、Engine 拆成多个仓库，会把尚未稳定的代码边界变成跨仓库版本和联调成本。

V0.3 使用过工作代号 AgentOS。架构文档把 FluxAxiom 标为首选名称，并把 `FluxForgeLab/FluxAxiom` 标为拟定路径，不代表仓库或商标已经确认。

## 决定

1. 实施名称定为 **FluxAxiom**。AgentOS 指同一项目。本决定不构成商标或名称可用性结论。
2. 使用一个 Rust Workspace 主导的模块化 Monorepo。代码边界、编译发布边界和部署边界分开：依赖规则约束代码，Cargo Workspace 成员决定编译单元，进程只在真实需要时拆分。独立进程不等于独立仓库。
3. 正式运行路径不得依赖 `research/` 中尚未晋升的实验。实验经测试、评测和 ADR/PR 后才能进入正式模块，失败时保留报告即可。
4. 目录按使用点创建。P0a 不预建没有引用的 crate 或空目录；P0b 再增量引入 Engine B 与 Gateway。
5. 当前 Git 远端是 `https://github.com/SamhandsomeLee/FluxAxiom.git`。`FluxForgeLab/FluxAxiom` 仍只是 V0.3 中的拟定路径。

## 后果

跨模块修改留在同一个 PR 里审查、测试和回退。`main` 保持可构建。特性分支和 worktree 使用短生命周期，不长期维持 memory、multi-agent、gpu 等大分支。

依赖方向和 `engine-api` 的提取由 ADR-019 在 A1.1 记录。DeepSeek Provider 由 ADR-020 在 A0 记录。

## 复审触发器

出现以下任一情况时重新评估是否拆仓：

- 某个模块需要独立发布节奏、独立权限或独立服务部署；
- CI 负担、接口变更成本或跨模块协作成本持续上升；
- 拆仓后的集成成本已经可以承受。

不因为模块名称本身拆仓。
