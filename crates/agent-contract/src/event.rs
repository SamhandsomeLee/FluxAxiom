use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::attempt::AttemptOutcome;
use crate::id::{ArtifactId, AttemptId, EventId, RunId};
use crate::run::TerminalReason;
use crate::time_stamp::Timestamp;
use crate::SCHEMA_VERSION;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenCount {
    Known(u64),
    Unknown,
}

impl Serialize for TokenCount {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Known(value) => serializer.serialize_u64(*value),
            Self::Unknown => serializer.serialize_str("unknown"),
        }
    }
}

impl<'de> Deserialize<'de> for TokenCount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct TokenVisitor;

        impl Visitor<'_> for TokenVisitor {
            type Value = TokenCount;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a non-negative integer or \"unknown\"")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(TokenCount::Known(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                u64::try_from(value)
                    .map(TokenCount::Known)
                    .map_err(E::custom)
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value == "unknown" {
                    Ok(TokenCount::Unknown)
                } else {
                    Err(E::custom("token count must be an integer or \"unknown\""))
                }
            }
        }

        deserializer.deserialize_any(TokenVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyPayload {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunTerminalPayload {
    pub terminal_reason: TerminalReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptStartedPayload {
    pub attempt_id: AttemptId,
    pub owner: String,
    pub lease_expires_at: Timestamp,
    /// 本次 drive 开始时采集的工作区版本（A4b）。旧事件没有该字段，反序列化为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_revision: Option<WorkspaceRevision>,
}

/// 工作区版本快照（A4b）。写入 `attempt.started`，也出现在样本导出里。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "vcs", rename_all = "lowercase")]
pub enum WorkspaceRevision {
    /// git 仓库：`commit` 为 40 位 hex；`prefix` 是工作区相对仓库根的路径（根上为空串）。
    Git {
        commit: String,
        dirty: bool,
        prefix: String,
    },
    /// 非 git、找不到 git、超时或失败（含无提交的新仓库）。
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptEndedPayload {
    pub attempt_id: AttemptId,
    pub outcome: AttemptOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptOrphanedPayload {
    pub attempt_id: AttemptId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelUsage {
    pub prompt_tokens: TokenCount,
    pub completion_tokens: TokenCount,
    pub prompt_cache_hit_tokens: TokenCount,
    pub prompt_cache_miss_tokens: TokenCount,
}

/// 一次 `model.call_completed` 的 payload。
///
/// **作用**：Host 把模型观测落成事件。以后的校验器用 `context_ref` 找到 context 记录。
/// **边界**：不含 prompt、思维链或工具正文。`context_ref` 与 `context_strategy` 成对。
/// 同为缺席时 JSON 仍写出这两个键，值是 `null`。缺键和 `null` 都读成 `None`。
/// 只出现一个，或出现空串，反序列化失败。这里不检查 blob 是否存在。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "WireModelCallCompleted")]
pub struct ModelCallCompletedPayload {
    /// 模型别名。空串在这里不拒绝；Host 写入前拒绝。
    pub model_alias: String,
    /// 四项用量。未知项是 `unknown`，不是 0。
    pub usage: ModelUsage,
    /// Provider 的结束原因，或 Loop 写入的畸形标记。空串在这里不拒绝。
    pub finish_reason: String,
    /// 本次调用墙钟毫秒。0 合法。
    pub latency_ms: u64,
    /// 请求 blob 引用。通常是 `sha256:` 加十六进制。空串在这里不拒绝。
    pub request_ref: String,
    /// 响应 blob 引用。正文在 blob 里，可以含思维链。空串在这里不拒绝。
    pub response_ref: String,
    /// context 记录 blob。`None` 表示没有记录，JSON 为 `null`，不是省略键。不会是空串。
    pub context_ref: Option<String>,
    /// 策略标识，例如 `identity@1`。与 `context_ref` 同有同无。`None` 的 JSON 是 `null`。不会是空串。
    pub context_strategy: Option<String>,
}

/// `model.call_completed` 的反序列化形状。
///
/// **作用**：先收下缺键和 `null`，再由 [`TryFrom`] 检查两个 context 字段是否成对。
/// **边界**：未知键拒绝。`context_ref`、`context_strategy` 缺席时是 `None`，不是空串。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireModelCallCompleted {
    /// 与 [`ModelCallCompletedPayload::model_alias`] 相同。
    model_alias: String,
    /// 与 [`ModelCallCompletedPayload::usage`] 相同。
    usage: ModelUsage,
    /// 与 [`ModelCallCompletedPayload::finish_reason`] 相同。
    finish_reason: String,
    /// 与 [`ModelCallCompletedPayload::latency_ms`] 相同。
    latency_ms: u64,
    /// 与 [`ModelCallCompletedPayload::request_ref`] 相同。
    request_ref: String,
    /// 与 [`ModelCallCompletedPayload::response_ref`] 相同。
    response_ref: String,
    /// 缺键或 JSON `null` 都是 `None`。空串留给 [`TryFrom`] 拒绝。
    #[serde(default)]
    context_ref: Option<String>,
    /// 缺键或 JSON `null` 都是 `None`。必须与 `context_ref` 同有同无。
    #[serde(default)]
    context_strategy: Option<String>,
}

impl TryFrom<WireModelCallCompleted> for ModelCallCompletedPayload {
    type Error = &'static str;

    /// 把线上对象收成成对的 context 字段。
    ///
    /// **作用**：`model.call_completed` 反序列化时调用。
    /// **参数**：`wire` 是 JSON 对象。缺键和 JSON `null` 都已经变成 `None`。
    /// **边界**：两个字段必须同为 `None`，或同为非空字符串。空串、只出现一个时返回错误，不写成已知值。不检查 blob 是否存在。
    fn try_from(wire: WireModelCallCompleted) -> Result<Self, Self::Error> {
        context_pair(&wire.context_ref, &wire.context_strategy)?;
        Ok(Self {
            model_alias: wire.model_alias,
            usage: wire.usage,
            finish_reason: wire.finish_reason,
            latency_ms: wire.latency_ms,
            request_ref: wire.request_ref,
            response_ref: wire.response_ref,
            context_ref: wire.context_ref,
            context_strategy: wire.context_strategy,
        })
    }
}

/// 判断两个 context 字段是同缺席还是同为非空字符串。
///
/// **作用**：反序列化时拒绝半对。
/// **参数**：`context_ref` 是记录引用，`None` 表示缺席。`context_strategy` 是策略标识，`None` 表示缺席。空串不是缺席。
/// **边界**：空串返回对应字段名。只出现一个时，返回缺席的那一个的名字。不访问 blob。
fn context_pair(
    context_ref: &Option<String>,
    context_strategy: &Option<String>,
) -> Result<(), &'static str> {
    match (context_ref, context_strategy) {
        (None, None) => Ok(()),
        (Some(context_ref), Some(context_strategy)) => {
            if context_ref.is_empty() {
                Err("context_ref")
            } else if context_strategy.is_empty() {
                Err("context_strategy")
            } else {
                Ok(())
            }
        }
        (Some(_), None) => Err("context_strategy"),
        (None, Some(_)) => Err("context_ref"),
    }
}

/// 请求发送前因上下文耗尽而停止的诊断引用。
///
/// **作用**：Host 把失败的 prepare 记录关联到 Run 与当前 Attempt。
/// **边界**：不是模型调用，也不是 Run 终态；只保存引用与策略名，不解析 context 记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextExhaustedPayload {
    /// 已落盘的 context 记录引用。必需的非空字符串，不包含记录正文。
    #[serde(deserialize_with = "nonempty_context_text")]
    pub context_ref: String,
    /// 记录使用的策略标识，例如 identity@1。必需的非空字符串；未知策略仍可读取。
    #[serde(deserialize_with = "nonempty_context_text")]
    pub context_strategy: String,
}

/// 读取一个必需的非空 context 引用或策略名。
///
/// **作用**：耗尽事件的两个字段不能缺席、为 null 或为空串。
/// **参数**：`deserializer` 提供字段的字符串值；不在此验证引用格式和 blob 存在性。
/// **边界**：错误消息不回显输入值。Host 写入时另外检查 blob 已落盘。
fn nonempty_context_text<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    if text.is_empty() {
        return Err(de::Error::custom("context field must not be empty"));
    }
    Ok(text)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallCompletedPayload {
    /// 对应响应中的调用 ID；旧事件缺失时无法证明工具顺序。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub tool_name: String,
    pub normalized_path_or_query: String,
    pub result_ref: String,
    /// 返回给模型的字节数，与 `result_ref` 指向的内容等长。不是截断前的原始长度。
    pub byte_count: u64,
    /// 为 true 时，原始输出比 `byte_count` 更大，超出部分未交给模型。
    pub truncated: bool,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactCreatedPayload {
    pub artifact_id: ArtifactId,
    pub artifact_type: String,
    pub storage_ref: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event_type", content = "payload")]
pub enum EventBody {
    #[serde(rename = "run.accepted")]
    RunAccepted(EmptyPayload),
    #[serde(rename = "run.started")]
    RunStarted(EmptyPayload),
    #[serde(rename = "run.cancelling")]
    RunCancelling(EmptyPayload),
    #[serde(rename = "run.cancelled")]
    RunCancelled(RunTerminalPayload),
    #[serde(rename = "run.completed")]
    RunCompleted(RunTerminalPayload),
    #[serde(rename = "run.failed")]
    RunFailed(RunTerminalPayload),
    #[serde(rename = "attempt.started")]
    AttemptStarted(AttemptStartedPayload),
    #[serde(rename = "attempt.ended")]
    AttemptEnded(AttemptEndedPayload),
    #[serde(rename = "attempt.orphaned")]
    AttemptOrphaned(AttemptOrphanedPayload),
    #[serde(rename = "model.call_completed")]
    ModelCallCompleted(ModelCallCompletedPayload),
    #[serde(rename = "context.exhausted")]
    ContextExhausted(ContextExhaustedPayload),
    #[serde(rename = "tool.call_completed")]
    ToolCallCompleted(ToolCallCompletedPayload),
    #[serde(rename = "artifact.created")]
    ArtifactCreated(ArtifactCreatedPayload),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub event_id: EventId,
    pub schema_version: String,
    pub run_id: RunId,
    pub sequence: u64,
    pub subject_id: String,
    pub occurred_at: Timestamp,
    #[serde(flatten)]
    pub body: EventBody,
}

impl Event {
    #[must_use]
    pub fn invariants_hold(&self) -> bool {
        self.schema_version == SCHEMA_VERSION && self.sequence >= 1 && !self.subject_id.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 旧 `attempt.started` 无 `workspace_revision` 仍可反序列化。
    #[test]
    fn attempt_started_without_revision_deserializes() {
        let value = json!({
            "event_id": "evt_01936c2f-8a4b-7c3d-9e1f-2a3b4c5d6e90",
            "schema_version": "0.3-draft",
            "run_id": "run_01936c2f-8a4b-7c3d-9e1f-2a3b4c5d6e7f",
            "sequence": 2,
            "subject_id": "attempt_01936c2f-8a4b-7c3d-9e1f-2a3b4c5d6e91",
            "occurred_at": "2026-10-26T12:00:00Z",
            "event_type": "attempt.started",
            "payload": {
                "attempt_id": "attempt_01936c2f-8a4b-7c3d-9e1f-2a3b4c5d6e91",
                "owner": "pid:1",
                "lease_expires_at": "2026-10-26T12:01:00Z"
            }
        });
        let event: Event = serde_json::from_value(value).unwrap();
        match event.body {
            EventBody::AttemptStarted(payload) => {
                assert!(payload.workspace_revision.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    /// 没有 context 字段的模型事件读成 `None`，再写出显式 `null`。
    #[test]
    fn model_call_payload_writes_null_for_absent_context_fields() {
        let usage = json!({
            "prompt_tokens": 1,
            "completion_tokens": 2,
            "prompt_cache_hit_tokens": "unknown",
            "prompt_cache_miss_tokens": "unknown"
        });
        let legacy = json!({
            "model_alias": "deepseek-flash",
            "usage": usage,
            "finish_reason": "stop",
            "latency_ms": 3,
            "request_ref": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "response_ref": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        });
        let payload: ModelCallCompletedPayload =
            serde_json::from_value(legacy).expect("legacy payload");
        assert_eq!(payload.context_ref, None);
        assert_eq!(payload.context_strategy, None);
        assert_eq!(
            serde_json::to_value(&payload).expect("json"),
            json!({
                "model_alias": "deepseek-flash",
                "usage": {
                    "prompt_tokens": 1,
                    "completion_tokens": 2,
                    "prompt_cache_hit_tokens": "unknown",
                    "prompt_cache_miss_tokens": "unknown"
                },
                "finish_reason": "stop",
                "latency_ms": 3,
                "request_ref": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "response_ref": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "context_ref": null,
                "context_strategy": null
            })
        );

        let recorded = json!({
            "model_alias": "deepseek-flash",
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 2,
                "prompt_cache_hit_tokens": "unknown",
                "prompt_cache_miss_tokens": "unknown"
            },
            "finish_reason": "stop",
            "latency_ms": 3,
            "request_ref": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "response_ref": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "context_ref": "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            "context_strategy": "identity@1"
        });
        let parsed: ModelCallCompletedPayload =
            serde_json::from_value(recorded.clone()).expect("recorded");
        assert_eq!(serde_json::to_value(&parsed).expect("json"), recorded);

        for broken in [
            json!({
                "model_alias": "deepseek-flash",
                "usage": {
                    "prompt_tokens": 1,
                    "completion_tokens": 2,
                    "prompt_cache_hit_tokens": "unknown",
                    "prompt_cache_miss_tokens": "unknown"
                },
                "finish_reason": "stop",
                "latency_ms": 3,
                "request_ref": "sha256:aa",
                "response_ref": "sha256:bb",
                "context_ref": "sha256:cc",
                "context_strategy": null
            }),
            json!({
                "model_alias": "deepseek-flash",
                "usage": {
                    "prompt_tokens": 1,
                    "completion_tokens": 2,
                    "prompt_cache_hit_tokens": "unknown",
                    "prompt_cache_miss_tokens": "unknown"
                },
                "finish_reason": "stop",
                "latency_ms": 3,
                "request_ref": "sha256:aa",
                "response_ref": "sha256:bb",
                "context_strategy": "identity@1"
            }),
            json!({
                "model_alias": "deepseek-flash",
                "usage": {
                    "prompt_tokens": 1,
                    "completion_tokens": 2,
                    "prompt_cache_hit_tokens": "unknown",
                    "prompt_cache_miss_tokens": "unknown"
                },
                "finish_reason": "stop",
                "latency_ms": 3,
                "request_ref": "sha256:aa",
                "response_ref": "sha256:bb",
                "context_ref": "",
                "context_strategy": "identity@1"
            }),
        ] {
            assert!(
                serde_json::from_value::<ModelCallCompletedPayload>(broken).is_err(),
                "broken context pair"
            );
        }
    }
}
