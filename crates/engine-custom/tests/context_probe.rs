//! T8 探针的离线比例计算。
//!
//! **作用**：把两次 `prompt_tokens` 换成比例，并判断 200 字与 2000 字两档是否稳定。也生成固定类别的探针正文。
//! **边界**：默认测试不发网络请求，不写 Run、事件或档案。文件末尾的 `#[ignore]` 测试只在显式 `--ignored` 时拨号。字符类、源代码单元、字符串、注释和整段分开实调。

use fluxaxiom_contract::TokenCount;
use fluxaxiom_engine_custom::{
    execute, CancellationToken, ChatError, ChatRequest, DeepSeekClient, Message, ReasoningEffort,
    SleepBackoff, API_KEY_ENV, DEFAULT_MODEL,
};

/// 一次探针的比例计算失败。
///
/// **作用**：`probe_ratio` 拒绝无法产生比例的输入。
/// **边界**：只有字数为 0 和 token 差为负两类。不表示 HTTP 失败。
#[derive(Debug, PartialEq, Eq)]
enum ProbeError {
    /// 探针字数为 0，不能做除法。
    EmptyProbe,

    /// 第二次的 prompt token 比第一次少。原始两次计数由调用方另记，这里不产生比例。
    NegativeDelta,
}

/// 200 字与 2000 字两档比例是否稳定。
///
/// **作用**：`two_size_verdict` 的结果。两档原始比例仍由调用方保存。
/// **边界**：`Unknown` 是判定结果，不是错误。不保存合并后的单个比例。
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// 相对差不超过 0.10，或两档都是 0。
    Stable,

    /// 相对差大于 0.10。不取两档的平均。
    Unknown,
}

/// 探针正文重复哪一类字符。
///
/// **作用**：把类别固定成一个代表字符，调用方不能再传入别的字符。
/// **边界**：每一类只重复一个标量。源代码切片不在这里。重复用的具体字符由 [`probe_text`] 决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeClass {
    /// 重复 ASCII 字母 `a`。
    Letter,
    /// 重复汉字「中」。一个「中」是一个 Unicode 标量。
    Han,
    /// 重复 ASCII 数字 `1`。不含全角数字。
    Digit,
    /// 重复空格 U+0020。不含换行或其他空白。
    Space,
    /// 重复 ASCII 句点 `.`。不含全角句点。
    AsciiPunct,
    /// 重复中文逗号「，」（U+FF0C）。一个逗号是一个 Unicode 标量。
    HanPunct,
}

/// 生成正好 `chars` 个相同字符的探针正文。
///
/// **作用**：供 200 字和 2000 字两档使用。调用方拥有返回的字符串，并可以追加到 user 末尾。
/// **参数**：`class` 选择重复哪一个代表字符。`chars` 是 Unicode 标量个数，0 返回空串。
/// **边界**：结果只含该类的一个字符，末尾空格也保留。不发请求，不计算比例。
fn probe_text(class: ProbeClass, chars: usize) -> String {
    let unit = match class {
        ProbeClass::Letter => "a",
        ProbeClass::Han => "中",
        ProbeClass::Digit => "1",
        ProbeClass::Space => " ",
        ProbeClass::AsciiPunct => ".",
        ProbeClass::HanPunct => "，",
    };
    unit.repeat(chars)
}

/// 源代码切片重复哪一个双字符单元。
///
/// **作用**：把前四片固定成关键字、数字与下划线、运算符和空白换行。
/// **边界**：不含字符串字面量、注释、完整函数或 JSON。单元正文由 [`slice_text`] 决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SliceClass {
    /// 重复 ASCII 关键字 `fn`。不含数字或下划线。
    Keyword,
    /// 重复 `1_`。数字和下划线各占一个标量。
    DigitUnderscore,
    /// 重复 `+;`。加号是运算符，分号是分隔符。
    Operator,
    /// 重复空格 U+0020 加换行 U+000A。不含制表符或其他空白。
    SpaceNewline,
}

/// 生成正好 `chars` 个标量的切片正文。
///
/// **作用**：供 200 字和 2000 字两档使用。调用方拥有返回的字符串，并可以追加到 user 末尾。
/// **参数**：`class` 选择重复哪一个双字符单元。`chars` 是 Unicode 标量个数，必须是 2 的倍数，0 返回空串。
/// **边界**：结果由该单元整段重复组成，末尾换行也保留。不发请求，不计算比例。
fn slice_text(class: SliceClass, chars: usize) -> String {
    let unit = match class {
        SliceClass::Keyword => "fn",
        SliceClass::DigitUnderscore => "1_",
        SliceClass::Operator => "+;",
        SliceClass::SpaceNewline => " \n",
    };
    assert!(chars.is_multiple_of(unit.chars().count()));
    unit.repeat(chars / unit.chars().count())
}

/// 字符串和注释重复哪一个语法单元。
///
/// **作用**：把这两类切片固定成五个单元，调用方不能再传入别的正文。
/// **边界**：不含完整函数或 JSON。单元正文由 [`syntax_text`] 决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyntaxClass {
    /// 重复 ASCII 字符串 `"aa"`。引号计入字数。
    AsciiString,
    /// 重复含汉字的字符串 `"中中"`。引号计入字数。
    HanString,
    /// 重复英文行注释 `//a` 加换行。换行计入字数。
    LineComment,
    /// 重复英文块注释 `/*a*/`。不含换行。
    BlockComment,
    /// 重复中文行注释 `//中` 加换行。换行计入字数。
    HanComment,
}

