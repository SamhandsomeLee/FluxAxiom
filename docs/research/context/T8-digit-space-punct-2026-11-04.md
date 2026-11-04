# T8 记录：数字、空格与标点

核对日期：2026-11-04。只覆盖 `ProbeClass::Digit`、`ProbeClass::Space`、`ProbeClass::AsciiPunct` 与 `ProbeClass::HanPunct`。换行、其他空白、其他标点、源代码切片和整段相加尚未测量，保持未知。

## 方法

测试：`crates/engine-custom/tests/context_probe.rs` 的 `live_digit_space_punct_when_deepseek_client_response`。命令是 `cargo test -p fluxaxiom-engine-custom --test context_probe live_digit_space_punct_when_deepseek_client_response -- --ignored --exact --nocapture`。进程从 `.env` 读到 `DEEPSEEK_API_KEY` 后拨号，输出里没有密钥。这次没有重跑汉字和英文字母。

模型 `deepseek-flash`。不带 tools。`max_tokens` 为 1，`reasoning_effort` 为 `Low`。客户端按现有实现补上 `stream: true` 和 `include_usage: true`。

前缀是测试中的 `LIVE_PREFIX_SYSTEM` 与 `LIVE_PREFIX_USER`，八次探针共用同一次前缀测量。探针 user 是该前缀 user 再接上探针，不去掉末尾空格。探针是同一个字符重复 N 次：`Digit` 重复 `1`，`Space` 重复空格 U+0020，`AsciiPunct` 重复 `.`，`HanPunct` 重复「，」。字数按 Unicode 标量计。比例是两次 `prompt_tokens` 之差除以 N。

成功时客户端不返回 HTTP 200，记录写成 `completed`。本次没有失败档。completion、缓存命中和未命中没有从这次输出里取得，不补数。

## 结果

| 类别 | 字数 | prompt_before | prompt_after | 差值 | 比例 | HTTP |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| 前缀 | — | — | 35 | — | — | completed |
| digit | 200 | 35 | 102 | 67 | 0.335 | completed |
| digit | 2000 | 35 | 702 | 667 | 0.3335 | completed |
| space | 200 | 35 | 38 | 3 | 0.015 | completed |
| space | 2000 | 35 | 52 | 17 | 0.0085 | completed |
| ascii_punct | 200 | 35 | 38 | 3 | 0.015 | completed |
| ascii_punct | 2000 | 35 | 52 | 17 | 0.0085 | completed |
| han_punct | 200 | 35 | 135 | 100 | 0.5 | completed |
| han_punct | 2000 | 35 | 1035 | 1000 | 0.5 | completed |

两档相对差按 `|r200 - r2000| / max(r200, r2000)` 计算。digit 的相对差是 3/670，约 0.0045，不超过 0.10，判定为稳定。han_punct 的相对差是 0，判定为稳定。space 与 ascii_punct 的相对差都是 13/30，约 0.433，超过 0.10，判定为未知。四类都不取平均。

空格和英文句点在本次两档上的 token 差值相同。这只是这次输出的事实，不把两类合成一个比例。

这是单字符重复探针上的比例，不是混合文本的比例。空格探针不含换行。档案 `crates/context/profiles/deepseek-deepseek-flash.toml` 仍是文档比例：汉字 0.6，ASCII 英文字母 0.3。`profile_version` 仍为 1，`count_text` 未改。数字和中文逗号的稳定比例是否进入估算，留给 T2 第二步。空格和英文句点保持未知，不能写入档案。
