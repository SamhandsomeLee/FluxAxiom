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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCallCompletedPayload {
    pub model_alias: String,
    pub usage: ModelUsage,
    pub finish_reason: String,
    pub latency_ms: u64,
    pub request_ref: String,
    pub response_ref: String,
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
}