/// 生成正好 `chars` 个标量的字符串或注释正文。
///
/// **作用**：供 200 字和 2000 字两档使用。调用方拥有返回的字符串，并可以追加到 user 末尾。
/// **参数**：`class` 选择重复哪一个语法单元。`chars` 是 Unicode 标量个数，必须是该单元长度的倍数，0 返回空串。
/// **边界**：结果由该单元整段重复组成，末尾换行也保留。不发请求，不计算比例。
fn syntax_text(class: SyntaxClass, chars: usize) -> String {
    let unit = match class {
        SyntaxClass::AsciiString => "\"aa\"",
        SyntaxClass::HanString => "\"中中\"",
        SyntaxClass::LineComment => "//a\n",
        SyntaxClass::BlockComment => "/*a*/",
        SyntaxClass::HanComment => "//中\n",
    };
    assert!(chars.is_multiple_of(unit.chars().count()));
    unit.repeat(chars / unit.chars().count())
}

/// 比较 200 字与 2000 字两档比例的相对差。
///
/// **作用**：决定这一类字符的比例能不能当作稳定结果。
/// **参数**：`r_200` 是 200 字探针的 token 比例，由 `probe_ratio` 返回。`r_2000` 是 2000 字探针的 token 比例，来源相同。
/// **边界**：两档都是 0 时返回 `Stable`，避免除以 0。相对差大于 0.10 返回 `Unknown`，不取平均。
/// 不拒绝负数和非有限数；较大的数为负时相对差为负，大于 0.10 的比较不成立，返回 `Stable`。
fn two_size_verdict(r_200: f64, r_2000: f64) -> Verdict {
    if r_200 == 0.0 && r_2000 == 0.0 {
        return Verdict::Stable;
    }

    let gap = (r_200 - r_2000).abs() / r_200.max(r_2000);

    if gap > 0.10 {
        return Verdict::Unknown;
    }
    return Verdict::Stable;
}

/// 用两次 prompt token 的差除以探针字数。
///
/// **作用**：得到某一档探针的 token 比例。
/// **参数**：`prompt_before` 是只含前缀的 `prompt_tokens`，0 允许。`prompt_after` 是前缀末尾追加探针后的 `prompt_tokens`，必须不小于 `prompt_before`。`chars` 是探针的 Unicode 标量数，必须大于 0。
/// **边界**：`chars` 为 0 时返回 `EmptyProbe`。`prompt_after` 更小时返回 `NegativeDelta`，不产生比例。差值用 `checked_sub`。
fn probe_ratio(prompt_before: u64, prompt_after: u64, chars: u64) -> Result<f64, ProbeError> {
    if chars == 0 {
        return Err(ProbeError::EmptyProbe);
    }

    let delta = prompt_after
        .checked_sub(prompt_before)
        .ok_or(ProbeError::NegativeDelta)?;
    Ok(delta as f64 / chars as f64)
}

/// 差为 2、字数为 4 时比例是 0.5。
///
/// **作用**：锁住 `probe_ratio` 使用减法。
/// **边界**：不访问网络。
#[test]
fn normal_sub() {
    assert_eq!(probe_ratio(10, 12, 4), Ok(0.5))
}

/// 字数为 0 时不能做除法。
///
/// **作用**：锁住 `EmptyProbe`。
/// **边界**：token 数合法也不产生比例。
#[test]
fn assert_chars_eq_0() {
    assert_eq!(probe_ratio(10, 12, 0), Err(ProbeError::EmptyProbe))
}

/// 第二次 token 更少时拒绝产生比例。
///
/// **作用**：锁住 `NegativeDelta`。
/// **边界**：不把负差编成 0。
#[test]
fn assert_negative_delta() {
    assert_eq!(probe_ratio(12, 10, 4), Err(ProbeError::NegativeDelta))
}

/// 字母探针的字数等于参数，并且不含数字。
///
/// **作用**：锁住 `Letter` 重复的是 `a`。
/// **边界**：不访问网络。2000 字与 200 字使用同一个生成规则。
#[test]
fn letter_probe_has_exact_length_and_no_digits() {
    let text = probe_text(ProbeClass::Letter, 200);
    assert_eq!(text.chars().count(), 200);
    assert!(text.chars().all(|ch| ch == 'a'));
    assert_eq!(probe_text(ProbeClass::Letter, 2000).chars().count(), 2000);
}

/// 汉字探针的字数等于参数。
///
/// **作用**：锁住 `Han` 重复的是「中」，每个「中」只计一个 Unicode 标量。
/// **边界**：不访问网络。0 返回空串。
#[test]
fn han_probe_counts_one_scalar_per_character() {
    let text = probe_text(ProbeClass::Han, 200);
    assert_eq!(text.chars().count(), 200);
    assert!(text.chars().all(|ch| ch == '中'));
    assert_eq!(probe_text(ProbeClass::Han, 0).chars().count(), 0);
}

/// 数字探针的字数等于参数，并且只含 ASCII `1`。
///
/// **作用**：锁住 `Digit` 重复的是 `1`。
/// **边界**：不访问网络。不含字母、空格或全角数字。
#[test]
fn digit_probe_repeats_ascii_one() {
    let text = probe_text(ProbeClass::Digit, 200);
    assert_eq!(text.chars().count(), 200);
    assert!(text.chars().all(|ch| ch == '1'));
    assert_eq!(probe_text(ProbeClass::Digit, 2000).chars().count(), 2000);
}

