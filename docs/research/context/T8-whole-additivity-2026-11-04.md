# T8 记录：整段与类别相加

核对日期：2026-11-04。覆盖一段 Rust 函数和一段 JSON 工具输出，以及把整段按类别拆开后的 token 差相加。其他空白、其他标点尚未测量，保持未知。

## 方法

测试：`crates/engine-custom/tests/context_probe.rs` 的 `live_whole_additivity_when_deepseek_client_response`。命令是 `cargo test -p fluxaxiom-engine-custom --test context_probe live_whole_additivity_when_deepseek_client_response -- --ignored --exact --nocapture`。进程从 `.env` 读到 `DEEPSEEK_API_KEY` 后拨号，输出里没有密钥。这次没有重跑已有切片。

模型 `deepseek-flash`。不带 tools。`max_tokens` 为 1，`reasoning_effort` 为 `Low`。客户端按现有实现补上 `stream: true` 和 `include_usage: true`。

前缀是测试中的 `LIVE_PREFIX_SYSTEM` 与 `LIVE_PREFIX_USER`，整段和类别桶共用同一次前缀测量。200 字是测试里拼好的那段正文。2000 字是这 200 字重复 10 次。类别桶把同一标签的片段按出现顺序直接拼起来，中间原来的其他类别不在桶里。比例是两次 `prompt_tokens` 之差除以该桶或整段的 Unicode 标量数。相加比较的是各类别桶的 token 差之和与整段 token 差。

成功时客户端不返回 HTTP 200，记录写成 `completed`。本次没有失败档。completion、缓存命中和未命中没有从这次输出里取得，不补数。

## 结果

| 类别 | 字数 | prompt_before | prompt_after | 差值 | 比例 | HTTP |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| 前缀 | — | — | 35 | — | — | completed |
| rust_whole | 200 | 35 | 125 | 90 | 0.45 | completed |
| rust_whole | 2000 | 35 | 935 | 900 | 0.45 | completed |
| json_whole | 200 | 35 | 122 | 87 | 0.435 | completed |
| json_whole | 2000 | 35 | 905 | 870 | 0.435 | completed |

两档相对差按 `|r200 - r2000| / max(r200, r2000)` 计算。两段整文的相对差都是 0，不超过 0.10，判定为稳定。不取平均。

| 样本 | 字数 | 类别桶 token 差之和 | 整段 token 差 | HTTP |
| --- | ---: | ---: | ---: | --- |
| rust | 200 | 74 | 90 | completed |
| rust | 2000 | 731 | 900 | completed |
| json | 200 | 108 | 87 | completed |
| json | 2000 | 1048 | 870 | completed |

四次都不相等。Rust 的类别桶之和小于整段，JSON 的类别桶之和大于整段。

类别桶里，两档相对差不超过 0.10 的有：Rust 的关键字、空白、数字与下划线、运算符、英文行注释、中文注释，以及 JSON 的运算符和 ASCII 字符串。Rust 的 ASCII 字符串、含汉字字符串、英文块注释，以及 JSON 的含汉字字符串、数字、空白，相对差超过 0.10，比例保持未知。这些桶的比例不替换此前单字符或重复单元的比例。

这是把同类片段拼成一条之后再相加，不是把每个片段留在原位测出来的和。档案 `crates/context/profiles/deepseek-deepseek-flash.toml` 仍是文档比例：汉字 0.6，ASCII 英文字母 0.3。`profile_version` 仍为 1，`count_text` 未改。哪些比例进入估算，留给 T2 第二步。
