//! DeepSeek Flash 的模型档案。编译进本 crate，运行时不猜缺失字段。
//!
//! **作用**：硬上限读取窗口、输出是否占窗口，以及临时的 `estimate_error`。
//! **边界**：没有 `value` 的字段保持未知。单价不进入估算。

use sha2::{Digest, Sha256};

const EMBEDDED_DEEPSEEK_FLASH: &str = include_str!("../profiles/deepseek-deepseek-flash.toml");

const REQUIRED_NAMES: &[&str] = &[
    "provider",
    "alias",
    "doc_version",
    "context_window",
    "window_counts_output",
    "max_output",
    "reasoning_in_output",
    "tokenizer",
    "token_ratio_han",
    "token_ratio_ascii_letter",
    "token_ratio_ascii_digit",
    "token_ratio_repeated_a",
    "token_ratio_repeated_fn",
    "usage_fields",
    "estimate_error",
    "cache_ratio",
    "cache_mode",
    "cache_granularity",
    "cache_ttl",
    "system_update",
    "tool_update",
    "effective_context",
    "context_awareness",
    "reasoning_passback",
    "tool_pairing",
    "native_context_mgmt",
    "price_miss",
    "price_hit",
    "price_output",
];

/// 档案里一个字段的可信度。与执行方案用词一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credibility {
    /// 文档已经写出这个值。
    Documented,
    /// 现有运行已经见到这个值。
    Measured,
    /// 执行方案写明的假设，还没有实测记录。
    Hypothesis,
    /// 尚未测完。可以没有取值。
    Unknown,
}

/// 档案中的一个字段。
///
/// **作用**：保留来源和可信度，供以后的实测记录对上号。
/// **边界**：`value` 为 `None` 时表示没有取值。`Debug` 可以打印这些短字符串。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileField {
    /// 字段名，与 toml 的 `name` 相同。同一份档案里不重复。
    pub name: String,
    /// 文档、实测、假设或未知。未知仍可以带临时取值。
    pub credibility: Credibility,
    /// 取值的字符串形式。`None` 表示没有取值，不是空串，也不是 0。空串在加载时被拒绝。
    pub value: Option<String>,
    /// 来源说明，给人把字段对到执行方案或实测记录。不在这里访问网络。
    pub source: String,
    /// `true` 表示临时保守值，测完要替换。`false` 不表示已经实测。
    pub uncalibrated: bool,
}

/// 估算和硬上限实际读取的档案。
///
/// **作用**：`deepseek_flash` 从嵌入的 toml 构造。`profile_hash` 是该文件字节的 sha256 小写十六进制。
/// **边界**：不保存单价数字。`estimate_error` 仍是未校准的 0.30；T2 第二步的 p99 没有低于它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelProfile {
    /// 档案版本，从 1 起。与嵌入 toml 的 `profile_version` 相同。当前加载器只接受 2。
    pub profile_version: u32,
    /// 档案 toml 原文 UTF-8 字节的 sha256，64 位小写十六进制，无前缀。
    pub profile_hash: String,
    /// 服务商名，例如 `deepseek`。来自字段 `provider`。
    pub provider: String,
    /// 模型别名，例如 `deepseek-flash`。来自字段 `alias`。
    pub alias: String,
    /// 文档所列模型版本，例如 `V4.1-Flash-1017`。不合并 `doc_version` 小表。
    pub doc_version: String,
    /// 上下文窗口，单位是 token。DeepSeek Flash 当前为 1048576。
    pub context_window: u64,
    /// 单次输出上限，单位是 token。硬上限不用它，输出预留用请求的 `max_tokens`。
    pub max_output: u64,
    /// 窗口是否包含本次输出。`true` 时硬上限先扣掉 `max_tokens`。当前档案取假设值 `true`，可信度仍是未知。
    pub window_counts_output: bool,
    /// `estimate_error` 的分子。与 [`Self::estimate_error_den`] 一起表示临时误差，当前是 30/100。
    pub estimate_error_num: u64,
    /// `estimate_error` 的分母，必须大于分子。0 不会从合法档案里读出。
    pub estimate_error_den: u64,
    /// 汉字比例的分子。与 [`Self::han_den`] 一起表示每字 token。档案 v2 是 1。
    pub han_num: u64,
    /// 汉字比例的分母。
    pub han_den: u64,
    /// ASCII 英文字母比例的分子。与 [`Self::letter_den`] 一起。档案 v2 仍是文档比例 0.3，不改用重复 `a` 或 `fn` 的实测。
    pub letter_num: u64,
    /// ASCII 英文字母比例的分母。
    pub letter_den: u64,
    /// ASCII 数字比例的分子。与 [`Self::digit_den`] 一起。档案 v2 取 T8 两档里较高的 0.335。
    pub digit_num: u64,
    /// ASCII 数字比例的分母。
    pub digit_den: u64,
    /// 档案里的全部字段，含没有取值的未知项。估算只读上面拆出来的数，不扫描这个列表。
    pub fields: Vec<ProfileField>,
}

