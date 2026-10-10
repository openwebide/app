//! One execution protocol for server hosts and paired local hosts.
use super::PreparedPlugin;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginOperation {
    #[default]
    Tool,
    Context,
    Event,
}

/// Host-owned execution scope. This is never passed to the plugin or model.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginExecutionContext {
    pub project_id: Option<i64>,
    pub session_id: Option<i64>,
    pub primary: Option<crate::ModelSelection>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginGrantRequest {
    pub context: PluginExecutionContext,
    pub plugins: Vec<PreparedPlugin>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventInput {
    pub name: String,
    pub payload: serde_json::Value,
}
pub fn validate_event(name: &str, payload: &serde_json::Value) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
    {
        return Err("Event names use 1–64 lowercase letters, digits or underscores".into());
    }
    if serde_json::to_vec(payload)
        .map_err(|error| error.to_string())?
        .len()
        > 256 * 1024
    {
        return Err("Plugin event data exceeds 256 KiB".into());
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextContribution {
    pub prompt: Option<String>,
    pub disabled_tools: Vec<String>,
}

/// Read-only context authority is checked by the runtime and the orchestration
/// facade before forwarding a transport's host request.
pub fn context_request_allowed(capability: &str, payload: &str) -> bool {
    match capability {
        "clock" => true,
        "records" | "collections" => serde_json::from_str::<super::records::RecordRequest>(payload)
            .is_ok_and(|request| {
                request.validate().is_ok()
                    && matches!(
                        request.operation,
                        super::records::RecordOperation::List { .. }
                            | super::records::RecordOperation::Read { .. }
                    )
            }),
        _ => false,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvokePlugin {
    #[serde(default)]
    pub operation: PluginOperation,
    pub prepared: PreparedPlugin,
    pub name: String,
    pub arguments: String,
}
impl InvokePlugin {
    pub fn validate_event(&self) -> Result<(), String> {
        if matches!(self.operation, PluginOperation::Event) {
            if !self.name.is_empty() {
                return Err("Event callbacks cannot supply a tool name".into());
            }
            let input: EventInput =
                serde_json::from_str(&self.arguments).map_err(|error| error.to_string())?;
            validate_event(&input.name, &input.payload)?;
            if !self
                .prepared
                .manifest
                .contributions
                .events
                .contains(&input.name)
            {
                return Err("Event is not declared by this plugin".into());
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuePlugin {
    pub id: String,
    pub sequence: u32,
    pub response: Result<String, String>,
}
/// The authenticated caller supplies the opaque run grant, never the plugin.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginHostRequest {
    pub grant: String,
    pub capability: String,
    pub payload: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginInvocation {
    pub id: String,
    pub step: PluginStep,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginStep {
    Ready,
    HostCall {
        sequence: u32,
        capability: String,
        payload: String,
    },
    Complete {
        ok: bool,
        content: String,
        summary: String,
    },
    Failed {
        error: String,
    },
}
