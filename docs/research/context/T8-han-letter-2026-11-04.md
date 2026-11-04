# T8 记录：汉字与英文字母

核对日期：2026-11-04。只覆盖 `ProbeClass::Han` 与 `ProbeClass::Letter`。数字、空白、标点、源代码切片和整段相加尚未测量，保持未知。

## 方法

测试：`crates/engine-custom/tests/context_probe.rs` 的 `live_completion_when_deepseek_client_response`。命令是 `cargo test -p fluxaxiom-engine-custom --test context_probe -- --ignored --nocapture`。进程从 `.env` 读到 `DEEPSEEK_API_KEY` 后拨号，输出里没有密钥。

模型 `deepseek-flash`。不带 tools。`max_tokens` 为 1，`reasoning_effort` 为 `Low`。客户端按现有实现补上 `stream: true` 和 `include_usage: true`。

前缀是测试中的 `LIVE_PREFIX_SYSTEM` 与 `LIVE_PREFIX_USER`，四次探针共用同一次前缀测量。第二次及以后的 user 是该前缀 user 再接上探针。探针是同一个字符重复 N 次：`Letter` 重复 `a`，`Han` 重复「中」。字数按 Unicode 标量计。比例是两次 `prompt_tokens` 之差除以 N。

成功时客户端不返回 HTTP 200，记录写成 `completed`。本次没有失败档。completion、缓存命中和未命中没有从这次输出里取得，不补数。

## 结果

| 类别 | 字数 | prompt_before | prompt_after | 差值 | 比例 | HTTP |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| 前缀 | — | — | 35 | — | — | completed |
| letter | 200 | 35 | 60 | 25 | 0.125 | completed |
| letter | 2000 | 35 | 285 | 250 | 0.125 | completed |
| han | 200 | 35 | 235 | 200 | 1 | completed |
| han | 2000 | 35 | 2035 | 2000 | 1 | completed |

两档相对差按 `|r200 - r2000| / max(r200, r2000)` 计算。letter 与 han 的相对差都是 0，不超过 0.10，判定为稳定。不取平均。

这是单字符重复探针上的比例，不是混合文本的比例。档案 `crates/context/profiles/deepseek-deepseek-flash.toml` 仍是文档比例：汉字 0.6，ASCII 英文字母 0.3。`profile_version` 仍为 1，`count_text` 未改。是否用本次比例替换文档比例，留给 T2 第二步。
