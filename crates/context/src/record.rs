//! context 记录。独立 blob 的 schema 1，不进入 `agent-contract`。
//!
//! **作用**：`prepare` 写出记录；`replay` 按记录重建视图。
//! **边界**：只有引用和数值。不含 prompt、工具正文或 `reasoning_content`。

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::strategy::IDENTITY_STRATEGY;

pub(crate) const SCHEMA_VERSION: u32 = 1;

/// 一次 `prepare` 的记录。
///
/// **作用**：以后作为 blob 保存。事件只存它的引用。
/// **边界**：`schema_version` 为 1 时 `spec` 只有硬上限。`edits` 在恒等策略下为空。
/// `Debug` 只含这些数和短标识。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextRecord {
    /// 记录 schema。当前只写出 `1`。其它值在 `replay` 时返回 [`ReplayError::UnsupportedSchema`]。
    pub schema_version: u32,
    /// 档案版本，与档案文件里的 `profile_version` 相同。从 1 起。
    pub profile_version: u32,
    /// 档案文件 UTF-8 字节的 sha256，64 位小写十六进制，无 `sha256:` 前缀。
    pub profile_hash: String,
    /// 策略标识。成功和耗尽的 `prepare` 写 `l0@1`。`identity()` 仍写 `identity@1`。空串会被 `replay` 当成未知策略。
    pub context_strategy: String,
    /// 这次调用的派生规格。schema 1 只有硬上限。
    pub spec: ContextSpec,
    /// 这次调用的 token 估算。不含正文。
    pub estimate: Estimate,
    /// 各策略追加的编辑。恒等策略必须是空数组。L0 的元素是不含正文的 edit 对象。
    pub edits: Vec<serde_json::Value>,
    /// 能否按记录重建视图。`false` 时 `replay` 返回 [`ReplayError::NotReplayable`]，不猜视图。
    pub replayable: bool,
}

/// schema 1 的派生规格。
///
/// **作用**：恒等策略只记录硬上限。软触发、目标和保护窗口留给 `mask@1`。
/// **边界**：JSON 只有 `hard_limit`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSpec {
    /// 本次请求允许的 prompt token 上限，单位是 token，已经向下取整。0 表示没有可用输入空间，此时不发请求；正上限在估算超过它时不发请求。
    pub hard_limit: u64,
}

/// 这次调用的 token 估算。
///
/// **作用**：和硬上限比较，并留给以后的 trace。
/// **边界**：`source` 为 `usage` 或 `estimated`。没有锚点时 `anchor_round` 是 JSON `null`，键仍在。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Estimate {
    /// 估算的 token 数，本地部分向上取整。0 表示没有可计数内容，不表示未知。
    pub tokens: u64,
    /// 数从哪里来。`usage` 时 [`Self::anchor_round`] 必为 `Some`；`estimated` 时必为 `None`。
    pub source: EstimateSource,
    /// 用量所在模型轮，从 1 起，与 `trace` 的 `#k` 相同。完整用量用于锚点；只有 prompt 已知时也可用于下界。`None` 表示没有使用服务端用量。
    pub anchor_round: Option<u32>,
}

/// 估算数从哪里来。
///
/// **作用**：区分服务端锚点和全文字符估算。
/// **边界**：线上字符串只有 `usage` 与 `estimated`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimateSource {
    /// 完整锚点的 prompt + completion 加新增内容估算，或已知 prompt 提供的下界。
    Usage,
    /// 没有可用锚点，整段可计数字符向上取整。
    Estimated,
}

/// `replay` 不能重建视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    /// `schema_version` 不是 1。
    UnsupportedSchema {
        /// 记录里的 schema 版本，不是 1。
        schema_version: u32,
    },
    /// 记录里的策略不是 `identity@1` 或 `l0@1`。
    UnknownStrategy {
        /// 记录里的 `context_strategy` 原文。空串也会出现在这里。
        id: String,
    },
    /// 记录标明不能重放。
    NotReplayable,
    /// 恒等策略的记录里出现了编辑。
    IdentityHasEdits,
    /// 估算来源与锚点不一致，或锚点为 0、超出事实轮次。
    InvalidEstimate,

    /// 一条 edit 不能应用到事实视图。
    ///
    /// **作用**：L0 重放拒绝缺字段、错误的 `op`、越界序号或对不上的 `result_ref`。
    /// **边界**：不区分是哪一种。不含工具正文。
    InvalidEdit,
}

