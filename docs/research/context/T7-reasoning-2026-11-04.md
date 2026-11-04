# T7 记录：清空第 5 次请求的旧轮思维链

核对日期：2026-11-04。只把任务 `mini-inventory-k5` 里已有 assistant 消息的 `reasoning_content` 改成空字符串。键保留。不改 `content` 和 `tool_calls`。不接 Engine，不写 Run。档案不改。`mini-inventory-k1` 不进入。

## 方法

测试：`crates/engine-custom/tests/context_baseline.rs` 的 `cleared_reasoning_for_mini_inventory_k5`。命令是 `cargo test -p fluxaxiom-engine-custom --test context_baseline cleared_reasoning_for_mini_inventory_k5 -- --ignored --exact --nocapture`。进程从 `.env` 读到 `DEEPSEEK_API_KEY` 后拨号，输出里没有密钥，也没有思维链。

底稿是 `T0-unfilled-k5-2026-11-04.md` 的第 5 次请求，blob 是 `sha256:005b4e4c7a92b174f62b889d01ffa9c33815af4a880e46ab329fa26e3aebb7cc`。这条请求里的 assistant 消息都算旧轮。模型、`reasoning_effort`、`max_tokens` 和工具声明沿用 blob，不另改。签名规则与未填充基线相同。

先发一次。没有完成就停，不再采样。这次第一次就完成，并把它算进 5 次。决策等价率是这 5 次里签名等于未填充基线的比例。未填充基线的两两一致率是 1，损失是 1 减去决策等价率。

## 结果

清空了 4 条 assistant 的思维链。五次都完成，没有提前停下。结束原因都是 `tool_calls`，都没有结束。服务端 `prompt_tokens` 都是 2959。当时那次未清空请求的 `prompt_tokens` 是 2964。

| 采样 | 工具调用 | 路径 | 等于基线 |
| --- | --- | --- | --- |
| 1、3、4、5 | `read_file {path:"tests/inventory.rs",start_line:1}` | `tests/inventory.rs` | 是 |
| 2 | `read_file {path:"tests/inventory.rs",start_line:1}`；`search_code {query:"overflow\|checked_\|saturating"}` | `tests/inventory.rs` | 否 |

5 次里 4 次等于未填充基线，决策等价率是 0.8，损失是 0.2。这 5 次自身的两两一致率是 0.6，众数匹配的方差是 0.2。

## 结论

这条带工具的请求接受空字符串形式的旧轮 `reasoning_content`，没有返回 400。清空之后，下一步不再像未填充基线那样五次都相同。这次不把「可以清思维链」写进档案。
