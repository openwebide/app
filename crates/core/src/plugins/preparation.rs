//! Host-owned source preparation, separate from recording an installation.
use super::PreparedPlugin;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreparationState {
    Queued,
    Preparing,
    Ready,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginPreparation {
    pub id: String,
    pub state: PreparationState,
    pub prepared: Option<PreparedPlugin>,
    pub error: Option<String>,
}
