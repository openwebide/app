//! One execution protocol for server hosts and paired local hosts.
use super::PreparedPlugin;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvokePlugin {
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
