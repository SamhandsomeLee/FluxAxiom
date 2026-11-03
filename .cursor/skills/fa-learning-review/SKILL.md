---
name: fa-learning-review
description: "在 FluxAxiom 开发任务后复盘 Rust、架构、调试与测试知识，训练迁移能力"
disable-model-invocation: true
---

# Learning Review · 能力复盘

## Purpose
衡量能否**独立迁移**，而不仅是完成一次实现；复盘要短、具体、可证实。默认不写持久文件。

## Input / Evidence
用户指定的任务、自己完成的 diff、经核实的测试结果与本次讨论。无法看到上一轮代码或日志时主动说明信息缺口，不捏造“已掌握”或学习进度。

## Workflow
1. 用三句话归纳“实现了什么 / 为什么如此设计 / 如何证明正确”。
2. 向用户提出最多 2 个有价值的理解检验：一个 Rust 机制，一个架构或失败场景。用户不想答时直接提供参考解释。
3. 分离“已能独立说明”“需要回看提示”“仍未知”；不要按聊天长度或复制代码量打分。
4. 设计一个相似但不相同的微型迁移练习，例如将 Context Strategy 的设计取舍迁移到 Retry Policy。
5. 如用户明确要求保存笔记，再以 `assets/learning-note.md` 为模板生成短文；未经授权不修改仓库。

## Output Contract
本次成果（事实）→ 关键 2–3 个知识点 → 1 个可迁移挑战 → 下一次独立实践建议。

## Restrictions
不夸大个人能力进步、不机械考试、不把自评当测试结果；不要记录密钥、prompt、敏感运行内容。