/// 档案文本无法载入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileError {
    /// toml 语法或类型不对。
    Toml,
    /// `profile_version` 不是本加载器认识的版本。
    Version(u32),
    /// 同一个 `name` 出现两次。
    DuplicateField(String),
    /// 必需名字不在文件里。
    MissingField(String),
    /// 文档、实测或假设字段没有 `value`。
    MissingValue(String),
    /// 整数、布尔或比例字符串无法按约定解析。
    BadValue {
        /// 没通过解析的字段名。
        name: String,
        /// 原始 `value`。空串表示 toml 里写了空值。
        value: String,
    },
}

/// 载入嵌入的 `deepseek-deepseek-flash` 档案。
///
/// **作用**：P1 唯一的内置档案。每次调用都解析同一段编译进二进制的文本。
/// **边界**：文件写错时返回 [`ProfileError`]，不改用另一套默认数。
pub fn deepseek_flash() -> Result<ModelProfile, ProfileError> {
    load(EMBEDDED_DEEPSEEK_FLASH)
}

impl ModelProfile {
    /// 本次请求的硬上限。
    ///
    /// **作用**：`prepare` 用它和估算比较。
    /// **参数**：`max_tokens` 是这次请求的输出上限。窗口包含输出时，它整段从窗口里扣掉。
    /// **边界**：扣完小于等于 0 时硬上限为 0。临时 30% 误差向下取整。
    pub fn hard_limit(&self, max_tokens: u32) -> u64 {
        let reserve = if self.window_counts_output {
            u64::from(max_tokens)
        } else {
            0
        };
        crate::text::floor_hard_limit(
            self.context_window,
            reserve,
            self.estimate_error_num,
            self.estimate_error_den,
        )
    }

    /// 按名字取字段。只给档案测试用。
    ///
    /// **作用**：断言未知字段没有取值。
    /// **参数**：`name` 是 toml 里的 `name`。
    /// **边界**：没有这个名字时返回 `None`，不补默认字段。
    #[cfg(test)]
    fn field(&self, name: &str) -> Option<&ProfileField> {
        self.fields.iter().find(|field| field.name == name)
    }
}

