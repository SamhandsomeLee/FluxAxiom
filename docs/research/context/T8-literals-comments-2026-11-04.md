# T8 记录：字符串与注释

核对日期：2026-11-04。只覆盖 ASCII 字符串 `"aa"`、含汉字的字符串 `"中中"`、英文行注释 `//a` 加换行、英文块注释 `/*a*/`、中文注释 `//中` 加换行。完整函数、JSON 和整段相加见 `T8-whole-additivity-2026-11-04.md`。其他空白和其他标点尚未测量，保持未知。

## 方法

测试：`crates/engine-custom/tests/context_probe.rs` 的 `live_literals_comments_when_deepseek_client_response`。命令是 `cargo test -p fluxaxiom-engine-custom --test context_probe live_literals_comments_when_deepseek_client_response -- --ignored --exact --nocapture`。进程从 `.env` 读到 `DEEPSEEK_API_KEY` 后拨号，输出里没有密钥。这次没有重跑已有切片。

模型 `deepseek-flash`。不带 tools。`max_tokens` 为 1，`reasoning_effort` 为 `Low`。客户端按现有实现补上 `stream: true` 和 `include_usage: true`。

前缀是测试中的 `LIVE_PREFIX_SYSTEM` 与 `LIVE_PREFIX_USER`，十次探针共用同一次前缀测量。探针 user 是该前缀 user 再接上探针，不去掉末尾换行。探针是同一个语法单元重复到 N 个 Unicode 标量。引号、斜杠和换行都计入 N。比例是两次 `prompt_tokens` 之差除以 N。

成功时客户端不返回 HTTP 200，记录写成 `completed`。本次没有失败档。completion、缓存命中和未命中没有从这次输出里取得，不补数。

## 结果

| 类别 | 字数 | prompt_before | prompt_after | 差值 | 比例 | HTTP |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| 前缀 | — | — | 35 | — | — | completed |
| ascii_string | 200 | 35 | 136 | 101 | 0.505 | completed |
| ascii_string | 2000 | 35 | 1036 | 1001 | 0.5005 | completed |
| han_string | 200 | 35 | 186 | 151 | 0.755 | completed |
| han_string | 2000 | 35 | 1536 | 1501 | 0.7505 | completed |
| line_comment | 200 | 35 | 185 | 150 | 0.75 | completed |
| line_comment | 2000 | 35 | 1535 | 1500 | 0.75 | completed |
| block_comment | 200 | 35 | 194 | 159 | 0.795 | completed |
| block_comment | 2000 | 35 | 1634 | 1599 | 0.7995 | completed |
| han_comment | 200 | 35 | 185 | 150 | 0.75 | completed |
| han_comment | 2000 | 35 | 1535 | 1500 | 0.75 | completed |

两档相对差按 `|r200 - r2000| / max(r200, r2000)` 计算。ascii_string 的相对差是 9/1010，约 0.0089。han_string 的相对差是 9/1510，约 0.0060。block_comment 的相对差是 9/1599，约 0.0056。line_comment 与 han_comment 的相对差都是 0。五类都不超过 0.10，判定为稳定。不取平均。

英文行注释和中文注释在本次两档上的 token 差值相同。这只是这次输出的事实，不把两类合成一个比例。这些比例也不替换已经测过的单字符比例。

这是语法单元重复探针上的比例，不是一段真实函数里各切片相加的结果。档案 `crates/context/profiles/deepseek-deepseek-flash.toml` 仍是文档比例：汉字 0.6，ASCII 英文字母 0.3。`profile_version` 仍为 1，`count_text` 未改。这些稳定比例是否进入估算，留给 T2 第二步。
