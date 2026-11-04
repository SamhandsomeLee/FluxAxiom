# T10 记录：占位工具结果的配对

核对日期：2026-11-04。只核对成对占位是否被接受，以及缺少 tool 消息、call id 对不上是否被拒绝。不接 Engine，不写 Run。档案不改。

## 方法

测试：`crates/engine-custom/tests/context_pairing.rs` 的 `paired_placeholder_when_deepseek_client_response`。命令是 `cargo test -p fluxaxiom-engine-custom --test context_pairing paired_placeholder_when_deepseek_client_response -- --ignored --exact --nocapture`。进程从 `.env` 读到 `DEEPSEEK_API_KEY` 后拨号，输出里没有密钥。

模型 `deepseek-flash`。`reasoning_effort` 为 `Low`，`max_tokens` 为 1。每条请求都声明工具 `ping`。消息是一条短 system、一条短 user，再加一条 assistant。assistant 的 `reasoning_content` 是空字符串，键仍在。工具调用 id 是 `call_1`，名字是 `ping`，参数是 `{}`。

四次只改最后的 tool 消息。对照正文是 `ok`，id 是 `call_1`。对照完成之后才发其余三次。占位正文是 `[unchanged: same result as round #1 call 1]`，43 个字符，id 仍是 `call_1`。缺消息用例不追加 tool 消息。错配用例的 id 是 `call_2`，正文仍是 `ok`。

成功记为 `completed`。客户端不返回 HTTP 200。拒绝只记状态码和类别，不记 `error.message`。

## 结果

对照完成，没有提前停下。

| 用例 | HTTP |
| --- | --- |
| `paired_plain` | completed |
| `paired_stub` | completed |
| `missing_tool` | rejected:400 |
| `mismatched_id` | rejected:400 |

## 结论

成对占位被接受。缺少 tool 消息和 call id 对不上都被拒绝，状态码都是 400。

这与假设 `must_pair_placeholder_accepted` 一致。档案 `crates/context/profiles/deepseek-deepseek-flash.toml` 这次不改，可信度仍是「假设」。要把可信度改成「实测」时另做一次，来源指向本记录，取值不变。

L1、L2 的措辞没有测。空 id、多条调用和省掉 `tool_call_id` 键也没有测。
