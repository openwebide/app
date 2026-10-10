//! Host-owned source preparation, separate from recording an installation.
use super::{PluginError, PluginSource, PreparedPlugin};
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum PreparationCommand {
    Start { source: PluginSource },
    Status { id: String },
    Cancel { id: String },
}

impl PreparationCommand {
    pub fn validate(&self) -> Result<(), PluginError> {
        match self {
            Self::Start { source } => source.validate(),
            Self::Status { id } | Self::Cancel { id } => validate_id(id),
        }
    }

    pub fn operation(&self) -> &'static str {
        match self {
            Self::Start { .. } => "start",
            Self::Status { .. } => "status",
            Self::Cancel { .. } => "cancel",
        }
    }

    pub fn host_payload(&self) -> serde_json::Value {
        match self {
            Self::Start { source } => serde_json::json!({"source":source}),
            Self::Status { id } | Self::Cancel { id } => serde_json::json!({"id":id}),
        }
    }
}

fn validate_id(id: &str) -> Result<(), PluginError> {
    if id.len() != 32
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(PluginError::Invalid(
            "Invalid plugin preparation id.".into(),
        ));
    }
    Ok(())
}

impl PluginPreparation {
    pub fn validate(&self) -> Result<(), PluginError> {
        validate_id(&self.id)?;
        match self.state {
            PreparationState::Ready if self.error.is_none() => self
                .prepared
                .as_ref()
                .ok_or_else(|| PluginError::Invalid("Missing prepared plugin receipt.".into()))?
                .validate(),
            PreparationState::Failed
                if self.prepared.is_none()
                    && self
                        .error
                        .as_ref()
                        .is_some_and(|error| !error.is_empty() && error.len() <= 256 * 1024) =>
            {
                Ok(())
            }
            PreparationState::Queued
            | PreparationState::Preparing
            | PreparationState::Cancelled
                if self.prepared.is_none() && self.error.is_none() =>
            {
                Ok(())
            }
            _ => Err(PluginError::Invalid(
                "Invalid plugin preparation response.".into(),
            )),
        }
    }
}