impl Estimate {
    /// 校验估算来源与用量轮次是否一致。
    ///
    /// **作用**：反序列化与 replay 共用，避免手工构造的记录绕过检查。
    /// **边界**：usage 必须有从 1 起的轮次；estimated 必须没有轮次。事实轮数由 replay 另查。
    pub(crate) fn validate(&self) -> Result<(), ReplayError> {
        match (self.source, self.anchor_round) {
            (EstimateSource::Usage, Some(1..)) | (EstimateSource::Estimated, None) => Ok(()),
            _ => Err(ReplayError::InvalidEstimate),
        }
    }
}

impl<'de> Deserialize<'de> for Estimate {
    /// 读回估算并检查来源与锚点。
    ///
    /// **作用**：拒绝缺键、未知键和内部矛盾的记录。
    /// **参数**：`deserializer` 提供估算对象。`anchor_round` 必须存在，允许显式 null。
    /// **边界**：不重新估算 token，也不检查事实轮数。
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WireEstimate::deserialize(deserializer)?;
        let estimate = Self {
            tokens: wire.tokens,
            source: wire.source,
            anchor_round: wire.anchor_round,
        };
        estimate.validate().map_err(serde::de::Error::custom)?;
        Ok(estimate)
    }
}

/// 估算的读取形状，显式要求可空的锚点键存在。
///
/// **作用**：通过字段读取函数关闭 serde 对缺失 Option 的隐式默认。
/// **边界**：未知键拒绝；来源与轮次的关联在转成 Estimate 后检查。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireEstimate {
    /// 与 Estimate::tokens 相同，单位 token，0 允许。
    tokens: u64,
    /// 与 Estimate::source 相同，只允许 usage 或 estimated。
    source: EstimateSource,
    /// 必需键。显式 null 表示无用量轮次，正整数表示从 1 起的轮次。
    #[serde(deserialize_with = "required_anchor")]
    anchor_round: Option<u32>,
}

/// 读取必须存在、但值可以为空的锚点字段。
///
/// **作用**：让缺少字段产生 serde 缺键错误，而不是自动得到 None。
/// **参数**：`deserializer` 提供显式 null 或 u32 数值；其它类型拒绝。
/// **边界**：0 在 Estimate::validate 中拒绝。
fn required_anchor<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<u32>::deserialize(deserializer)
}

impl ContextRecord {
    /// 组装一条恒等策略记录。
    ///
    /// **作用**：`prepare` 用它打底，再把策略名改成 `l0@1`。直接调用时策略标识是 `identity@1`。
    /// **参数**：`profile_version` 写入记录，从 1 起。`profile_hash` 是 64 位小写十六进制，原样保存。`spec` 只含硬上限。`estimate` 是这次的 token 估算，其 `source` 与 `anchor_round` 必须一致。
    /// **边界**：`edits` 为空，`replayable` 为真。不写入任何正文。
    pub(crate) fn identity(
        profile_version: u32,
        profile_hash: String,
        spec: ContextSpec,
        estimate: Estimate,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            profile_version,
            profile_hash,
            context_strategy: IDENTITY_STRATEGY.to_owned(),
            spec,
            estimate,
            edits: Vec::new(),
            replayable: true,
        }
    }
}

impl Serialize for ContextRecord {
    /// 按固定键序写出 schema 1。
    ///
    /// **作用**：blob 与单测对照同一份 JSON。
    /// **参数**：`serializer` 是 serde 给出的写入端。这里按固定顺序写出 8 个键。
    /// **边界**：不写出正文。`anchor_round` 即使是 null 也保留键。`edits` 空数组也保留。
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ContextRecord", 8)?;
        state.serialize_field("schema_version", &self.schema_version)?;
        state.serialize_field("profile_version", &self.profile_version)?;
        state.serialize_field("profile_hash", &self.profile_hash)?;
        state.serialize_field("context_strategy", &self.context_strategy)?;
        state.serialize_field("spec", &self.spec)?;
        state.serialize_field("estimate", &self.estimate)?;
        state.serialize_field("edits", &self.edits)?;
        state.serialize_field("replayable", &self.replayable)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ContextRecord {
    /// 读回 schema 1 的记录。
    ///
    /// **作用**：`replay` 的输入。未知键拒绝。
    /// **参数**：`deserializer` 是 serde 给出的读取端。缺任一必需键时由它返回错误。
    /// **边界**：不在这里检查策略名是否存在；那是 [`crate::ContextManager::replay`] 的事。
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WireRecord::deserialize(deserializer)?;
        Ok(Self {
            schema_version: wire.schema_version,
            profile_version: wire.profile_version,
            profile_hash: wire.profile_hash,
            context_strategy: wire.context_strategy,
            spec: wire.spec,
            estimate: wire.estimate,
            edits: wire.edits,
            replayable: wire.replayable,
        })
    }
}