/// 空白探针的字数等于参数，并且只含空格。
///
/// **作用**：锁住 `Space` 重复的是 U+0020。
/// **边界**：不访问网络。不把换行算进这一类，也不去掉末尾空格。
#[test]
fn space_probe_repeats_one_space_and_keeps_trailing_spaces() {
    let text = probe_text(ProbeClass::Space, 200);
    assert_eq!(text.chars().count(), 200);
    assert_eq!(text.len(), 200);
    assert!(text.chars().all(|ch| ch == ' '));
    assert!(!text.contains('\n'));
    assert_eq!(probe_text(ProbeClass::Space, 2000).chars().count(), 2000);
}

/// 英文标点探针的字数等于参数，并且只含 ASCII 句点。
///
/// **作用**：锁住 `AsciiPunct` 重复的是 `.`。
/// **边界**：不访问网络。不含中文标点。
#[test]
fn ascii_punct_probe_repeats_period() {
    let text = probe_text(ProbeClass::AsciiPunct, 200);
    assert_eq!(text.chars().count(), 200);
    assert!(text.chars().all(|ch| ch == '.'));
    assert_eq!(
        probe_text(ProbeClass::AsciiPunct, 2000).chars().count(),
        2000
    );
}

/// 中文标点探针的字数等于参数。
///
/// **作用**：锁住 `HanPunct` 重复的是「，」，每个逗号只计一个 Unicode 标量。
/// **边界**：不访问网络。不含 ASCII 逗号。
#[test]
fn han_punct_probe_counts_one_scalar_per_comma() {
    let text = probe_text(ProbeClass::HanPunct, 200);
    assert_eq!(text.chars().count(), 200);
    assert!(text.chars().all(|ch| ch == '，'));
    assert!(!text.contains(','));
    assert_eq!(probe_text(ProbeClass::HanPunct, 2000).chars().count(), 2000);
}

/// 关键字切片的字数等于参数，并且只含 `fn`。
///
/// **作用**：锁住 `Keyword` 重复的是 `fn`。
/// **边界**：不访问网络。不含数字或下划线。
#[test]
fn keyword_slice_repeats_fn() {
    let text = slice_text(SliceClass::Keyword, 200);
    assert_eq!(text.chars().count(), 200);
    assert!(text
        .chars()
        .enumerate()
        .all(|(index, ch)| ch == if index.is_multiple_of(2) { 'f' } else { 'n' }));
    assert_eq!(slice_text(SliceClass::Keyword, 2000).chars().count(), 2000);
    assert_eq!(slice_text(SliceClass::Keyword, 0).chars().count(), 0);
}

/// 数字与下划线切片的字数等于参数。
///
/// **作用**：锁住 `DigitUnderscore` 重复的是 `1_`。
/// **边界**：不访问网络。不含字母。
#[test]
fn digit_underscore_slice_repeats_one_and_underscore() {
    let text = slice_text(SliceClass::DigitUnderscore, 200);
    assert_eq!(text.chars().count(), 200);
    assert!(text
        .chars()
        .enumerate()
        .all(|(index, ch)| { ch == if index.is_multiple_of(2) { '1' } else { '_' } }));
    assert!(!text.chars().any(|ch| ch.is_ascii_alphabetic()));
    assert_eq!(
        slice_text(SliceClass::DigitUnderscore, 2000)
            .chars()
            .count(),
        2000
    );
}

/// 运算符切片的字数等于参数，并且只含 `+;`。
///
/// **作用**：锁住 `Operator` 重复的是加号和分号。
/// **边界**：不访问网络。不含字母、数字或空白。
#[test]
fn operator_slice_repeats_plus_and_semicolon() {
    let text = slice_text(SliceClass::Operator, 200);
    assert_eq!(text.chars().count(), 200);
    assert!(text
        .chars()
        .enumerate()
        .all(|(index, ch)| ch == if index.is_multiple_of(2) { '+' } else { ';' }));
    assert_eq!(slice_text(SliceClass::Operator, 2000).chars().count(), 2000);
}

/// 空白换行切片的字数等于参数，并且保留末尾换行。
///
/// **作用**：锁住 `SpaceNewline` 交替重复空格和换行。
/// **边界**：不访问网络。不去掉末尾换行，也不把制表符算进这一类。
#[test]
fn space_newline_slice_keeps_trailing_newline() {
    let text = slice_text(SliceClass::SpaceNewline, 200);
    assert_eq!(text.chars().count(), 200);
    assert!(text
        .chars()
        .enumerate()
        .all(|(index, ch)| ch == if index.is_multiple_of(2) { ' ' } else { '\n' }));
    assert!(text.ends_with('\n'));
    assert!(!text.contains('\t'));
    assert_eq!(
        slice_text(SliceClass::SpaceNewline, 2000).chars().count(),
        2000
    );
}

