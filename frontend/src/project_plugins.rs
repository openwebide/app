//! Shared catalog, installation and activation workflows with thin host transports.
use crate::{
    backend::Api,
    local_agent::BrowserBridgeClient,
    project_host::{ProjectExecution, ProjectHost},
    state::{
        auth::AuthState, chat::ChatState, plugins::PluginsState, projects::ProjectsState,
        settings::SettingsState,
    },
};
use leptos::{prelude::*, task::spawn_local};
use openwebide_core::plugins::{marketplace::*, *};

enum PluginTransport {
    Remote(Api, Option<i64>),
    Local(BrowserBridgeClient),
}
impl PluginTransport {
    async fn prepare(&self, source: &PluginSource) -> Result<PreparedPlugin, String> {
        match self {
            Self::Remote(api, id) => {
                api.with_value(Clone::clone)
                    .prepare_plugin(*id, source)
                    .await
            }
            Self::Local(client) => client.prepare_plugin(source).await,
        }
    }
    async fn package(&self, expected: &PreparedPlugin) -> Result<PluginPackage, String> {
        match self {
            Self::Remote(api, id) => {
                api.with_value(Clone::clone)
                    .plugin_package(*id, expected)
                    .await
            }
            Self::Local(client) => client.plugin_package(expected).await,
        }
    }
}
#[derive(Clone, Debug)]
pub struct CatalogSelection {
    pub marketplace: MarketplaceSource,
    pub publisher: String,
    pub name: String,
    pub version: String,
}
#[derive(Clone)]
enum Operation {
    Refresh,
    Catalogs,
    UpdateAll,
    ApproveUpdate(PluginSource),
    Policy(Box<PluginInstallation>, PluginUpdatePolicy),
    Sources(Vec<MarketplaceSource>),
    Install(Option<CatalogSelection>),
    Enable(Box<PluginInstallation>),
    Disable(Box<ProjectPlugin>),
    Remove(Box<PluginInstallation>),
}
#[derive(Clone, Copy)]
pub struct ProjectPluginActions {
    pub refresh: Callback<()>,
    pub refresh_catalogs: Callback<()>,
    pub update_all: Callback<()>,
    pub approve_update: Callback<PluginSource>,
    pub dismiss_update: Callback<PluginSource>,
    pub set_update_policy: Callback<(PluginInstallation, PluginUpdatePolicy)>,
    pub save_sources: Callback<Vec<MarketplaceSource>>,
    pub install: Callback<()>,
    pub install_release: Callback<CatalogSelection>,
    pub enable: Callback<PluginInstallation>,
    pub disable: Callback<ProjectPlugin>,
    pub remove: Callback<PluginInstallation>,
}
impl ProjectPluginActions {
    pub fn new(
        api: Api,
        state: PluginsState,
        host: ProjectHost,
        auth: AuthState,
        projects: ProjectsState,
        chat: ChatState,
        settings: SettingsState,
    ) -> Self {
        let generation = RwSignal::new(0_u64);
        let skill_actions = use_context::<crate::project_skills::ProjectSkillActions>();
        let scope = move || {
            (
                auth.generation.get_untracked(),
                projects.active_project.get_untracked(),
                chat.active_session.get_untracked(),
                settings.bridge_url.get_untracked(),
                host.revision(),
                projects
                    .active_project
                    .get_untracked()
                    .and_then(|id| projects.project(id))
                    .map(|p| (p.mode, p.path)),
                projects.local_handles.with_untracked(|h| {
                    projects
                        .active_project
                        .get_untracked()
                        .and_then(|id| h.get(&id).cloned())
                }),
            )
        };
        let perform = Callback::new(move |operation: Operation| {
            // Check synchronously as well as in the invalidation effect: a
            // pending review cannot cross a scope change before effects run.
            if state
                .review_current
                .get_untracked()
                .is_some_and(|check| !check())
            {
                state.pending_updates.set(Vec::new());
                state.review_current.set(None);
            }
            if state.busy.get_untracked() {
                return;
            }
            generation.update(|g| *g += 1);
            let ticket = generation.get_untracked();
            let identity = scope();
            if state.pending_updates.with_untracked(Vec::is_empty) {
                let review_identity = identity.clone();
                state.review_current.set(Some(std::sync::Arc::new(move || {
                    scope() == review_identity
                })));
            }
            let current = move || {
                generation.try_get_untracked() == Some(ticket)
                    && auth.generation.try_get_untracked().is_some()
                    && scope() == identity
            };
            let project = projects.active_project.get_untracked();
            state.busy.set(true);
            state.error.set(None);
            spawn_local(async move {
                let refresh_skills = matches!(
                    operation,
                    Operation::Install(_)
                        | Operation::UpdateAll
                        | Operation::ApproveUpdate(_)
                        | Operation::Catalogs
                        | Operation::Refresh
                        | Operation::Enable(_)
                        | Operation::Disable(_)
                        | Operation::Remove(_)
                );
                let result =
                    run_operation(operation, api, state, host, project, current.clone()).await;
                if !current() {
                    return;
                }
                state.busy.set(false);
                if let Err(error) = result {
                    state.error.set(Some(error));
                } else if refresh_skills && let Some(actions) = skill_actions {
                    actions.refresh.run(());
                }
            });
        });
        let refresh = Callback::new(move |()| perform.run(Operation::Refresh));
        Effect::new(move |_| {
            auth.generation.track();
            projects.active_project.track();
            chat.active_session.track();
            settings.bridge_url.track();
            projects.projects.track();
            projects.local_handles.track();
            generation.update(|g| *g += 1);
            state.installations.set(Vec::new());
            state.pending_updates.set(Vec::new());
            state.review_current.set(None);
            state.project_plugins.set(Vec::new());
            state.marketplaces.set(MarketplaceSettings::default());
            state.failures.set(Vec::new());
            state.loaded.set(false);
            state.busy.set(false);
            state.error.set(None);
            state.repository.set(String::new());
            state.commit.set(String::new());
            state.path.set(".".into());
            state.marketplace_repository.set(String::new());
            state.marketplace_reference.set(String::new());
            state.marketplace_path.set("marketplace.json".into());
            state.search.set(String::new());
            if auth.user.get_untracked().is_some() {
                refresh.run(());
            }
        });
        let checked = StoredValue::new(None::<u64>);
        Effect::new(move |_| {
            let account = auth.generation.get();
            if state.loaded.get()
                && !state.busy.get()
                && auth.user.get().is_some()
                && checked.get_value() != Some(account)
            {
                checked.set_value(Some(account));
                perform.run(Operation::Catalogs);
            }
        });
        let timer = leptos::leptos_dom::helpers::set_interval_with_handle(
            move || {
                if auth.user.get_untracked().is_some() && !chat.streaming.get_untracked() {
                    perform.run(Operation::Catalogs);
                }
            },
            std::time::Duration::from_secs(15 * 60),
        )
        .ok();
        on_cleanup(move || {
            if let Some(timer) = timer {
                timer.clear();
            }
        });
        Self {
            refresh,
            refresh_catalogs: Callback::new(move |()| perform.run(Operation::Catalogs)),
            update_all: Callback::new(move |()| perform.run(Operation::UpdateAll)),
            approve_update: Callback::new(move |source| {
                perform.run(Operation::ApproveUpdate(source));
            }),
            dismiss_update: Callback::new(move |source| {
                state
                    .pending_updates
                    .update(|updates| updates.retain(|update| update.prepared.source != source));
            }),
            set_update_policy: Callback::new(move |(entry, policy)| {
                perform.run(Operation::Policy(Box::new(entry), policy));
            }),
            save_sources: Callback::new(move |sources| perform.run(Operation::Sources(sources))),
            install: Callback::new(move |()| perform.run(Operation::Install(None))),
            install_release: Callback::new(move |release| {
                perform.run(Operation::Install(Some(release)));
            }),
            enable: Callback::new(move |entry| perform.run(Operation::Enable(Box::new(entry)))),
            disable: Callback::new(move |entry| perform.run(Operation::Disable(Box::new(entry)))),
            remove: Callback::new(move |entry| perform.run(Operation::Remove(Box::new(entry)))),
        }
    }
}