/// 反序列化用的中间形状，字段与 [`ContextRecord`] 一一对应。
///
/// **作用**：打开 `deny_unknown_fields`，避免手写 visitor。
/// **边界**：不对外公开。读完就拆进 [`ContextRecord`]。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRecord {
    /// 与 [`ContextRecord::schema_version`] 相同。
    schema_version: u32,
    /// 与 [`ContextRecord::profile_version`] 相同。
    profile_version: u32,
    /// 与 [`ContextRecord::profile_hash`] 相同。
    profile_hash: String,
    /// 与 [`ContextRecord::context_strategy`] 相同。
    context_strategy: String,
    /// 与 [`ContextRecord::spec`] 相同。未知键在这一层拒绝。
    spec: ContextSpec,
    /// 与 [`ContextRecord::estimate`] 相同。`anchor_round` 允许 JSON null。
    estimate: Estimate,
    /// 与 [`ContextRecord::edits`] 相同。缺席不是空数组，反序列化会失败。
    edits: Vec<serde_json::Value>,
    /// 与 [`ContextRecord::replayable`] 相同。
    replayable: bool,
}

impl std::fmt::Display for ReplayError {
    /// 说明重放被拒绝的原因。
    ///
    /// **作用**：校验器以后可以把未知策略标成 `unverifiable`。
    /// **参数**：`f` 是 `Display` 的输出目标。只写入版本号或策略标识。
    /// **边界**：只打印策略标识或版本号，不打印记录里没有的正文。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSchema { schema_version } => {
                write!(f, "unsupported context schema {schema_version}")
            }
            Self::UnknownStrategy { id } => write!(f, "unknown context strategy {id}"),
            Self::NotReplayable => write!(f, "context record is not replayable"),
            Self::IdentityHasEdits => write!(f, "identity record contains edits"),
            Self::InvalidEstimate => write!(f, "invalid context estimate anchor"),
            Self::InvalidEdit => write!(f, "context edit cannot be replayed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 两种合法来源都保持完整 JSON 形状，并可往返读取；显式 null 不等于缺键。
    #[test]
    fn estimate_json_round_trips_with_required_anchor_key() {
        for (source, anchor_round, expected) in [
            (
                EstimateSource::Usage,
                Some(2),
                json!({"tokens": 42, "source": "usage", "anchor_round": 2}),
            ),
            (
                EstimateSource::Estimated,
                None,
                json!({"tokens": 42, "source": "estimated", "anchor_round": null}),
            ),
        ] {
            let estimate = Estimate {
                tokens: 42,
                source,
                anchor_round,
            };
            let value = serde_json::to_value(&estimate).expect("json");
            assert_eq!(value, expected);
            assert_eq!(
                serde_json::from_value::<Estimate>(value).expect("parse"),
                estimate
            );
        }
    }

    /// 缺键、矛盾来源、非正轮次、错误类型和未知字段都必须拒绝。
    #[test]
    fn estimate_json_rejects_invalid_anchors() {
        for value in [
            json!({"tokens": 1, "source": "estimated"}),
            json!({"tokens": 1, "source": "usage"}),
            json!({"tokens": 1, "source": "usage", "anchor_round": null}),
            json!({"tokens": 1, "source": "usage", "anchor_round": 0}),
            json!({"tokens": 1, "source": "usage", "anchor_round": -1}),
            json!({"tokens": 1, "source": "usage", "anchor_round": "1"}),
            json!({"tokens": 1, "source": "estimated", "anchor_round": 1}),
            json!({"tokens": 1, "source": "estimated", "anchor_round": null, "extra": 1}),
        ] {
            assert!(
                serde_json::from_value::<Estimate>(value.clone()).is_err(),
                "{value}"
            );
        }
    }
}