/// 解析一份档案文本。
///
/// **作用**：嵌入文件和单测里的坏样本都走这里。`profile_hash` 覆盖 `text` 的 UTF-8 字节。
/// **参数**：`text` 是完整 toml，不是单字段。
/// **边界**：必需字段缺席、文档字段没有值、比例不是约定字符串时返回错误。
pub(crate) fn load(text: &str) -> Result<ModelProfile, ProfileError> {
    let file: ProfileFile = toml::from_str(text).map_err(|_| ProfileError::Toml)?;
    if file.profile_version != 2 {
        return Err(ProfileError::Version(file.profile_version));
    }
    let mut fields = Vec::with_capacity(file.fields.len());
    for raw in file.fields {
        if fields
            .iter()
            .any(|field: &ProfileField| field.name == raw.name)
        {
            return Err(ProfileError::DuplicateField(raw.name));
        }
        let credibility =
            Credibility::parse(&raw.credibility).map_err(|_| ProfileError::BadValue {
                name: raw.name.clone(),
                value: raw.credibility,
            })?;
        if raw.value.as_ref().is_some_and(|value| value.is_empty()) {
            return Err(ProfileError::BadValue {
                name: raw.name,
                value: String::new(),
            });
        }
        if matches!(
            credibility,
            Credibility::Documented | Credibility::Measured | Credibility::Hypothesis
        ) && raw.value.is_none()
        {
            return Err(ProfileError::MissingValue(raw.name));
        }
        fields.push(ProfileField {
            name: raw.name,
            credibility,
            value: raw.value,
            source: raw.source,
            uncalibrated: raw.uncalibrated,
        });
    }
    for name in REQUIRED_NAMES {
        if !fields.iter().any(|field| field.name == *name) {
            return Err(ProfileError::MissingField((*name).to_owned()));
        }
    }
    let profile = ModelProfile {
        profile_version: file.profile_version,
        profile_hash: sha256_hex(text.as_bytes()),
        provider: required(&fields, "provider")?.to_owned(),
        alias: required(&fields, "alias")?.to_owned(),
        doc_version: required(&fields, "doc_version")?.to_owned(),
        context_window: parse_u64(&fields, "context_window")?,
        max_output: parse_u64(&fields, "max_output")?,
        window_counts_output: parse_bool(&fields, "window_counts_output")?,
        estimate_error_num: parse_ratio(&fields, "estimate_error")?.0,
        estimate_error_den: parse_ratio(&fields, "estimate_error")?.1,
        han_num: parse_ratio(&fields, "token_ratio_han")?.0,
        han_den: parse_ratio(&fields, "token_ratio_han")?.1,
        letter_num: parse_ratio(&fields, "token_ratio_ascii_letter")?.0,
        letter_den: parse_ratio(&fields, "token_ratio_ascii_letter")?.1,
        digit_num: parse_ratio(&fields, "token_ratio_ascii_digit")?.0,
        digit_den: parse_ratio(&fields, "token_ratio_ascii_digit")?.1,
        fields,
    };
    Ok(profile)
}

/// 读取字段取值。没有 `value` 时视为缺失。
///
/// **作用**：硬上限和身份字段必须有字面值。
/// **参数**：`fields` 是已经去重的档案字段，空切片会找不到名字。`name` 是 toml 里的字段名，大小写敏感。
/// **边界**：未知且未填值的字段走这里会得到 [`ProfileError::MissingValue`]。
fn required<'a>(fields: &'a [ProfileField], name: &str) -> Result<&'a str, ProfileError> {
    fields
        .iter()
        .find(|field| field.name == name)
        .and_then(|field| field.value.as_deref())
        .ok_or_else(|| ProfileError::MissingValue(name.to_owned()))
}

/// 把字段取值解析成非负整数。
///
/// **作用**：读取窗口和输出上限。
/// **参数**：`fields` 是档案字段。`name` 对应的 `value` 必须是十进制非负整数，不允许空串、小数和负数。
/// **边界**：解析失败返回 [`ProfileError::BadValue`]，不换成 0。
fn parse_u64(fields: &[ProfileField], name: &str) -> Result<u64, ProfileError> {
    let value = required(fields, name)?;
    value.parse::<u64>().map_err(|_| ProfileError::BadValue {
        name: name.to_owned(),
        value: value.to_owned(),
    })
}

/// 把字段取值解析成布尔。
///
/// **作用**：读取 `window_counts_output` 这类已经写下的临时取值。
/// **参数**：`fields` 是档案字段。`name` 对应的 `value` 只能是 `true` 或 `false`。
/// **边界**：不把缺席当成 false。其它字符串返回 [`ProfileError::BadValue`]。
fn parse_bool(fields: &[ProfileField], name: &str) -> Result<bool, ProfileError> {
    match required(fields, name)? {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(ProfileError::BadValue {
            name: name.to_owned(),
            value: other.to_owned(),
        }),
    }
}

/// 把十进制小数字符串解析成分子和分母。
///
/// **作用**：避免用浮点保存比例。接入新模型或写入实测比例时只改档案文件。
/// **参数**：`fields` 是档案字段。`name` 对应的 `value` 形如 `0.6`、`0.30` 或 `1`。
/// **边界**：不接受符号、指数、空串、`.5`、`5.`。整数部分和小数部分各最多 9 位。不把无法识别的比例换成 0。
fn parse_ratio(fields: &[ProfileField], name: &str) -> Result<(u64, u64), ProfileError> {
    let value = required(fields, name)?;
    parse_decimal(value).ok_or_else(|| ProfileError::BadValue {
        name: name.to_owned(),
        value: value.to_owned(),
    })
}

