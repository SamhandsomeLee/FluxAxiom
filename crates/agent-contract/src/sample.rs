//! 样本记录：从已提交事实与 blob 派生的导出物（A4b），不是 durable fact。

use serde::{Deserialize, Serialize};

use crate::event::{ModelUsage, WorkspaceRevision};
use crate::id::{ArtifactId, RunId};
use crate::run::{Budget, RunStatus, TerminalReason};
use crate::time_stamp::Timestamp;

/// 一次 Run 的可长期保存样本记录。
///
/// **作用**：`flux sample export` 的输出；字段与 `contracts/schemas/sample.schema.json` 对齐。
/// **边界**：只含引用与摘要，不含 prompt / 工具正文 / `reasoning_content` / 报告正文。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sample {
    pub schema_version: String,
    pub run: SampleRun,
    pub workspace: SampleWorkspace,
    pub model: SampleModel,
    pub calls: Vec<SampleCall>,
    pub totals: SampleTotals,
    pub artifacts: Vec<SampleArtifact>,
    pub rebuild: RebuildResult,
    pub feedback: Vec<SampleFeedback>,
    pub exported_at: Timestamp,
}

/// 样本里的 Run 摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SampleRun {
    pub run_id: RunId,
    pub status: RunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<TerminalReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_seconds: Option<u64>,
    pub budget: Budget,
    pub model_calls_used: u32,
    pub tool_calls_used: u32,
}

/// 样本里的工作区。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SampleWorkspace {
    pub path: String,
    pub revision: WorkspaceRevision,
}

/// 样本里的模型身份与布局。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SampleModel {
    pub provider: String,
    pub alias: String,
    pub doc_version: String,
    pub doc_checked_at: String,
    pub reasoning_effort: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    pub prompt_layout_version: String,
    pub engine_id: String,
    pub engine_version: String,
}

/// 一次模型调用及其后工具。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SampleCall {
    pub index: u32,
    pub seq: u64,
    pub occurred_at: Timestamp,
    pub finish_reason: String,
    pub latency_ms: u64,
    pub usage: ModelUsage,
    pub request_ref: String,
    pub response_ref: String,
    pub tools: Vec<SampleToolCall>,
}

/// 一轮内的工具调用引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SampleToolCall {
    /// 新事件的调用关联 ID；历史样本允许缺失。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub tool_name: String,
    pub normalized_path_or_query: String,
    pub result_ref: String,
    pub byte_count: u64,
    pub truncated: bool,
    pub duration_ms: u64,
}

/// 样本合计。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SampleTotals {
    pub model_calls: u32,
    pub tool_calls: u32,
    pub prompt_tokens: crate::TokenCount,
    pub completion_tokens: crate::TokenCount,
    pub prompt_cache_hit_tokens: crate::TokenCount,
    pub prompt_cache_miss_tokens: crate::TokenCount,
}

/// 样本里的 Artifact 引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SampleArtifact {
    pub artifact_id: ArtifactId,
    pub artifact_type: String,
    pub storage_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

/// 可重建性三态（A4b）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RebuildResult {
    Consistent,
    Inconsistent { call: u32, rule: u8, detail: String },
    Unverifiable { missing: String },
}

/// 一条人工反馈。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SampleFeedback {
    pub feedback_id: crate::FeedbackId,
    pub created_at: Timestamp,
    pub verdict: FeedbackVerdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 人工总评三档（对应观察期「可直接用 / 需改 / 重做」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackVerdict {
    Usable,
    NeedsEdit,
    Redo,
}
