# T8 记录：四片源代码单元

核对日期：2026-11-04。只覆盖重复单元 `fn`、`1_`、`+;` 和空格加换行。字符串与注释见 `T8-literals-comments-2026-11-04.md`。完整 Rust 函数、JSON、其他空白、其他标点和整段相加尚未测量，保持未知。

## 方法

测试：`crates/engine-custom/tests/context_probe.rs` 的 `live_source_slices_when_deepseek_client_response`。命令是 `cargo test -p fluxaxiom-engine-custom --test context_probe live_source_slices_when_deepseek_client_response -- --ignored --exact --nocapture`。进程从 `.env` 读到 `DEEPSEEK_API_KEY` 后拨号，输出里没有密钥。这次没有重跑已有字符类。

模型 `deepseek-flash`。不带 tools。`max_tokens` 为 1，`reasoning_effort` 为 `Low`。客户端按现有实现补上 `stream: true` 和 `include_usage: true`。

前缀是测试中的 `LIVE_PREFIX_SYSTEM` 与 `LIVE_PREFIX_USER`，八次探针共用同一次前缀测量。探针 user 是该前缀 user 再接上探针，不去掉末尾空白或换行。探针是同一个双字符单元重复到 N 个 Unicode 标量：`keyword` 重复 `fn`，`digit_underscore` 重复 `1_`，`operator` 重复 `+;`，`space_newline` 重复空格 U+0020 加换行 U+000A。比例是两次 `prompt_tokens` 之差除以 N。

成功时客户端不返回 HTTP 200，记录写成 `completed`。本次没有失败档。completion、缓存命中和未命中没有从这次输出里取得，不补数。

## 结果

| 类别 | 字数 | prompt_before | prompt_after | 差值 | 比例 | HTTP |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| 前缀 | — | — | 35 | — | — | completed |
| keyword | 200 | 35 | 135 | 100 | 0.5 | completed |
| keyword | 2000 | 35 | 1035 | 1000 | 0.5 | completed |
| digit_underscore | 200 | 35 | 235 | 200 | 1 | completed |
| digit_underscore | 2000 | 35 | 2035 | 2000 | 1 | completed |
| operator | 200 | 35 | 235 | 200 | 1 | completed |
| operator | 2000 | 35 | 2035 | 2000 | 1 | completed |
| space_newline | 200 | 35 | 60 | 25 | 0.125 | completed |
| space_newline | 2000 | 35 | 285 | 250 | 0.125 | completed |

两档相对差按 `|r200 - r2000| / max(r200, r2000)` 计算。四片的相对差都是 0，不超过 0.10，判定为稳定。不取平均。

`digit_underscore` 与 `operator` 在本次两档上的 token 差值相同。这只是这次输出的事实，不把两片合成一个比例。`keyword` 的 0.5 也不替换字母 `a` 的 0.125。`space_newline` 的 0.125 不替换单独空格那次的未知结果。

这是双字符单元重复探针上的比例，不是一段真实函数里各切片相加的结果。档案 `crates/context/profiles/deepseek-deepseek-flash.toml` 仍是文档比例：汉字 0.6，ASCII 英文字母 0.3。`profile_version` 仍为 1，`count_text` 未改。这些稳定比例是否进入估算，留给 T2 第二步。
