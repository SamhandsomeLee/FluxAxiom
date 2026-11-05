# P3 记录：遮蔽第 5 次请求的窗口外工具结果

核对日期：2026-11-05。只对任务 `mini-inventory-k5` 做一次遮蔽前后的比较。不接 Engine，不写 Run。档案不改，也不把损失门槛写成一个通用数值。

## 方法

测试：`crates/engine-custom/tests/context_baseline.rs` 的 `masked_tools_for_mini_inventory_k5`。命令是 `cargo test -p fluxaxiom-engine-custom --test context_baseline masked_tools_for_mini_inventory_k5 -- --ignored --exact --nocapture`。进程读到 `DEEPSEEK_API_KEY` 后拨号，输出里没有密钥，也没有工具正文。

底稿是 `T0-unfilled-k5-2026-11-04.md` 的第 5 次请求，blob 是 `sha256:005b4e4c7a92b174f62b889d01ffa9c33815af4a880e46ab329fa26e3aebb7cc`。当时这次请求的 `prompt_tokens` 是 2964。模型、`reasoning_effort`、`max_tokens` 和工具声明沿用 blob。签名规则与未填充基线相同。未填充基线的两两一致率是 1。决策等价率是遮蔽版 5 次采样里签名等于该基线的比例。损失是 1 减去决策等价率。

这条请求有 4 个已完成的模型轮，估算远低于 128K。`prepare` 不会在这里留下 L2。本次仍按保护窗口规则，把最近 2 轮以外的工具正文换成 L2 存根，system、user、assistant 和最近 2 轮的工具结果不动。换掉 5 条工具消息，原正文合计 873 字节。

缓存分两段。先把完整请求连发两次，作为触发前。再发 5 次遮蔽版，作为触发后。命中率是 `prompt_cache_hit_tokens / prompt_tokens`，两项都已知才计算。缺失不补成 0。

## 结果

七次都完成，结束原因都是 `tool_calls`。遮蔽版五次都没有结束，签名都是 `read_file {path:"tests/inventory.rs",start_line:1}`，路径 `tests/inventory.rs`。5 次都等于未填充基线。决策等价率是 1，损失是 0。这 5 次自身的两两一致率是 1，众数匹配的方差是 0。

完整请求的 `prompt_tokens` 是 2964。遮蔽版是 2907。少了 57。

| 阶段 | 次序 | prompt | hit | miss | 命中率 |
| --- | --- | ---: | ---: | ---: | ---: |
| 触发前 | 1 | 2964 | 2816 | 148 | 2816/2964 |
| 触发前 | 2 | 2964 | 2816 | 148 | 2816/2964 |
| 触发后 | 1 | 2907 | 384 | 2523 | 384/2907 |
| 触发后 | 2–5 | 2907 | 2688 | 219 | 2688/2907 |

触发前两次相同，前缀已经是热的。遮蔽后的第一次命中降到 384，未命中升到 2523。同一条遮蔽请求再发，命中回到 2688，未命中是 219。每一次都满足 `prompt = hit + miss`。

## 结论

在 `mini-inventory-k5` 这个位置上，把保护窗口外的 873 字节工具结果换成存根，没有改变下一步。损失是 0。最近两轮里较大的工具结果仍在请求中，所以这不能说明遮掉决策所依赖的正文也没有损失。

这条请求低于软触发，不能当作一次真实的 L2 触发。缓存上能确定的是：改写前缀后的第一次请求，命中从 2816 降到 384；同一条改写后的请求再次发送，命中回到 2688。

这次不把决策等价门槛写成一个通用数，也不改档案里的软触发。