/// ASCII 字符串切片的字数等于参数，并且由 `"aa"` 重复组成。
///
/// **作用**：锁住 `AsciiString` 的单元含引号。
/// **边界**：不访问网络。不含汉字或换行。
#[test]
fn ascii_string_slice_repeats_quoted_aa() {
    let text = syntax_text(SyntaxClass::AsciiString, 200);
    assert_eq!(text.chars().count(), 200);
    assert_eq!(text, "\"aa\"".repeat(50));
    assert!(!text.contains('中'));
    assert!(!text.contains('\n'));
    assert_eq!(
        syntax_text(SyntaxClass::AsciiString, 2000).chars().count(),
        2000
    );
}

/// 含汉字的字符串切片的字数等于参数。
///
/// **作用**：锁住 `HanString` 重复的是 `"中中"`。
/// **边界**：不访问网络。不含换行。
#[test]
fn han_string_slice_repeats_quoted_han() {
    let text = syntax_text(SyntaxClass::HanString, 200);
    assert_eq!(text.chars().count(), 200);
    assert_eq!(text, "\"中中\"".repeat(50));
    assert!(!text.contains('\n'));
    assert_eq!(
        syntax_text(SyntaxClass::HanString, 2000).chars().count(),
        2000
    );
    assert_eq!(syntax_text(SyntaxClass::HanString, 0).chars().count(), 0);
}

/// 英文行注释切片的字数等于参数，并且保留末尾换行。
///
/// **作用**：锁住 `LineComment` 重复的是 `//a` 加换行。
/// **边界**：不访问网络。不去掉末尾换行，也不含块注释。
#[test]
fn line_comment_slice_keeps_trailing_newline() {
    let text = syntax_text(SyntaxClass::LineComment, 200);
    assert_eq!(text.chars().count(), 200);
    assert_eq!(text, "//a\n".repeat(50));
    assert!(text.ends_with('\n'));
    assert!(!text.contains("/*"));
    assert_eq!(
        syntax_text(SyntaxClass::LineComment, 2000).chars().count(),
        2000
    );
}

/// 英文块注释切片的字数等于参数。
///
/// **作用**：锁住 `BlockComment` 重复的是 `/*a*/`。
/// **边界**：不访问网络。不含换行。
#[test]
fn block_comment_slice_repeats_slash_star() {
    let text = syntax_text(SyntaxClass::BlockComment, 200);
    assert_eq!(text.chars().count(), 200);
    assert_eq!(text, "/*a*/".repeat(40));
    assert!(!text.contains('\n'));
    assert_eq!(
        syntax_text(SyntaxClass::BlockComment, 2000).chars().count(),
        2000
    );
}

/// 中文注释切片的字数等于参数，并且保留末尾换行。
///
/// **作用**：锁住 `HanComment` 重复的是 `//中` 加换行。
/// **边界**：不访问网络。不去掉末尾换行。
#[test]
fn han_comment_slice_keeps_trailing_newline() {
    let text = syntax_text(SyntaxClass::HanComment, 200);
    assert_eq!(text.chars().count(), 200);
    assert_eq!(text, "//中\n".repeat(50));
    assert!(text.ends_with('\n'));
    assert!(text.contains('中'));
    assert_eq!(
        syntax_text(SyntaxClass::HanComment, 2000).chars().count(),
        2000
    );
}

/// 整段探针里的一片。
///
/// **作用**：按出现顺序拼成 Rust 函数或 JSON。同一 `label` 的正文再拼成类别桶。
/// **边界**：`text` 只留在测试进程里，不写入记录或 stdout。
struct Piece {
    /// 类别标签。同一标签的片段属于一个切片桶。
    label: &'static str,
    /// 这一片的正文。空串不会出现。
    text: String,
}

/// 用标签和静态正文做一片。
///
/// **作用**：组装 [`rust_pieces`] 和 [`json_pieces`]。
/// **参数**：`label` 是类别标签，不能是空串。`text` 是这一片的正文，不能是空串。
/// **边界**：调用方拥有返回值里的正文。不检查标签是否属于某一段。
fn piece(label: &'static str, text: &'static str) -> Piece {
    Piece {
        label,
        text: text.to_owned(),
    }
}

/// 追加一行 `let x = 1;`。
///
/// **作用**：把 Rust 样本补到 200 个标量。
/// **参数**：`pieces` 接收新片段，顺序加在已有片段之后。
/// **边界**：这一行固定是 15 个标量。不检查样本总长。
fn push_let_x(pieces: &mut Vec<Piece>) {
    pieces.push(piece("whitespace", "    "));
    pieces.push(piece("keyword", "let"));
    pieces.push(piece("whitespace", " "));
    pieces.push(piece("keyword", "x"));
    pieces.push(piece("whitespace", " "));
    pieces.push(piece("operator", "="));
    pieces.push(piece("whitespace", " "));
    pieces.push(piece("digit_underscore", "1"));
    pieces.push(piece("operator", ";"));
    pieces.push(piece("whitespace", "\n"));
}

