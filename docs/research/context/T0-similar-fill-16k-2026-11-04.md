# T0 记录：第 5 次请求的 16K 相似干扰基线

核对日期：2026-11-04。只做任务 `mini-inventory-k5` 在约 16K 上的完整版对完整版。填充是邻近路径，不是重复汉字。不接 Engine，不写 Run。

## 方法

测试：`crates/engine-custom/tests/context_baseline.rs` 的 `similar_fill_16k_baseline_for_mini_inventory_k5`。命令是 `cargo test -p fluxaxiom-engine-custom --test context_baseline similar_fill_16k_baseline_for_mini_inventory_k5 -- --ignored --exact --nocapture`。进程从 `.env` 读到 `DEEPSEEK_API_KEY` 后拨号，输出里没有密钥。

底稿是 `T0-unfilled-k5-2026-11-04.md` 的第 5 次请求，16 条消息不改。只在第一条 user 末尾重复这一行：

`src/inventory.rs src/storage.rs src/lib.rs src/main.rs README.md`

这一行不含 `tests/inventory.rs`。先用 40 次重复测得 `prompt_tokens` 为 3684，再按未填充的 2964 token 换算到约 16000，正式采样用 724 次重复，共 47060 个字符。校准那一次不计入下面的五次。模型、`reasoning_effort`、`max_tokens` 和签名规则与未填充基线相同。采样次数 N 为 5。

## 结果

五次都完成。服务端 `prompt_tokens` 都是 15996。结束原因都是 `tool_calls`，都没有结束。

| 采样 | 工具调用 | 路径 |
| --- | --- | --- |
| 1–5 | `read_file {path:"tests/inventory.rs",start_line:1}` | `tests/inventory.rs` |

不同签名 1 个。10 对全部相同，两两一致率是 1。众数匹配的方差是 0。签名与未填充基线和 16K 无关填充基线相同。

这一档的相似干扰基线稳定。64K 以及更长的填充这次没有测。
