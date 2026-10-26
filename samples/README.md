# samples

真实模型分析与相关测试的样本。目录结构：

| 路径 | 内容 |
|---|---|
| `workspaces/<name>/` | 被分析的小型工作区，作为 `flux run --workspace` 的目标 |
| `cases/<name>.md` | 对应工作区的提示词、标准答案、通过标准和运行记录 |

标准答案只放在 `cases/`，不放进 `workspaces/`，否则模型能读到。

`workspaces/` 下的项目不是仓库 Cargo workspace 的成员（根 `Cargo.toml` 已 `exclude`）。要单独验证样本本身能编译：

```bash
cargo test --manifest-path samples/workspaces/mini-inventory/Cargo.toml
```

## 运行

先按根目录 README 配好 `.env`，然后：

```bash
cargo run -p fluxaxiom-cli -- run --engine deepseek \
  --workspace samples/workspaces/mini-inventory \
  --prompt "分析这个仓库：说明入口、模块划分和数据怎么存储，并指出你认为最值得修复的一个缺陷。"
```

这会把 `workspaces/mini-inventory` 的内容发送到 DeepSeek。样本内容不得含真实密钥或私人数据。

## 新增样本

1. 在 `workspaces/` 建目录，保持小而确定，里面有可核对的事实和至少一个已知缺陷。
2. 在 `cases/` 写同名文件，格式参照 `mini-inventory.md`。
3. 不要提交含密钥、`.env` 或个人数据的内容。