/// 200 个标量的 Rust 函数，按切片拆开。
///
/// **作用**：整段相加使用的函数样本。
/// **边界**：片段按顺序拼接后正好 200 个标量。不含制表符。
fn rust_pieces() -> Vec<Piece> {
    let mut pieces = vec![
        piece("keyword", "fn"),
        piece("whitespace", " "),
        piece("keyword", "add"),
        piece("digit_underscore", "_1"),
        piece("operator", "()"),
        piece("whitespace", " "),
        piece("operator", "{"),
        piece("whitespace", "\n"),
        piece("whitespace", "    "),
        piece("keyword", "let"),
        piece("whitespace", " "),
        piece("keyword", "s"),
        piece("whitespace", " "),
        piece("operator", "="),
        piece("whitespace", " "),
        piece("ascii_string", "\"aa\""),
        piece("operator", ";"),
        piece("whitespace", "\n"),
        piece("whitespace", "    "),
        piece("keyword", "let"),
        piece("whitespace", " "),
        piece("keyword", "h"),
        piece("whitespace", " "),
        piece("operator", "="),
        piece("whitespace", " "),
        piece("han_string", "\"中\""),
        piece("operator", ";"),
        piece("whitespace", "\n"),
        piece("whitespace", "    "),
        piece("line_comment", "//a\n"),
        piece("whitespace", "    "),
        piece("block_comment", "/*a*/"),
        piece("whitespace", "\n"),
        piece("whitespace", "    "),
        piece("han_comment", "//中\n"),
    ];
    for _ in 0..8 {
        push_let_x(&mut pieces);
    }
    pieces.push(piece("whitespace", "    "));
    pieces.push(piece("operator", "}"));
    pieces.push(piece("whitespace", "\n"));
    pieces
}

/// 200 个标量的 JSON 工具输出，按切片拆开。
///
/// **作用**：整段相加使用的 JSON 样本。
/// **边界**：片段按顺序拼接后正好 200 个标量。末尾 8 个空格用来补齐长度。
fn json_pieces() -> Vec<Piece> {
    let mut pieces = vec![
        piece("operator", "{"),
        piece("ascii_string", "\"path\""),
        piece("operator", ":"),
        piece("ascii_string", "\"aa\""),
        piece("operator", ","),
        piece("ascii_string", "\"name\""),
        piece("operator", ":"),
        piece("han_string", "\"中\""),
        piece("operator", ","),
        piece("ascii_string", "\"n\""),
        piece("operator", ":"),
        piece("digit", "1"),
    ];
    for _ in 0..18 {
        pieces.push(piece("operator", ","));
        pieces.push(piece("ascii_string", "\"k\""));
        pieces.push(piece("operator", ":"));
        pieces.push(piece("ascii_string", "\"aa\""));
    }
    pieces.push(piece("whitespace", "        "));
    pieces.push(piece("operator", "}"));
    pieces
}

/// 按顺序拼接全部片段。
///
/// **作用**：得到整段探针。
/// **参数**：`pieces` 是按出现顺序排列的片段。空切片得到空串。
/// **边界**：不插入分隔符，也不去掉末尾换行。
fn whole_text(pieces: &[Piece]) -> String {
    pieces.iter().map(|item| item.text.as_str()).collect()
}

/// 把同一标签的片段按出现顺序拼成一个桶。
///
/// **作用**：整段相加时，每一类单独测这一条拼接结果。
/// **参数**：`pieces` 是整段的全部片段。`label` 是要收集的类别标签。
/// **边界**：标签没有出现时返回空串。拼接会去掉原本夹在中间的其他类别。
fn bucket_text(pieces: &[Piece], label: &str) -> String {
    pieces
        .iter()
        .filter(|item| item.label == label)
        .map(|item| item.text.as_str())
        .collect()
}

/// 按首次出现收集标签。
///
/// **作用**：决定类别桶的测量顺序。
/// **参数**：`pieces` 是整段的全部片段。
/// **边界**：重复标签只保留第一次。不返回空标签。
fn labels_in_order(pieces: &[Piece]) -> Vec<&str> {
    let mut labels = Vec::new();
    for item in pieces {
        if !labels.contains(&item.label) {
            labels.push(item.label);
        }
    }
    labels
}

/// Rust 样本是 200 个标量，类别桶合起来等于整段。
///
/// **作用**：锁住函数正文的长度和拆分。
/// **边界**：不访问网络。2000 字是这 200 字重复 10 次。
#[test]
fn rust_whole_is_200_scalars_and_buckets_cover_it() {
    let pieces = rust_pieces();
    let whole = whole_text(&pieces);
    assert_eq!(whole.chars().count(), 200);
    assert!(whole.starts_with("fn add_1() {"));
    assert!(whole.ends_with("}\n"));
    assert!(!whole.contains('\t'));
    let bucket_chars: usize = labels_in_order(&pieces)
        .into_iter()
        .map(|label| bucket_text(&pieces, label).chars().count())
        .sum();
    assert_eq!(bucket_chars, 200);
    assert_eq!(whole.repeat(10).chars().count(), 2000);
}

/// JSON 样本是 200 个标量，类别桶合起来等于整段。
///
/// **作用**：锁住工具输出的长度和拆分。
/// **边界**：不访问网络。2000 字是这 200 字重复 10 次。
#[test]
fn json_whole_is_200_scalars_and_buckets_cover_it() {
    let pieces = json_pieces();
    let whole = whole_text(&pieces);
    assert_eq!(whole.chars().count(), 200);
    assert!(whole.starts_with("{\"path\":\"aa\""));
    assert!(whole.ends_with('}'));
    assert!(whole.contains('中'));
    let bucket_chars: usize = labels_in_order(&pieces)
        .into_iter()
        .map(|label| bucket_text(&pieces, label).chars().count())
        .sum();
    assert_eq!(bucket_chars, 200);
    assert_eq!(whole.repeat(10).chars().count(), 2000);
}

