//! One execution protocol for server hosts and paired local hosts.
use super::PreparedPlugin;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginOperation {
    #[default]
    Tool,
    Context,
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