async fn run_operation(
    operation: Operation,
    api: Api,
    state: PluginsState,
    host: ProjectHost,
    project: Option<i64>,
    current: impl Fn() -> bool + Clone + 'static,
) -> Result<(), String> {
    let backend = api.with_value(Clone::clone);

    match operation {
        Operation::ApproveUpdate(source) => {
            let request = state
                .pending_updates
                .with_untracked(|updates| {
                    updates
                        .iter()
                        .find(|update| update.prepared.source == source)
                        .cloned()
                })
                .ok_or("This plugin update is no longer pending.")?;
            let entries = backend.record_plugin(&request).await?;
            if !current() {
                return Ok(());
            }
            state.installations.set(entries);
            state
                .pending_updates
                .update(|updates| updates.retain(|update| update.prepared.source != source));
            if let Some(id) = project {
                let bindings = backend.project_plugins(id).await?;
                if current() {
                    state.project_plugins.set(bindings);
                }
            }
        }
        Operation::Refresh => {
            let entries = backend.plugin_installations().await?;
            if !current() {
                return Ok(());
            }
            let mut entries = entries;

            state.installations.set(entries.clone());
            let markets = backend.plugin_marketplaces().await?;
            if !current() {
                return Ok(());
            }
            state.marketplaces.set(markets);
            if let Some(id) = project {
                let entries = backend.project_plugins(id).await?;
                if !current() {
                    return Ok(());
                }
                state.project_plugins.set(entries);
            }
            state.loaded.set(true);
            for entry in entries
                .clone()
                .into_iter()
                .filter(|entry| !entry.default_enabled)
            {
                let transport = match host.resolve_guarded(project, true, current.clone()).await? {
                    ProjectExecution::Remote { api, .. } => PluginTransport::Remote(api, project),
                    ProjectExecution::Local(client) => PluginTransport::Local(client),
                };
                if !current() {
                    return Ok(());
                }
                let package = transport.package(&entry.prepared).await?;
                validate_loaded_plugin(&entry.prepared, &package)?;
                if !current() {
                    return Ok(());
                }
                entries = backend
                    .record_plugin(&RecordPlugin {
                        approved_capabilities: Vec::new(),
                        update_policy: None,
                        prepared: package.prepared.clone(),
                        revision: Some(entry.revision),
                        package: Some(Box::new(package)),
                    })
                    .await?;
                if !current() {
                    return Ok(());
                }
            }
            if !current() {
                return Ok(());
            }
            state.installations.set(entries);
            if let Some(id) = project {
                let bindings = backend.project_plugins(id).await?;
                if current() {
                    state.project_plugins.set(bindings);
                }
            }
        }
        Operation::Catalogs => {
            let refreshed = backend.refresh_plugin_marketplaces().await?;
            if !current() {
                return Ok(());
            }
            state.marketplaces.set(refreshed.settings);
            state.failures.set(refreshed.failures);
            update_plugins(api, state, host, project, current, true).await?;
        }
        Operation::Sources(sources) => {
            let request = SaveMarketplaces {
                revision: state.marketplaces.get_untracked().revision,
                sources,
            };
            let saved = backend.save_plugin_marketplaces(&request).await?;
            if !current() {
                return Ok(());
            }
            state.marketplaces.set(saved);
            state.failures.set(Vec::new());
        }
        Operation::Disable(entry) => {
            let id = project.ok_or("Open a project first.")?;
            let entries = backend
                .project_plugin_command(
                    id,
                    &ProjectPluginCommand::Disable {
                        id: entry.id,
                        revision: entry.revision,
                    },
                )
                .await?;
            if !current() {
                return Ok(());
            }
            state.project_plugins.set(entries);
        }
        Operation::Remove(entry) => {
            let entries = backend
                .remove_plugin(&RemovePlugin {
                    source: entry.prepared.source.clone(),
                    revision: entry.revision,
                })
                .await?;
            if !current() {
                return Ok(());
            }
            state.installations.set(entries);
            if let Some(id) = project {
                let entries = backend.project_plugins(id).await?;
                if current() {
                    state.project_plugins.set(entries);
                }
            }
        }
        Operation::Install(selection) => {
            install_plugin(api, state, host, project, current, selection).await?;
        }
        Operation::UpdateAll => {
            update_plugins(api, state, host, project, current, false).await?;
        }
        Operation::Policy(entry, policy) => {
            let entries = backend
                .record_plugin(&RecordPlugin {
                    approved_capabilities: Vec::new(),
                    prepared: entry.prepared.clone(),
                    revision: Some(entry.revision),
                    package: None,
                    update_policy: Some(policy),
                })
                .await?;
            if current() {
                state.installations.set(entries);
            }
        }
        Operation::Enable(entry) => {
            let id = project.ok_or("Open a project before enabling a plugin.")?;
            let revision = state.project_plugins.with_untracked(|entries| {
                entries
                    .iter()
                    .find(|e| {
                        e.prepared.source.repository == entry.prepared.source.repository
                            && e.prepared.source.path == entry.prepared.source.path
                    })
                    .map(|e| e.revision)
            });
            let transport = match host
                .resolve_guarded(Some(id), true, current.clone())
                .await?
            {
                ProjectExecution::Remote { api, .. } => PluginTransport::Remote(api, Some(id)),
                ProjectExecution::Local(client) => PluginTransport::Local(client),
            };
            if !current() {
                return Ok(());
            }
            let package = transport.package(&entry.prepared).await?;
            package.validate().map_err(|e| e.to_string())?;
            if package.prepared.source != entry.prepared.source
                || package.prepared.manifest != entry.prepared.manifest
                || package.prepared.digest != entry.prepared.digest
            {
                return Err("Plugin host returned different plugin content.".into());
            }
            if !current() {
                return Ok(());
            }
            let entries = backend
                .record_plugin(&RecordPlugin {
                    approved_capabilities: Vec::new(),
                    update_policy: None,
                    package: None,
                    prepared: package.prepared.clone(),
                    revision: Some(entry.revision),
                })
                .await?;
            if !current() {
                return Ok(());
            }
            let installed = entries
                .iter()
                .find(|e| e.prepared.source == package.prepared.source)
                .ok_or("Installation was not recorded.")?;
            let command = ProjectPluginCommand::Enable {
                package: Box::new(package),
                installation_revision: installed.revision,
                revision,
            };
            state.installations.set(entries);
            let bindings = backend.project_plugin_command(id, &command).await?;
            if current() {
                state.project_plugins.set(bindings);
            }
        }
    }
    Ok(())
}

