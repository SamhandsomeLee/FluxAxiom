# T2 记录：第一步，当前估算器对照 T8

核对日期：2026-11-04。只做执行方案 §8 的 T2 第一步：用 P1 估算器复算已记录的 T8 探针，按类别对照服务端 `prompt_tokens`。不写档案，不计算 p99，不做多轮锚点核对。

## 方法

测试：`crates/engine-custom/tests/context_probe.rs`。命令是 `cargo test -p fluxaxiom-engine-custom --test context_probe`。默认不跑 `#[ignore]` 的实调，本次没有拨号。

估算走 `ContextManager::prepare`。档案是 `deepseek_flash()`，`profile_version` 为 1。汉字比例 0.6，ASCII 英文字母比例 0.3，向上取整。`rounds` 为空，所以没有用量锚点。`max_tokens` 为 1。

正文与 T8 实调相同：system 是 `reply in one word`，user 是 `prefix` 再接探针。前缀单独一行，探针为空串。估算来源必须是 `estimated`，锚点必须为空。

偏差是估算减去该档记录的 `prompt_after`。负数表示低估。服务端数字来自 2026-11-04 的五份 T8 记录，不新打 API。类别桶只有稳定或未知的判定，没有逐档 `prompt_after`，不进入本表。

## 结果

| 类别 | 字数 | 估算 | prompt_after | 偏差 |
| --- | ---: | ---: | ---: | ---: |
| 前缀 | — | 6 | 35 | -29 |
| letter | 200 | 66 | 60 | 6 |
| letter | 2000 | 606 | 285 | 321 |
| han | 200 | 126 | 235 | -109 |
| han | 2000 | 1206 | 2035 | -829 |
| digit | 200 | 6 | 102 | -96 |
| digit | 2000 | 6 | 702 | -696 |
| space | 200 | 6 | 38 | -32 |
| space | 2000 | 6 | 52 | -46 |
| ascii_punct | 200 | 6 | 38 | -32 |
| ascii_punct | 2000 | 6 | 52 | -46 |
| han_punct | 200 | 6 | 135 | -129 |
| han_punct | 2000 | 6 | 1035 | -1029 |
| keyword | 200 | 66 | 135 | -69 |
| keyword | 2000 | 606 | 1035 | -429 |
| digit_underscore | 200 | 6 | 235 | -229 |
| digit_underscore | 2000 | 6 | 2035 | -2029 |
| operator | 200 | 6 | 235 | -229 |
| operator | 2000 | 6 | 2035 | -2029 |
| space_newline | 200 | 6 | 60 | -54 |
| space_newline | 2000 | 6 | 285 | -279 |
| ascii_string | 200 | 36 | 136 | -100 |
| ascii_string | 2000 | 306 | 1036 | -730 |
| han_string | 200 | 66 | 186 | -120 |
| han_string | 2000 | 606 | 1536 | -930 |
| line_comment | 200 | 21 | 185 | -164 |
| line_comment | 2000 | 156 | 1535 | -1379 |
| block_comment | 200 | 18 | 194 | -176 |
| block_comment | 2000 | 126 | 1634 | -1508 |
| han_comment | 200 | 36 | 185 | -149 |
| han_comment | 2000 | 306 | 1535 | -1229 |
| rust_whole | 200 | 22 | 125 | -103 |
| rust_whole | 2000 | 165 | 935 | -770 |
| json_whole | 200 | 27 | 122 | -95 |
| json_whole | 2000 | 207 | 905 | -698 |

数字、空格、英文句点、中文逗号、数字与下划线、运算符、空格加换行都不增加估算，这些行的估算停在前缀的 6。偏差大是当前计数范围下的预期结果。空格和英文句点在 T8 中已判定为未知，这里仍只报告偏差，不取比例。

档案在本记录写完后已进入 v2，见 `T2-profile-v2-2026-11-04.md`。上面的偏差表描述的是改档案之前的估算器。
