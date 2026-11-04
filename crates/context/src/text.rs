//! 把正文里可计数的字符换成 token 估算。
//!
//! **作用**：供估算器使用。计入汉字、ASCII 英文字母和 ASCII 数字。
//! **边界**：空白、标点和全角数字不计数。不按语法拆源代码。重复字母的其它实测比例不在这里选用。

/// 一段文字里参与估算的字符数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Count {
    /// 汉字个数，含扩展区。0 表示这段没有汉字，不是未知。
    pub han: u64,
    /// ASCII `A–Z` / `a–z` 的个数。0 表示没有。全角拉丁字母不计入。
    pub ascii_letters: u64,
    /// ASCII `0–9` 的个数。0 表示没有。全角数字不计入。
    pub ascii_digits: u64,
}

/// 累加 `text` 里的汉字、ASCII 英文字母和 ASCII 数字。
///
/// **作用**：全量估算和新增内容估算都先走这里。
/// **参数**：`text` 可以是 prompt、思维链或工具正文。空串不改变计数。
/// **边界**：不把结果当服务端 token。汉字含扩展区。全角拉丁字母、全角数字、日文假名、韩文、空白和标点不计入。
/// 这些文字出现时估算会偏低，靠 `estimate_error` 兜底。
pub(crate) fn count_text(text: &str) -> Count {
    let mut count = Count::default();
    for ch in text.chars() {
        if is_han(ch) {
            count.han += 1;
        } else if ch.is_ascii_alphabetic() {
            count.ascii_letters += 1;
        } else if ch.is_ascii_digit() {
            count.ascii_digits += 1;
        }
    }
    count
}

impl Count {
    /// 与另一段计数相加。
    ///
    /// **作用**：把多段正文合成一次取整前的计数。
    /// **参数**：`other` 是另一段的汉字数、字母数和数字个数。
    /// **边界**：溢出时饱和，不回绕成小值。
    pub(crate) fn saturating_add(self, other: Self) -> Self {
        Self {
            han: self.han.saturating_add(other.han),
            ascii_letters: self.ascii_letters.saturating_add(other.ascii_letters),
            ascii_digits: self.ascii_digits.saturating_add(other.ascii_digits),
        }
    }
}

/// 按档案比例把计数向上取整成 token。
///
/// **作用**：得到写入 `estimate.tokens` 的本地部分。
/// **参数**：`count` 是已经分开的汉字数、字母数和数字个数，三项都为 0 时结果为 0。`han_num` / `han_den` 是汉字比例，分母必须非 0。`letter_num` / `letter_den` 是英文字母比例，分母必须非 0。`digit_num` / `digit_den` 是 ASCII 数字比例，分母必须非 0。
/// **边界**：三项都是 0 时返回 0。不四舍五入。任一比例分母为 0 时不能调用。结果超出 `u64` 时饱和。
pub(crate) fn ceil_tokens(
    count: Count,
    han_num: u64,
    han_den: u64,
    letter_num: u64,
    letter_den: u64,
    digit_num: u64,
    digit_den: u64,
) -> u64 {
    let han_den = han_den as u128;
    let letter_den = letter_den as u128;
    let digit_den = digit_den as u128;
    let numerator = (count.han as u128) * (han_num as u128) * letter_den * digit_den
        + (count.ascii_letters as u128) * (letter_num as u128) * han_den * digit_den
        + (count.ascii_digits as u128) * (digit_num as u128) * han_den * letter_den;
    let denominator = han_den * letter_den * digit_den;
    ceil_div(numerator, denominator)
}

/// `(window - reserve)` 乘以 `(1 - estimate_error)` 后向下取整。
///
/// **作用**：硬上限。误差比例是 `error_num / error_den`。
/// **参数**：`window` 是档案里的上下文窗口，单位 token。`reserve` 是这次请求的输出预留；大于 `window` 时结果为 0。`error_num` 是误差分子，大于等于分母时结果为 0。`error_den` 是误差分母，必须非 0。
/// **边界**：不用浮点。乘法溢出时由 `u128` 接住后再截成 `u64` 的地板除法。
pub(crate) fn floor_hard_limit(window: u64, reserve: u64, error_num: u64, error_den: u64) -> u64 {
    let Some(span) = window.checked_sub(reserve) else {
        return 0;
    };
    if error_num >= error_den {
        return 0;
    }
    let numerator = (span as u128) * ((error_den - error_num) as u128);
    (numerator / (error_den as u128)) as u64
}

/// 正整数除法向上取整。
///
/// **作用**：把汉字、字母和数字按各自比例相加后收成整数 token。
/// **参数**：`denominator` 必须非 0。`numerator` 为 0 时返回 0。
/// **边界**：结果超出 `u64` 时饱和为 `u64::MAX`，不回绕。
fn ceil_div(numerator: u128, denominator: u128) -> u64 {
    if numerator == 0 {
        return 0;
    }
    let rounded = numerator.saturating_add(denominator - 1) / denominator;
    u64::try_from(rounded).unwrap_or(u64::MAX)
}

/// 判断一个字符是不是汉字。
///
/// **作用**：估算前把可计数文字和未测文字分开。
/// **参数**：`ch` 是正文里的一个 Unicode 标量。
/// **边界**：覆盖统一表意文字、兼容表意文字和扩展 A–I。全角拉丁字母返回 false。
fn is_han(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0x20000..=0x2A6DF
            | 0x2A700..=0x2B73F
            | 0x2B740..=0x2B81F
            | 0x2B820..=0x2CEAF
            | 0x2CEB0..=0x2EBEF
            | 0x2EBF0..=0x2EE5F
            | 0x2F800..=0x2FA1F
            | 0x30000..=0x3134F
            | 0x31350..=0x323AF
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 锁住取整：两字母加两汉字是 1.8，向上为 2；四字母是 1.2，向上为 2。
    #[test]
    fn ceil_matches_tenths() {
        let two_and_two = Count {
            han: 2,
            ascii_letters: 2,
            ascii_digits: 0,
        };
        assert_eq!(ceil_tokens(two_and_two, 3, 5, 3, 10, 0, 1), 2);
        let four_letters = Count {
            han: 0,
            ascii_letters: 4,
            ascii_digits: 0,
        };
        assert_eq!(ceil_tokens(four_letters, 3, 5, 3, 10, 0, 1), 2);
        assert_eq!(ceil_tokens(Count::default(), 3, 5, 3, 10, 0, 1), 0);
    }

    /// 空白、标点和全角字母不计数。ASCII 数字和扩展区汉字计数。
    #[test]
    fn counts_han_letters_and_ascii_digits() {
        let count = count_text("a 1，。\nＡ\u{20000}");
        assert_eq!(count.ascii_letters, 1);
        assert_eq!(count.ascii_digits, 1);
        assert_eq!(count.han, 1);
    }

    /// 当前档案下，窗口扣掉 32768 再留 30% 后是 711065。
    #[test]
    fn hard_limit_for_temporary_error_margin() {
        assert_eq!(floor_hard_limit(1_048_576, 32_768, 30, 100), 711_065);
        assert_eq!(floor_hard_limit(100, 100, 30, 100), 0);
        assert_eq!(floor_hard_limit(100, 101, 30, 100), 0);
    }
}