fn validate_loaded_plugin(
    expected: &PreparedPlugin,
    package: &PluginPackage,
) -> Result<(), String> {
    package.validate().map_err(|error| error.to_string())?;
    if package.prepared.source != expected.source
        || package.prepared.manifest != expected.manifest
        || package.prepared.digest != expected.digest
    {
        return Err("Plugin host returned different plugin content.".into());
    }
    Ok(())
}

async fn install_plugin(
    api: Api,
    state: PluginsState,
    host: ProjectHost,
    project: Option<i64>,
    current: impl Fn() -> bool + Clone + 'static,
    selection: Option<CatalogSelection>,
) -> Result<(), String> {
    let backend = api.with_value(Clone::clone);

    let (source, expected) = if let Some(selection) = selection {
        let markets = state.marketplaces.get_untracked();
        let catalog = markets
            .catalogs
            .iter()
            .find(|c| c.source == selection.marketplace)
            .ok_or("Refresh this marketplace first.")?;
        let source = catalog
            .catalog
            .resolve(
                &catalog.source,
                &selection.publisher,
                &selection.name,
                &selection.version,
            )
            .map_err(|e| e.to_string())?;
        (source, Some(selection))
    } else {
        (
            PluginSource {
                repository: state.repository.get_untracked().trim().into(),
                commit: state.commit.get_untracked().trim().into(),
                path: state.path.get_untracked().trim().into(),
            },
            None,
        )
    };
    source.validate().map_err(|e| e.to_string())?;
    let previous = state.installations.with_untracked(|entries| {
        entries
            .iter()
            .find(|e| {
                e.prepared.source.repository == source.repository
                    && e.prepared.source.path == source.path
            })
            .cloned()
    });
    let revision = previous.as_ref().map(|entry| entry.revision);
    let transport = match host.resolve_guarded(project, true, current.clone()).await? {
        ProjectExecution::Remote { api, .. } => PluginTransport::Remote(api, project),
        ProjectExecution::Local(client) => PluginTransport::Local(client),
    };
    if !current() {
        return Ok(());
    }
    let prepared = transport.prepare(&source).await?;
    prepared.validate().map_err(|e| e.to_string())?;
    if prepared.source != source {
        return Err("Plugin host returned a different source.".into());
    }
    if expected.is_some_and(|e| {
        e.publisher != prepared.manifest.publisher
            || e.name != prepared.manifest.name
            || e.version != prepared.manifest.version
    }) {
        return Err("Plugin identity or version does not match its marketplace release.".into());
    }
    if !current() {
        return Ok(());
    }
    let package = transport.package(&prepared).await?;
    validate_loaded_plugin(&prepared, &package)?;
    if !current() {
        return Ok(());
    }
    let capabilities = previous.as_ref().map_or_else(Vec::new, |entry| {
        added_capabilities(&entry.prepared, &prepared)
    });
    let request = RecordPlugin {
        approved_capabilities: capabilities.clone(),
        update_policy: None,
        prepared: package.prepared.clone(),
        revision,
        package: Some(Box::new(package)),
    };
    if !capabilities.is_empty() {
        state.pending_updates.update(|updates| {
            updates.retain(|update| {
                update.prepared.source.repository != source.repository
                    || update.prepared.source.path != source.path
            });
            updates.push(request);
        });
        return Ok(());
    }
    let entries = backend.record_plugin(&request).await?;
    if !current() {
        return Ok(());
    }
    state.installations.set(entries);
    if let Some(id) = project {
        let bindings = backend.project_plugins(id).await?;
        if current() {
            state.project_plugins.set(bindings);
        }
    }
    Ok(())
}
async fn update_plugins(
    api: Api,
    state: PluginsState,
    host: ProjectHost,
    project: Option<i64>,
    current: impl Fn() -> bool + Clone + 'static,
    automatic_only: bool,
) -> Result<(), String> {
    let installed = state.installations.get_untracked();
    let markets = state.marketplaces.get_untracked();
    let updates = if automatic_only {
        automatic_updates(&installed, &markets)
    } else {
        available_updates(&installed, &markets)
    };
    let mut failures = Vec::new();
    for update in updates
        .into_iter()
        .filter(|update| !automatic_only || update.automatic)
    {
        if !current() {
            return Ok(());
        }
        let name = update.installation.prepared.manifest.display_name.clone();
        if let Err(error) = install_plugin(
            api,
            state,
            host,
            project,
            current.clone(),
            Some(CatalogSelection {
                marketplace: update.marketplace,
                publisher: update.installation.prepared.manifest.publisher,
                name: update.installation.prepared.manifest.name,
                version: update.version,
            }),
        )
        .await
        {
            failures.push(format!("{name}: {error}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}