/// 四组两档比例覆盖稳定、全 0、门槛和超门槛。
///
/// **作用**：锁住 `two_size_verdict` 的相对差规则。
/// **边界**：`0.9` 与 `1.0` 的相对差是 0.10，仍为 `Stable`。`0.8` 与 `1.0` 为 `Unknown`。
#[test]
fn assert_two_size_verdict() {
    assert_eq!(two_size_verdict(0.5, 0.5), Verdict::Stable);
    assert_eq!(two_size_verdict(0.0, 0.0), Verdict::Stable);
    assert_eq!(two_size_verdict(0.9, 1.0), Verdict::Stable);
    assert_eq!(two_size_verdict(0.8, 1.0), Verdict::Unknown);
}

/// 实调前缀的 system。四次探针共用，不追加探针。
const LIVE_PREFIX_SYSTEM: &str = "reply in one word";

/// 实调前缀的 user。探针正文只追加在这条字符串后面。
const LIVE_PREFIX_USER: &str = "prefix";

/// 一次实调没有拿到可用的 `prompt_tokens`。
///
/// **作用**：把用量缺失和传输失败分开。
/// **边界**：不保存请求正文、思维链或密钥。
enum LivePromptError {
    /// 响应里没有已知的 `prompt_tokens`。不是 HTTP 状态。
    UsageMissing,
    /// 传输层失败。打印时只用类别，不用 `Display`，避免带上服务端摘要。
    Chat(ChatError),
}

/// 组装一条不带工具的实调请求。
///
/// **作用**：前缀请求和追加探针的请求共用同一套模型参数。
/// **参数**：`user` 是这次的 user 正文。前缀调用只含 [`LIVE_PREFIX_USER`]；探针调用在它后面接上探针。
/// **边界**：`max_tokens` 固定为 1，`reasoning_effort` 固定为 `Low`。不写入 `tools`。
fn live_request(user: String) -> ChatRequest {
    let mut request = ChatRequest::new(
        DEFAULT_MODEL,
        vec![
            Message::System {
                content: LIVE_PREFIX_SYSTEM.to_owned(),
            },
            Message::User { content: user },
        ],
    )
    .with_max_tokens(1)
    .expect("max_tokens 1 is valid");
    request.reasoning_effort = ReasoningEffort::Low;
    request
}

/// 发送一条 user 文本，取出服务端的 `prompt_tokens`。
///
/// **作用**：前缀和每一档探针都走这里。
/// **参数**：`client` 是已带密钥的 DeepSeek 客户端，本次调用会使用它。`user` 是完整的 user 正文，不写入输出。
/// **边界**：成功时传输层不把 HTTP 200 交出来。用量缺失返回 `UsageMissing`，不编成 0。
async fn live_prompt(client: &mut DeepSeekClient, user: &str) -> Result<u64, LivePromptError> {
    let request = live_request(user.to_owned());
    let completion = execute(
        client,
        &request,
        &mut SleepBackoff,
        &CancellationToken::new(),
    )
    .await
    .map_err(LivePromptError::Chat)?;
    match completion.usage.prompt_tokens {
        TokenCount::Known(tokens) => Ok(tokens),
        TokenCount::Unknown => Err(LivePromptError::UsageMissing),
    }
}

/// 失败时打印的状态标签。
///
/// **作用**：stdout 只留类别和状态码。
/// **参数**：`error` 是一次实调失败。
/// **边界**：不打印 `summary`、正文或密钥。成功路径不使用这个函数。
fn live_http_label(error: &LivePromptError) -> String {
    match error {
        LivePromptError::UsageMissing => "usage_missing".to_owned(),
        LivePromptError::Chat(ChatError::Rejected { status, .. }) => format!("rejected:{status}"),
        LivePromptError::Chat(ChatError::Unavailable { status, .. }) => {
            format!("unavailable:{status}")
        }
        LivePromptError::Chat(ChatError::StreamInterrupted) => "stream_interrupted".to_owned(),
        LivePromptError::Chat(ChatError::Cancelled) => "cancelled".to_owned(),
        LivePromptError::Chat(ChatError::InvalidToolArguments { .. }) => {
            "invalid_tool_arguments".to_owned()
        }
        LivePromptError::Chat(ChatError::MalformedToolCall { .. }) => {
            "malformed_tool_call".to_owned()
        }
        LivePromptError::Chat(ChatError::NotEncodable) => "not_encodable".to_owned(),
        LivePromptError::Chat(ChatError::MissingCredential) => "missing_credential".to_owned(),
        LivePromptError::Chat(ChatError::Transport) => "transport".to_owned(),
    }
}

/// 打印一档探针的比例，或失败类别。
///
/// **作用**：把 `probe_ratio` 用到实调得到的两个 token 数上。
/// **参数**：`class` 是这一档的类别标签。`chars` 是探针字数。`before` 是前缀的 `prompt_tokens`。`after` 是追加探针后的结果。
/// **边界**：不打印探针正文。成功时 HTTP 状态写成 `completed`，因为客户端不返回 200。
fn print_probe(class: &str, chars: usize, before: u64, after: Result<u64, LivePromptError>) {
    match after {
        Ok(tokens) => match probe_ratio(before, tokens, chars as u64) {
            Ok(ratio) => println!(
                "{class} chars={chars} prompt_before={before} prompt_after={tokens} ratio={ratio} http=completed"
            ),
            Err(error) => println!(
                "{class} chars={chars} prompt_before={before} prompt_after={tokens} ratio={error:?} http=completed"
            ),
        },
        Err(error) => println!(
            "{class} chars={chars} prompt_before={before} http={}",
            live_http_label(&error)
        ),
    }
}

