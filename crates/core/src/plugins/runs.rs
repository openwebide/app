//! Durable prompt submission primitives. Plugins own recurrence and result policy.
use serde::{Deserialize, Serialize};

pub const MAX_RUNS: i64 = 1000;
pub const RUN_PAGE_SIZE: i64 = 16;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunTarget {
    Origin,
    Session { id: i64 },
    New { title: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunRequest {
    List {
        #[serde(default)]
        after: i64,
    },
    Read {
        id: i64,
    },
    Submit {
        key: String,
        prompt: String,
        target: RunTarget,
        #[serde(default)]
        model: Option<crate::ModelSelection>,
    },
    Cancel {
        id: i64,
        revision: i64,
    },
    Delete {
        id: i64,
        revision: i64,
    },
}
impl RunRequest {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::List { after } if *after < 0 => Err("Invalid run cursor".into()),
            Self::Read { id } if *id <= 0 => Err("Invalid run ID".into()),
            Self::Cancel { id, revision } | Self::Delete { id, revision }
                if *id <= 0 || *revision <= 0 =>
            {
                Err("Invalid run revision".into())
            }
            Self::Submit {
                key,
                prompt,
                target,
                model,
            } => {
                if key.is_empty()
                    || key.len() > 128
                    || !key.bytes().all(|c| {
                        c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b':')
                    })
                {
                    return Err("Run keys use 1–128 letters, numbers or -_.:".into());
                }
                if prompt.trim().is_empty() || prompt.len() > 32 * 1024 {
                    return Err("Run prompts need 1–32 KiB of text".into());
                }
                match target {
                    RunTarget::Session { id } if *id <= 0 => {
                        return Err("Invalid run conversation".into());
                    }
                    RunTarget::New { title }
                        if title.trim().is_empty()
                            || title.chars().count() > 120
                            || title.chars().any(char::is_control) =>
                    {
                        return Err("Conversation titles need 1–120 characters".into());
                    }
                    _ => (),
                }
                if model.as_ref().is_some_and(|model| {
                    model.server_id <= 0
                        || model.model.trim().is_empty()
                        || model.model.len() > 256
                        || model.model.chars().any(char::is_control)
                }) {
                    return Err("Choose a valid run model".into());
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Pending,
    Leased,
    Running,
    Blocked,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}
impl RunState {
    pub fn terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRun {
    pub id: i64,
    pub revision: i64,
    pub key: String,
    pub session_id: Option<i64>,
    pub state: RunState,
    pub created_at: i64,
    pub detail: String,
    pub message_id: Option<i64>,
    pub permission_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunResult {
    pub runs: Vec<PluginRun>,
    pub next_after: Option<i64>,
}