/// 十进制小数字符串转 `(分子, 分母)`，分母是 10 的幂，不约分。
///
/// **作用**：`parse_ratio` 的纯函数部分，便于单测。
/// **参数**：`text` 是档案里的 `value`，不含引号。空串、符号和指数都不接受。
/// **边界**：格式不合法或位数过多时返回 `None`。
fn parse_decimal(text: &str) -> Option<(u64, u64)> {
    const MAX_DIGITS: usize = 9;
    let (int_part, frac_part) = match text.split_once('.') {
        Some((int_part, frac_part)) if !frac_part.is_empty() => (int_part, frac_part),
        Some(_) => return None,
        None => (text, ""),
    };
    let all_digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    if int_part.is_empty()
        || int_part.len() > MAX_DIGITS
        || frac_part.len() > MAX_DIGITS
        || !all_digits(int_part)
        || !all_digits(frac_part)
    {
        return None;
    }
    let denominator = 10u64.pow(frac_part.len() as u32);
    let integer: u64 = int_part.parse().ok()?;
    let fraction: u64 = if frac_part.is_empty() {
        0
    } else {
        frac_part.parse().ok()?
    };
    Some((integer * denominator + fraction, denominator))
}

/// 档案文件字节的 sha256，小写十六进制，不加前缀。
///
/// **作用**：写入记录的 `profile_hash`。
/// **参数**：`bytes` 是嵌入 toml 的原始 UTF-8。
/// **边界**：不哈希解析后的结构，避免键序变化被当成同一份档案。
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

impl Credibility {
    /// 把档案里的中文可信度换成枚举。
    ///
    /// **作用**：toml 使用执行方案里的四个词。
    /// **参数**：`text` 只能是「文档」「实测」「假设」「未知」。
    /// **边界**：其它字符串返回错误，不映射成未知。
    fn parse(text: &str) -> Result<Self, ()> {
        match text {
            "文档" => Ok(Self::Documented),
            "实测" => Ok(Self::Measured),
            "假设" => Ok(Self::Hypothesis),
            "未知" => Ok(Self::Unknown),
            _ => Err(()),
        }
    }
}

impl std::fmt::Display for ProfileError {
    /// 说明哪一个档案字段或版本没有通过。
    ///
    /// **作用**：单测和加载失败时指出名字。
    /// **参数**：`f` 是 `Display` 的输出目标。字段名和坏取值会写入。
    /// **边界**：不打印整份 toml。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Toml => write!(f, "profile toml is invalid"),
            Self::Version(version) => write!(f, "unsupported profile_version {version}"),
            Self::DuplicateField(name) => write!(f, "duplicate profile field {name}"),
            Self::MissingField(name) => write!(f, "missing profile field {name}"),
            Self::MissingValue(name) => write!(f, "profile field {name} has no value"),
            Self::BadValue { name, value } => {
                write!(f, "profile field {name} has bad value {value}")
            }
        }
    }
}

/// 档案 toml 的顶层形状。
///
/// **作用**：`load` 的反序列化入口。
/// **边界**：多余的顶层键拒绝。不设默认版本。
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    /// 档案版本。加载器只接受 2。
    profile_version: u32,
    /// 全部字段，顺序不参与 hash 以外的比较。hash 用的是原文，不是这个列表。
    fields: Vec<RawField>,
}

