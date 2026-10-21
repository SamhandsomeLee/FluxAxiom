use serde::{Deserialize, Serialize};

use crate::id::ArtifactId;
use crate::id::RunId;
use crate::time_stamp::Timestamp;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub artifact_id: ArtifactId,
    pub run_id: RunId,
    pub artifact_type: String,
    pub storage_ref: String,
    pub summary: String,
    pub created_at: Timestamp,
}