/// 有密钥时测量一次共用前缀。
///
/// **作用**：汉字、字母和后续字符类实调都从这里拿 `prompt_before`。
/// **边界**：没有 `DEEPSEEK_API_KEY` 时返回 `None`，不拨号。前缀失败时打印状态并返回 `None`。不打印密钥。
async fn live_shared_prefix() -> Option<(DeepSeekClient, u64)> {
    if std::env::var(API_KEY_ENV)
        .ok()
        .filter(|key| !key.is_empty())
        .is_none()
    {
        return None;
    }

    let mut client = DeepSeekClient::from_env().expect("key checked above");
    match live_prompt(&mut client, LIVE_PREFIX_USER).await {
        Ok(tokens) => {
            println!("prefix prompt_tokens={tokens} http=completed");
            Some((client, tokens))
        }
        Err(error) => {
            println!("prefix http={}", live_http_label(&error));
            None
        }
    }
}

/// 用同一条前缀测量一组探针。
///
/// **作用**：把探针追加到前缀 user 后面，再打印每一档的 token 差。
/// **参数**：`client` 是已测完前缀的 DeepSeek 客户端，本函数会继续用它拨号。`prompt_before` 是前缀的 `prompt_tokens`。`probes` 的每一项是类别标签、字数和探针正文。
/// **边界**：不打印探针正文。user 由前缀和探针直接拼接，不去掉末尾空格。
async fn live_probes(
    client: &mut DeepSeekClient,
    prompt_before: u64,
    probes: Vec<(&str, usize, String)>,
) {
    for (class, chars, probe) in probes {
        let user = format!("{LIVE_PREFIX_USER}{probe}");
        print_probe(
            class,
            chars,
            prompt_before,
            live_prompt(client, &user).await,
        );
    }
}

/// 汉字和英文字母各两档的实调。默认不跑。
///
/// **作用**：用同一条前缀测量 200 字和 2000 字探针的 `prompt_tokens`。
/// **边界**：没有 `DEEPSEEK_API_KEY` 时直接返回，不拨号。不写 Run、档案或记录文件。stdout 不含探针正文和密钥。
#[tokio::test]
#[ignore = "dials api.deepseek.com when DEEPSEEK_API_KEY is set"]
async fn live_completion_when_deepseek_client_response() {
    let Some((mut client, prompt_before)) = live_shared_prefix().await else {
        return;
    };
    live_probes(
        &mut client,
        prompt_before,
        vec![
            ("letter", 200, probe_text(ProbeClass::Letter, 200)),
            ("letter", 2000, probe_text(ProbeClass::Letter, 2000)),
            ("han", 200, probe_text(ProbeClass::Han, 200)),
            ("han", 2000, probe_text(ProbeClass::Han, 2000)),
        ],
    )
    .await;
}

/// 数字、空格、英文句点和中文逗号各两档的实调。默认不跑。
///
/// **作用**：沿用汉字与字母的前缀和请求参数，只追加这四类探针。
/// **边界**：不重测汉字和字母。没有密钥时不拨号。不写 Run、档案或记录文件。stdout 不含探针正文和密钥。
#[tokio::test]
#[ignore = "dials api.deepseek.com when DEEPSEEK_API_KEY is set"]
async fn live_digit_space_punct_when_deepseek_client_response() {
    let Some((mut client, prompt_before)) = live_shared_prefix().await else {
        return;
    };
    live_probes(
        &mut client,
        prompt_before,
        vec![
            ("digit", 200, probe_text(ProbeClass::Digit, 200)),
            ("digit", 2000, probe_text(ProbeClass::Digit, 2000)),
            ("space", 200, probe_text(ProbeClass::Space, 200)),
            ("space", 2000, probe_text(ProbeClass::Space, 2000)),
            ("ascii_punct", 200, probe_text(ProbeClass::AsciiPunct, 200)),
            (
                "ascii_punct",
                2000,
                probe_text(ProbeClass::AsciiPunct, 2000),
            ),
            ("han_punct", 200, probe_text(ProbeClass::HanPunct, 200)),
            ("han_punct", 2000, probe_text(ProbeClass::HanPunct, 2000)),
        ],
    )
    .await;
}

/// 四片源代码切片各两档的实调。默认不跑。
///
/// **作用**：沿用同一条前缀，只追加关键字、数字与下划线、运算符、空格加换行。
/// **边界**：不重测已有字符类。没有密钥时不拨号。不写 Run、档案或记录文件。stdout 不含探针正文和密钥。
#[tokio::test]
#[ignore = "dials api.deepseek.com when DEEPSEEK_API_KEY is set"]
async fn live_source_slices_when_deepseek_client_response() {
    let Some((mut client, prompt_before)) = live_shared_prefix().await else {
        return;
    };
    live_probes(
        &mut client,
        prompt_before,
        vec![
            ("keyword", 200, slice_text(SliceClass::Keyword, 200)),
            ("keyword", 2000, slice_text(SliceClass::Keyword, 2000)),
            (
                "digit_underscore",
                200,
                slice_text(SliceClass::DigitUnderscore, 200),
            ),
            (
                "digit_underscore",
                2000,
                slice_text(SliceClass::DigitUnderscore, 2000),
            ),
            ("operator", 200, slice_text(SliceClass::Operator, 200)),
            ("operator", 2000, slice_text(SliceClass::Operator, 2000)),
            (
                "space_newline",
                200,
                slice_text(SliceClass::SpaceNewline, 200),
            ),
            (
                "space_newline",
                2000,
                slice_text(SliceClass::SpaceNewline, 2000),
            ),
        ],
    )
    .await;
}

