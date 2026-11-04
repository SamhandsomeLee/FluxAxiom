# T9 记录：声明剩余空间远小于窗口

核对日期：2026-11-04。只在任务 `mini-inventory-k5` 的第一条 user 末尾加一句剩余空间说明。未填充基线不重测。不接 Engine，不写 Run。档案不改。`mini-inventory-k1` 不进入。

## 方法

测试：`crates/engine-custom/tests/context_baseline.rs` 的 `remaining_space_notice_for_mini_inventory_k5`。命令是 `cargo test -p fluxaxiom-engine-custom --test context_baseline remaining_space_notice_for_mini_inventory_k5 -- --ignored --exact --nocapture`。进程从 `.env` 读到 `DEEPSEEK_API_KEY` 后拨号，输出里没有密钥，也没有正文和思维链。

底稿是 `T0-unfilled-k5-2026-11-04.md` 的第 5 次请求，blob 是 `sha256:005b4e4c7a92b174f62b889d01ffa9c33815af4a880e46ab329fa26e3aebb7cc`。后面的消息、工具声明和参数不改。追加的句子是「剩余空间远小于上下文窗口。」，前面有一个换行。模型、`reasoning_effort`、`max_tokens` 沿用 blob。采样次数 N 为 5。签名规则与未填充基线相同。

不提窗口的对照就是那次未填充基线：两两一致率是 1，签名是 `read_file {path:"tests/inventory.rs",start_line:1}`。决策等价率是这 5 次里等于该签名的比例。损失是 1 减去决策等价率。

正文和思维链都检查。出现「剩余空间」或 “remaining context” 记为提到。输出只写 yes 或 no。

第一次没有完成就停。这次没有发生。

## 结果

五次都完成。结束原因都是 `tool_calls`，都没有结束。服务端 `prompt_tokens` 都是 2971。当时那次未填充请求的 `prompt_tokens` 是 2964。

| 采样 | 工具调用 | 路径 | 等于基线 | 提到剩余空间 |
| --- | --- | --- | --- | --- |
| 1–5 | `read_file {path:"tests/inventory.rs",start_line:1}` | `tests/inventory.rs` | 是 | 否 |

5 次全部等于未填充基线。决策等价率是 1，损失是 0。这 5 次自身的两两一致率是 1，众数匹配的方差是 0。提到剩余空间的次数是 0。

## 结论

在这个任务位置上，声明剩余空间远小于窗口没有改变下一步，模型也没有提到剩余空间。`context_awareness` 这次不写取值。
