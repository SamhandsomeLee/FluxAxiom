# T0 记录：第 5 次请求的更长填充基线

核对日期：2026-11-04。补完任务 `mini-inventory-k5` 在 64K、256K、512K 上的完整版对完整版。16K 已有记录，这次不重测。不接 Engine，不写 Run。

## 方法

测试：`crates/engine-custom/tests/context_baseline.rs` 的 `longer_fill_baselines_for_mini_inventory_k5`。命令是 `cargo test -p fluxaxiom-engine-custom --test context_baseline longer_fill_baselines_for_mini_inventory_k5 -- --ignored --exact --nocapture`。进程从 `.env` 读到 `DEEPSEEK_API_KEY` 后拨号，输出里没有密钥。

底稿仍是第 5 次请求，16 条消息不改。无关填充是在第一条 user 末尾追加「中」，字数等于目标 token 减去未填充的 2964。相似干扰沿用 16K 那一行邻近路径，按每次重复 18 token 换算次数。两种填充各采样 5 次。模型、`reasoning_effort`、`max_tokens` 和签名规则与未填充基线相同。

为让 512K 的预填充不在第一个数据块之前被切断，客户端两个数据块之间的空闲上限从 60 秒改为 180 秒。这次六档都在该上限内完成。

## 结果

六档都完成。每次都是 `tool_calls`，都没有结束。签名都是 `read_file {path:"tests/inventory.rs",start_line:1}`，路径 `tests/inventory.rs`。每档 10 对全部相同，两两一致率是 1，众数匹配的方差是 0。

| 填充 | 目标 | 规模 | 服务端 prompt_tokens |
| --- | ---: | --- | ---: |
| 无关「中」 | 64K | 61036 字 | 64000 |
| 相似路径 | 64K | 3391 次重复 | 64002 |
| 无关「中」 | 256K | 253036 字 | 256000 |
| 相似路径 | 256K | 14058 次重复 | 256008 |
| 无关「中」 | 512K | 509036 字 | 512000 |
| 相似路径 | 512K | 28280 次重复 | 512004 |

`mini-inventory-k5` 在未填充、16K、64K、256K、512K 上，两种填充的基线都稳定，且下一步与未填充基线相同。`mini-inventory-k1` 仍因未填充基线不一致而剔除。