/// toml 里的一条字段，取值还是字符串。
///
/// **作用**：先收下原文，再由 `load` 判断缺值是否合法。
/// **边界**：未知键拒绝。`value` 缺席变成 `None`，不变成空串。
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawField {
    /// 字段名。空串会留下来，随后因找不到必需名字或重复而失败。
    name: String,
    /// 原始取值。缺席表示未知且没有临时值。空串稍后被拒绝。
    #[serde(default)]
    value: Option<String>,
    /// 可信度原文，必须是「文档」「实测」「假设」「未知」之一。
    credibility: String,
    /// 来源说明。空串允许，不在这里检查链接是否可访问。
    source: String,
    /// 是否为临时保守值。toml 里缺这个键时是 `false`。
    #[serde(default)]
    uncalibrated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 嵌入档案的版本、临时误差、未知字段和 hash 与文件字节一致。
    #[test]
    fn embedded_deepseek_flash_keeps_unknowns_unfilled() {
        let profile = deepseek_flash().expect("embedded profile");
        assert_eq!(profile.profile_version, 2);
        assert_eq!((profile.han_num, profile.han_den), (1, 1));
        assert_eq!((profile.letter_num, profile.letter_den), (3, 10));
        assert_eq!((profile.digit_num, profile.digit_den), (335, 1000));
        assert_eq!(
            profile
                .field("token_ratio_repeated_a")
                .unwrap()
                .value
                .as_deref(),
            Some("0.125")
        );
        assert_eq!(
            profile
                .field("token_ratio_repeated_fn")
                .unwrap()
                .value
                .as_deref(),
            Some("0.5")
        );
        assert_eq!(profile.alias, "deepseek-flash");
        assert_eq!(profile.doc_version, "V4.1-Flash-1017");
        assert_eq!(profile.context_window, 1_048_576);
        assert!(profile.window_counts_output);
        assert_eq!(
            (profile.estimate_error_num, profile.estimate_error_den),
            (30, 100)
        );
        assert!(profile.field("estimate_error").unwrap().uncalibrated);
        for name in ["cache_ttl", "effective_context", "price_miss", "cache_mode"] {
            let field = profile.field(name).unwrap();
            assert_eq!(field.credibility, Credibility::Unknown);
            assert!(field.value.is_none(), "{name}");
        }
        assert_eq!(
            profile.profile_hash,
            sha256_hex(EMBEDDED_DEEPSEEK_FLASH.as_bytes())
        );
        assert_eq!(profile.hard_limit(32_768), 711_065);
    }

    /// 档案版本不是 2 时拒绝，不按旧比例加载。
    #[test]
    fn profile_version_other_than_current_is_rejected() {
        let text =
            EMBEDDED_DEEPSEEK_FLASH.replacen("profile_version = 2", "profile_version = 1", 1);
        assert!(matches!(load(&text), Err(ProfileError::Version(1))));
    }

    /// 比例按十进制解析，档案换数不用改代码。
    #[test]
    fn decimal_ratios_parse_without_a_fixed_list() {
        assert_eq!(parse_decimal("0.30"), Some((30, 100)));
        assert_eq!(parse_decimal("0.6"), Some((6, 10)));
        assert_eq!(parse_decimal("0.125"), Some((125, 1000)));
        assert_eq!(parse_decimal("1"), Some((1, 1)));
        for bad in [
            "",
            ".5",
            "5.",
            "-0.1",
            "+0.1",
            "1e-1",
            "0.1.2",
            "a",
            "0.0000000001",
        ] {
            assert_eq!(parse_decimal(bad), None, "{bad}");
        }
    }

    /// 档案里改了 estimate_error，硬上限随之变化。
    #[test]
    fn changed_estimate_error_changes_the_hard_limit() {
        let text = EMBEDDED_DEEPSEEK_FLASH.replacen(
            "name = \"estimate_error\"\nvalue = \"0.30\"",
            "name = \"estimate_error\"\nvalue = \"0.12\"",
            1,
        );
        let profile = load(&text).expect("profile");
        assert_eq!(
            (profile.estimate_error_num, profile.estimate_error_den),
            (12, 100)
        );
        assert_eq!(profile.hard_limit(32_768), 893_911);
    }

    /// 文档字段没有 value 时拒绝，而不是填一个默认数。
    #[test]
    fn documented_field_without_value_is_rejected() {
        let mut text = EMBEDDED_DEEPSEEK_FLASH.to_owned();
        text = text.replacen(
            "name = \"context_window\"\nvalue = \"1048576\"\n",
            "name = \"context_window\"\n",
            1,
        );
        assert!(matches!(
            load(&text),
            Err(ProfileError::MissingValue(name)) if name == "context_window"
        ));
    }
}