/// 字符串和注释各两档的实调。默认不跑。
///
/// **作用**：沿用同一条前缀，只追加两个字符串字面量和三类注释。
/// **边界**：不重测已有切片。没有密钥时不拨号。不写 Run、档案或记录文件。stdout 不含探针正文和密钥。
#[tokio::test]
#[ignore = "dials api.deepseek.com when DEEPSEEK_API_KEY is set"]
async fn live_literals_comments_when_deepseek_client_response() {
    let Some((mut client, prompt_before)) = live_shared_prefix().await else {
        return;
    };
    live_probes(
        &mut client,
        prompt_before,
        vec![
            (
                "ascii_string",
                200,
                syntax_text(SyntaxClass::AsciiString, 200),
            ),
            (
                "ascii_string",
                2000,
                syntax_text(SyntaxClass::AsciiString, 2000),
            ),
            ("han_string", 200, syntax_text(SyntaxClass::HanString, 200)),
            (
                "han_string",
                2000,
                syntax_text(SyntaxClass::HanString, 2000),
            ),
            (
                "line_comment",
                200,
                syntax_text(SyntaxClass::LineComment, 200),
            ),
            (
                "line_comment",
                2000,
                syntax_text(SyntaxClass::LineComment, 2000),
            ),
            (
                "block_comment",
                200,
                syntax_text(SyntaxClass::BlockComment, 200),
            ),
            (
                "block_comment",
                2000,
                syntax_text(SyntaxClass::BlockComment, 2000),
            ),
            (
                "han_comment",
                200,
                syntax_text(SyntaxClass::HanComment, 200),
            ),
            (
                "han_comment",
                2000,
                syntax_text(SyntaxClass::HanComment, 2000),
            ),
        ],
    )
    .await;
}

/// 测量整段和各类别桶，并打印 token 差是否相加。
///
/// **作用**：Rust 函数和 JSON 各测 200 字与 2000 字。2000 字是 200 字重复 10 次。
/// **参数**：`client` 是已测完前缀的 DeepSeek 客户端，本函数会继续用它拨号。`prompt_before` 是前缀的 `prompt_tokens`。`sample` 是 `rust` 或 `json`。`pieces` 是这一段的有序片段。
/// **边界**：不打印正文。某一类失败时不打印相加结果。类别桶是同类片段直接拼接，中间原来的其他类别不在桶里。
async fn live_whole_sample(
    client: &mut DeepSeekClient,
    prompt_before: u64,
    sample: &str,
    pieces: &[Piece],
) {
    let base = whole_text(pieces);
    let labels = labels_in_order(pieces);
    for copies in [1usize, 10] {
        let whole = base.repeat(copies);
        let chars = whole.chars().count();
        let user = format!("{LIVE_PREFIX_USER}{whole}");
        let whole_after = live_prompt(client, &user).await;
        let whole_tokens = match &whole_after {
            Ok(tokens) => Some(*tokens),
            Err(_) => None,
        };
        print_probe(
            &format!("{sample}_whole"),
            chars,
            prompt_before,
            whole_after,
        );

        let mut part_sum = 0u64;
        let mut complete = true;
        for label in &labels {
            let bucket = bucket_text(pieces, label).repeat(copies);
            let bucket_chars = bucket.chars().count();
            let user = format!("{LIVE_PREFIX_USER}{bucket}");
            let after = live_prompt(client, &user).await;
            if let Ok(tokens) = &after {
                match tokens.checked_sub(prompt_before) {
                    Some(delta) => part_sum = part_sum.saturating_add(delta),
                    None => complete = false,
                }
            } else {
                complete = false;
            }
            print_probe(
                &format!("{sample}_{label}"),
                bucket_chars,
                prompt_before,
                after,
            );
        }

        if complete {
            if let Some(tokens) = whole_tokens {
                if let Some(whole_delta) = tokens.checked_sub(prompt_before) {
                    println!(
                        "additivity sample={sample} chars={chars} part_sum={part_sum} whole_delta={whole_delta} http=completed"
                    );
                }
            }
        }
    }
}

/// 完整 Rust 函数和 JSON 工具输出的实调。默认不跑。
///
/// **作用**：测量两段正文的 200 字与 2000 字，并比较类别桶 token 差之和与整段 token 差。
/// **边界**：不重测已有切片。没有密钥时不拨号。不写 Run、档案或记录文件。stdout 不含正文和密钥。
#[tokio::test]
#[ignore = "dials api.deepseek.com when DEEPSEEK_API_KEY is set"]
async fn live_whole_additivity_when_deepseek_client_response() {
    let Some((mut client, prompt_before)) = live_shared_prefix().await else {
        return;
    };
    live_whole_sample(&mut client, prompt_before, "rust", &rust_pieces()).await;
    live_whole_sample(&mut client, prompt_before, "json", &json_pieces()).await;
}
