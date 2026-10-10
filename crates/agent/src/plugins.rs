//! One contribution policy for local and remote run preparation.
pub mod execution;
use openwebide_core::{
    ToolDefinition,
    plugins::{PluginToolGroup, ProjectPlugin, enabled_tool_groups},
};
/// Keep workspace primitives built in; optional services are supplied by plugins.
pub struct PluginContext<'a> {
    pub bindings: &'a [ProjectPlugin],
    pub memories: &'a openwebide_core::ProjectMemories,
    pub skills: &'a openwebide_core::ProjectSkills,
    pub context_limit: Option<usize>,
}
pub fn configure(
    tools: &mut Vec<ToolDefinition>,
    prompt: &mut Option<String>,
    context: &PluginContext<'_>,
) {
    let groups = enabled_tool_groups(context.bindings);
    let disabled_memories = openwebide_core::ProjectMemories {
        enabled: false,
        entries: Vec::new(),
    };
    crate::memory::configure(
        tools,
        prompt,
        if groups.contains(&PluginToolGroup::Memory) {
            context.memories
        } else {
            &disabled_memories
        },
        context.context_limit,
    );
    crate::scheduled::configure(tools);
    crate::skills::configure(tools, prompt, context.skills, context.context_limit);
    tools.retain(|tool| match tool.name.as_str() {
        "search_web" | "fetch_web_page" => groups.contains(&PluginToolGroup::Web),
        "memory_create" | "memory_search" | "memory_read" | "memory_update" | "memory_delete" => {
            groups.contains(&PluginToolGroup::Memory)
        }
        "monitor" | "schedule_list" | "schedule_create" | "schedule_update" | "schedule_delete" => {
            groups.contains(&PluginToolGroup::Scheduling)
        }
        "skill_create" | "skill_update" | "skill_delete" => {
            groups.contains(&PluginToolGroup::SkillAuthoring)
        }
        // Authoring instructions now come from the plugin's discoverable skill.
        "skill_creator" => false,
        _ => true,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn optional_services_and_context_follow_enabled_plugins_in_both_host_modes() {
        for mut tools in [crate::vfs_tools(), crate::session::tools_for_host(true)] {
            let memories = openwebide_core::ProjectMemories {
                enabled: true,
                entries: vec![openwebide_core::ProjectMemory {
                    auto_title: false,
                    id: 1,
                    title: "Fact".into(),
                    content: "PRIVATE MEMORY".into(),
                    revision: 1,
                    updated_at: 0,
                }],
            };
            let skills = openwebide_core::ProjectSkills::default();
            let mut prompt = None;
            configure(
                &mut tools,
                &mut prompt,
                &PluginContext {
                    bindings: &[],
                    memories: &memories,
                    skills: &skills,
                    context_limit: None,
                },
            );
            assert!(
                !prompt
                    .as_ref()
                    .is_some_and(|prompt| prompt.contains("PRIVATE MEMORY"))
            );
            assert!(tools.iter().any(|tool| tool.name == "read_file"));
            assert!(tools.iter().any(|tool| tool.name == "run_command"));
            assert!(!tools.iter().any(|tool| matches!(
                tool.name.as_str(),
                "search_web" | "schedule_list" | "memory_read" | "skill_create" | "skill_creator"
            )));
            let mut prepared = openwebide_core::plugins::testing::receipt();
            prepared.manifest.compatibility.plugin_api = 2;
            prepared.manifest.contributions.tool_groups = vec![
                PluginToolGroup::Web,
                PluginToolGroup::Memory,
                PluginToolGroup::Scheduling,
                PluginToolGroup::SkillAuthoring,
            ];
            let mut bindings = vec![ProjectPlugin {
                id: 1,
                revision: 1,
                prepared,
                enabled: true,
            }];
            tools = crate::vfs_tools();
            configure(
                &mut tools,
                &mut prompt,
                &PluginContext {
                    bindings: &bindings,
                    memories: &memories,
                    skills: &skills,
                    context_limit: None,
                },
            );
            for name in [
                "search_web",
                "fetch_web_page",
                "memory_read",
                "schedule_list",
                "monitor",
                "skill_create",
                "skill_read",
            ] {
                assert!(tools.iter().any(|tool| tool.name == name), "missing {name}");
            }
            assert!(!tools.iter().any(|tool| tool.name == "skill_creator"));
            assert!(
                prompt
                    .as_ref()
                    .is_some_and(|prompt| prompt.contains("PRIVATE MEMORY"))
            );
            bindings[0].enabled = false;
            prompt = None;
            configure(
                &mut tools,
                &mut prompt,
                &PluginContext {
                    bindings: &bindings,
                    memories: &memories,
                    skills: &skills,
                    context_limit: None,
                },
            );
            assert!(!tools.iter().any(|tool| matches!(
                tool.name.as_str(),
                "search_web" | "memory_read" | "monitor" | "skill_create"
            )));
        }
    }
}
