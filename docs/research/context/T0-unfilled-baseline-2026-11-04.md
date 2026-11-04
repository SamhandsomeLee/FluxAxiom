# T0 记录：未填充的自一致基线

核对日期：2026-11-04。只做任务 `mini-inventory-k1` 的完整版对完整版。没有填充。不接 Engine，不写 Run。采样走 `DeepSeekClient`，请求是已保存的 blob，不是新拼的 prompt。

## 方法

测试：`crates/engine-custom/tests/context_baseline.rs` 的 `unfilled_full_baseline_for_mini_inventory_k1`。命令是 `cargo test -p fluxaxiom-engine-custom --test context_baseline unfilled_full_baseline_for_mini_inventory_k1 -- --ignored --exact --nocapture`。进程从 `.env` 读到 `DEEPSEEK_API_KEY` 后拨号，输出里没有密钥。

模型 `deepseek-flash`。`reasoning_effort` 为 `high`，`max_tokens` 为 32768。工具声明是 `list_files`、`read_file`、`search_code`。采样次数 N 为 5。

任务是 2026-10-26 Run `run_01a175b8-f488-77ed-b6ea-153e72ff7616` 的第 1 次模型请求。请求 blob 是 `sha256:f48efa04c0446c5ecae4b5d9de2397f5c3cbb93430c184d975f69e88dd11ad97`。消息 2 条，没有先前的工具结果。当时这次请求的 `prompt_tokens` 是 527，结束原因是 `tool_calls`。

一次采样的签名是三件事：是否 `stop`、工具调用多重集（工具名加键有序的参数）、参数和正文里的路径。路径只收 `.`、`..` 和含斜杠的字符串。两两一致率的分母是 10 对。方差是“是否等于众数”的无偏样本方差；五种签名各出现一次时，众数按签名排序取最后一个，方差是 0.2。

## 结果

五次都完成，结束原因都是 `tool_calls`，都没有结束。

| 采样 | 工具调用 | 路径 |
| --- | --- | --- |
| 1 | `list_files {path:""}`；`search_code {query:"README"}` | 无 |
| 2 | `list_files {page:2,path:"."}`；`list_files {path:""}` | `.` |
| 3 | `list_files {path:"src"}` | 无 |
| 4 | `list_files {page:2,path:""}`；`list_files {path:""}` | 无 |
| 5 | `list_files {path:""}`；`search_code {query:"TODO\|FIXME\|XXX"}` | 无 |

不同签名 5 个。两两一致率是 0。众数匹配的方差是 0.2。

这个任务位置的基线不稳定，从后续 T6、T7、T9 剔除。原因是五次下一步两两不相同：模型对第一次工具调用没有固定选择。填充长度上的基线这次没有测。
