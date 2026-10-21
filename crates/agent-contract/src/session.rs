use serde::{Deserialize, Serialize};

use crate::id::SessionId;
use crate::time_stamp::Timestamp;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    pub session_id: SessionId,
    pub created_at: Timestamp,
}
