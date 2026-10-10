//! Durable authority snapshots for general host callbacks.
use super::*;
use openwebide_core::plugins::{
    PreparedPlugin, default_bindings, execution::PluginHostRequest, records::RecordRequest,
};

impl<D: Db> Store<D> {
    /// The API generates the opaque token. Only an enabled, installed receipt can
    /// acquire authority; subsequent updates do not replace this run's snapshot.
    pub async fn issue_plugin_grant(
        &self,
        user: UserId,
        session: i64,
        plugin: &PreparedPlugin,
        token: &str,
        now: i64,
    ) -> Result<(), StorageError> {
        plugin
            .validate()
            .map_err(|error| StorageError::InvalidRequest(error.to_string()))?;
        if token.len() != 32 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) || now < 0 {
            return Err(StorageError::InvalidRequest(
                "Invalid plugin execution grant".into(),
            ));
        }
        self.db.transaction(|tx| async move {
            let store = Store::new(tx);
            let project = store.get_session(session, user).await?.project_id;
            let bindings = match project {
                Some(project) => store.project_plugins(user, project).await?,
                None => default_bindings(&store.plugin_installations(user).await?),
            };
            if plugin.manifest.executable.is_none() || !bindings.iter().any(|binding| {
                binding.enabled && binding.prepared.source == plugin.source
                    && binding.prepared.manifest == plugin.manifest && binding.prepared.digest == plugin.digest
            }) {
                return Err(StorageError::Conflict("Plugin is no longer enabled at the selected version".into()));
            }
            let expires = now.checked_add(86_400).ok_or_else(|| StorageError::InvalidRequest("Invalid grant expiry".into()))?;
            store.db.execute("DELETE FROM plugin_execution_grants WHERE expires_at<=?", &[DbValue::Int(now)]).await?;
            store.db.execute("INSERT INTO plugin_execution_grants(token,user_id,session_id,project_scope,prepared,expires_at) VALUES(?,?,?,?,?,?)", &[
                DbValue::Text(token.into()), DbValue::Int(user.get()), DbValue::Int(session), DbValue::Int(project.unwrap_or(0)),
                DbValue::Text(serde_json::to_string(plugin).map_err(|error| StorageError::Db(error.to_string()))?), DbValue::Int(expires),
            ]).await?;
            Ok(())
        }).await
    }
    /// Authority and storage mutations share a transaction, preventing a session
    /// reassignment between checking its original project and writing records.
    pub fn plugin_host_request<'a>(
        &'a self,
        user: UserId,
        session: i64,
        request: &'a PluginHostRequest,
        now: i64,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, StorageError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.db.transaction(|tx| async move {
            let store = Store::new(tx);
            let current = store.get_session(session, user).await?;
            let grant = store.db.execute("SELECT prepared,project_scope FROM plugin_execution_grants WHERE token=? AND user_id=? AND session_id=? AND expires_at>?", &[
                DbValue::Text(request.grant.clone()), DbValue::Int(user.get()), DbValue::Int(session), DbValue::Int(now),
            ]).await?;
            let row = grant.rows.first().ok_or_else(|| StorageError::NotFound("Plugin execution grant".into()))?;
            if row.get_int(1)? != current.project_id.unwrap_or(0) {
                return Err(StorageError::Conflict("Plugin execution project changed".into()));
            }
            let plugin: PreparedPlugin = serde_json::from_str(row.get_text(0)?).map_err(|error| StorageError::Db(error.to_string()))?;
            if !plugin.manifest.executable.as_ref().is_some_and(|rust| rust.capabilities.contains(&request.capability)) {
                return Err(StorageError::InvalidRequest("Plugin capability is not granted".into()));
            }
            let namespace = plugin.storage_namespace();
            match request.capability.as_str() {
                "records" => {
                    let command: RecordRequest = serde_json::from_str(&request.payload).map_err(|error| StorageError::InvalidRequest(error.to_string()))?;
                    let result = store.plugin_records_in_transaction(user, session, &namespace, &command, now).await?;
                    serde_json::to_string(&result).map_err(|error| StorageError::Db(error.to_string()))
                }
                "collections" => {
                    let command: RecordRequest = serde_json::from_str(&request.payload).map_err(|error| StorageError::InvalidRequest(error.to_string()))?;
                    let result = store.plugin_collections_in_transaction(user, session, &command, now).await?;
                    serde_json::to_string(&result).map_err(|error| StorageError::Db(error.to_string()))
                }
                _ => Err(StorageError::InvalidRequest("Plugin host capability unavailable".into())),
            }
        }).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rusqlite_db::RusqliteDb;
    use futures::executor::block_on;
    use openwebide_core::plugins::{
        PluginPackage, PluginTool, RecordPlugin, RemovePlugin, RustPlugin,
    };
    use serde_json::json;

    fn receipt() -> PreparedPlugin {
        let mut receipt = openwebide_core::plugins::testing::receipt();
        receipt.manifest.publisher = "example".into();
        receipt.manifest.name = "notes".into();
        receipt.manifest.compatibility.plugin_api = 3;
        receipt.manifest.contributions.skills.clear();
        receipt.manifest.contributions.tools = vec![PluginTool {
            name: "notes_add".into(),
            description: "Add a note".into(),
            parameters: json!({"type":"object"}),
            requires_approval: true,
        }];
        receipt.manifest.executable = Some(RustPlugin {
            manifest: "Cargo.toml".into(),
            library: "notes".into(),
            sdk_version: "0.1.0".into(),
            capabilities: vec!["records".into()],
        });
        receipt
    }
    fn installation(prepared: &PreparedPlugin, revision: Option<i64>) -> RecordPlugin {
        RecordPlugin {
            approved_capabilities: Vec::new(),
            prepared: prepared.clone(),
            revision,
            update_policy: None,
            package: Some(Box::new(PluginPackage {
                prepared: prepared.clone(),
                skills: Vec::new(),
            })),
        }
    }
    #[test]
    fn skill_collection_callbacks_preserve_ui_records_and_plugin_provenance_in_both_modes() {
        block_on(async {
            for mode in [WorkspaceMode::Local, WorkspaceMode::Remote] {
                let store = Store::new(RusqliteDb::open_in_memory().unwrap());
                store.migrate().await.unwrap();
                let user = store
                    .insert_user("owner", "hash", UserRole::Admin, 0)
                    .await
                    .unwrap()
                    .id;
                let other = store
                    .insert_user("other", "hash", UserRole::User, 0)
                    .await
                    .unwrap()
                    .id;
                let project = store
                    .create_project(
                        &NewProject {
                            name: "p".into(),
                            mode,
                            path: Some("p".into()),
                        },
                        user,
                        0,
                    )
                    .await
                    .unwrap()
                    .id;
                let session = store
                    .create_session("s", None, None, Some(project), user, 0)
                    .await
                    .unwrap()
                    .id;
                let mut plugin = receipt();
                plugin.manifest.executable.as_mut().unwrap().capabilities =
                    vec!["collections".into()];
                store
                    .record_plugin(user, &installation(&plugin, None), 1)
                    .await
                    .unwrap();
                let token = "a".repeat(32);
                store
                    .issue_plugin_grant(user, session, &plugin, &token, 2)
                    .await
                    .unwrap();
                let call = |operation| PluginHostRequest {
                    grant: token.clone(),
                    capability: "collections".into(),
                    payload: json!({"collection":"skills","operation":operation}).to_string(),
                };
                let draft = |name: &str| openwebide_core::SkillDraft {
                    name: name.into(),
                    description: "Build guidance".into(),
                    instructions: "Run approved checks".into(),
                    enabled: true,
                    resources: vec![openwebide_core::SkillResource {
                        name: "references/build.md".into(),
                        content: "cargo test".into(),
                        binary: false,
                    }],
                    metadata: Default::default(),
                };
                let ui = store
                    .skill_command(
                        user,
                        project,
                        &openwebide_core::SkillCommand::Create {
                            draft: draft("ui-skill"),
                        },
                        false,
                        1,
                    )
                    .await
                    .unwrap()
                    .entries
                    .remove(0);
                let read = call(json!({"action":"read","id":ui.id}));
                let result: serde_json::Value = serde_json::from_str(
                    &store
                        .plugin_host_request(user, session, &read, 3)
                        .await
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(
                    result["records"][0]["value"]["draft"]["resources"][0]["content"],
                    "cargo test"
                );
                assert!(result["records"][0]["value"]["origin"].is_null());
                assert!(
                    store
                        .plugin_host_request(other, session, &read, 3)
                        .await
                        .is_err()
                );
                let mut changed = draft("ui-skill");
                changed.instructions = "Plugin changed these instructions".into();
                let edit = call(
                    json!({"action":"update","id":ui.id,"revision":ui.revision,"value":{"draft":changed}}),
                );
                store
                    .plugin_host_request(user, session, &edit, 4)
                    .await
                    .unwrap();
                assert_eq!(
                    store.project_skills(user, project).await.unwrap().entries[0]
                        .draft
                        .instructions,
                    changed.instructions
                );
                assert!(
                    store
                        .plugin_host_request(user, session, &edit, 5)
                        .await
                        .is_err()
                );
                assert!(
                    store
                        .plugin_host_request(
                            user,
                            session,
                            &call(json!({"action":"create","value":{"draft":draft("ui-skill")}})),
                            5
                        )
                        .await
                        .is_err()
                );
                assert!(store.plugin_host_request(user, session, &call(json!({"action":"create","value":{"draft":draft("invalid-skill"),"origin":{"publisher":"forged"}}})), 5).await.is_err());
                for index in 0..9 {
                    store.plugin_host_request(user, session, &call(json!({"action":"create","value":{"draft":draft(&format!("created-{index}"))}})), 5).await.unwrap();
                }
                let first: serde_json::Value = serde_json::from_str(
                    &store
                        .plugin_host_request(user, session, &call(json!({"action":"list"})), 6)
                        .await
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(first["records"].as_array().unwrap().len(), 8);
                let second: serde_json::Value = serde_json::from_str(
                    &store
                        .plugin_host_request(
                            user,
                            session,
                            &call(json!({"action":"list","after":first["next"]})),
                            6,
                        )
                        .await
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(second["records"].as_array().unwrap().len(), 2);
                // A managed skill may be read with its provenance, but its owner
                // controls updates and deletion through plugin lifecycle operations.
                let binding = store.db.execute("SELECT id FROM project_plugins WHERE user_id=? AND project_id=? AND repository=? AND path=?", &[
                    DbValue::Int(user.get()),DbValue::Int(project),DbValue::Text(plugin.source.repository.clone()),DbValue::Text(plugin.source.path.clone()),
                ]).await.unwrap().rows[0].get_int(0).unwrap();
                store
                    .db
                    .execute(
                        "INSERT INTO project_plugin_skills(plugin_id,skill_id) VALUES(?,?)",
                        &[DbValue::Int(binding), DbValue::Int(ui.id)],
                    )
                    .await
                    .unwrap();
                let managed: serde_json::Value = serde_json::from_str(
                    &store
                        .plugin_host_request(user, session, &read, 7)
                        .await
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(
                    managed["records"][0]["value"]["origin"]["publisher"],
                    "example"
                );
                assert!(
                    store
                        .plugin_host_request(
                            user,
                            session,
                            &call(json!({"action":"delete","id":ui.id,"revision":2})),
                            7
                        )
                        .await
                        .is_err()
                );
                let personal = store
                    .project_skills(user, project)
                    .await
                    .unwrap()
                    .entries
                    .into_iter()
                    .find(|skill| skill.draft.name == "created-0")
                    .unwrap();
                let mut disabled_draft = personal.draft.clone();
                disabled_draft.enabled = false;
                store
                    .skill_command(
                        user,
                        project,
                        &openwebide_core::SkillCommand::Update {
                            id: personal.id,
                            revision: personal.revision,
                            draft: disabled_draft,
                        },
                        false,
                        7,
                    )
                    .await
                    .unwrap();
                assert!(
                    store
                        .plugin_host_request(
                            user,
                            session,
                            &call(json!({"action":"read","id":personal.id})),
                            7
                        )
                        .await
                        .is_err()
                );
                assert!(store.plugin_host_request(user, session, &call(json!({"action":"delete","id":personal.id,"revision":personal.revision+1})), 7).await.is_err());
                store
                    .set_user_setting(user, &format!("project_skills_{project}"), "false")
                    .await
                    .unwrap();
                let disabled: serde_json::Value = serde_json::from_str(
                    &store
                        .plugin_host_request(user, session, &read, 8)
                        .await
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(disabled["enabled"], false);
                assert!(disabled["records"].as_array().unwrap().is_empty());
                assert!(store.plugin_host_request(user, session, &call(json!({"action":"create","value":{"draft":draft("disabled-create")}})), 8).await.is_err());
            }
        });
    }
    fn request(grant: &str) -> PluginHostRequest {
        PluginHostRequest {grant:grant.into(), capability:"records".into(), payload:json!({"collection":"notes","operation":{"action":"create","value":{"text":"owned"}}}).to_string()}
    }
    #[test]
    fn shared_collection_grants_preserve_ui_data_revisions_and_opt_out_in_both_modes() {
        block_on(async {
            for mode in [WorkspaceMode::Local, WorkspaceMode::Remote] {
                let store = Store::new(RusqliteDb::open_in_memory().unwrap());
                store.migrate().await.unwrap();
                let user = store
                    .insert_user("owner", "hash", UserRole::Admin, 0)
                    .await
                    .unwrap()
                    .id;
                let project = store
                    .create_project(
                        &NewProject {
                            name: "p".into(),
                            mode,
                            path: Some("p".into()),
                        },
                        user,
                        0,
                    )
                    .await
                    .unwrap()
                    .id;
                let session = store
                    .create_session("s", None, None, Some(project), user, 0)
                    .await
                    .unwrap()
                    .id;
                let initial = store
                    .memory_command(
                        user,
                        project,
                        &openwebide_core::MemoryCommand::Create {
                            auto_title: false,
                            title: "Existing UI memory".into(),
                            content: "Kept intact".into(),
                        },
                        false,
                        1,
                    )
                    .await
                    .unwrap()
                    .entries
                    .remove(0);
                let plugin = receipt();
                store
                    .record_plugin(user, &installation(&plugin, None), 1)
                    .await
                    .unwrap();
                let private_token = "a".repeat(32);
                store
                    .issue_plugin_grant(user, session, &plugin, &private_token, 2)
                    .await
                    .unwrap();
                let call = |token: &str, operation| PluginHostRequest {
                    grant: token.into(),
                    capability: "collections".into(),
                    payload: json!({"collection":"memories","operation":operation}).to_string(),
                };
                let list = json!({"action":"list"});
                // Granting private persistence does not authorize app-visible data.
                assert!(
                    store
                        .plugin_host_request(user, session, &call(&private_token, list.clone()), 3)
                        .await
                        .is_err()
                );
                let mut shared = plugin.clone();
                shared.source.commit = "c".repeat(40);
                shared.digest = "c".repeat(64);
                shared.manifest.version = "0.2.0".into();
                shared
                    .manifest
                    .executable
                    .as_mut()
                    .unwrap()
                    .capabilities
                    .push("collections".into());
                let mut update = installation(&shared, Some(1));
                update.approved_capabilities = vec!["collections".into()];
                store.record_plugin(user, &update, 3).await.unwrap();
                let token = "b".repeat(32);
                store
                    .issue_plugin_grant(user, session, &shared, &token, 4)
                    .await
                    .unwrap();
                let projectless = store
                    .create_session("projectless", None, None, None, user, 4)
                    .await
                    .unwrap()
                    .id;
                let global_token = "c".repeat(32);
                store
                    .issue_plugin_grant(user, projectless, &shared, &global_token, 4)
                    .await
                    .unwrap();
                let global: serde_json::Value = serde_json::from_str(
                    &store
                        .plugin_host_request(
                            user,
                            projectless,
                            &call(&global_token, list.clone()),
                            5,
                        )
                        .await
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(global["enabled"], false);
                assert_eq!(global["records"], json!([]));
                assert!(
                    store
                        .plugin_host_request(
                            user,
                            projectless,
                            &call(
                                &global_token,
                                json!({"action":"create","value":{"title":"x","content":"y"}})
                            ),
                            5
                        )
                        .await
                        .is_err()
                );
                let read = store
                    .plugin_host_request(user, session, &call(&token, list.clone()), 5)
                    .await
                    .unwrap();
                let page: serde_json::Value = serde_json::from_str(&read).unwrap();
                assert_eq!(page["records"][0]["id"], initial.id);
                assert_eq!(page["records"][0]["value"]["content"], "Kept intact");
                let edit = json!({"action":"update","id":initial.id,"revision":initial.revision,"value":{"title":"Plugin-edited","content":"Visible in UI","auto_title":false}});
                store
                    .plugin_host_request(user, session, &call(&token, edit.clone()), 6)
                    .await
                    .unwrap();
                assert!(
                    store
                        .plugin_host_request(user, session, &call(&token, edit), 7)
                        .await
                        .is_err()
                );
                let ui = store.project_memories(user, project).await.unwrap();
                assert_eq!(ui.entries[0].title, "Plugin-edited");
                assert_eq!(ui.entries[0].revision, initial.revision + 1);
                let invalid =
                    json!({"action":"create","value":{"title":"ok","content":"bad","user_id":42}});
                assert!(
                    store
                        .plugin_host_request(user, session, &call(&token, invalid), 7)
                        .await
                        .is_err()
                );
                for index in 0..33 {
                    let create = json!({"action":"create","value":{"title":format!("Row {index}"),"content":"data"}});
                    store
                        .plugin_host_request(user, session, &call(&token, create), 8)
                        .await
                        .unwrap();
                }
                let first: serde_json::Value = serde_json::from_str(
                    &store
                        .plugin_host_request(user, session, &call(&token, list.clone()), 9)
                        .await
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(first["records"].as_array().unwrap().len(), 32);
                let second: serde_json::Value = serde_json::from_str(
                    &store
                        .plugin_host_request(
                            user,
                            session,
                            &call(&token, json!({"action":"list","after":first["next"]})),
                            9,
                        )
                        .await
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(second["records"].as_array().unwrap().len(), 2);
                store
                    .memory_command(
                        user,
                        project,
                        &openwebide_core::MemoryCommand::SetEnabled { enabled: false },
                        false,
                        10,
                    )
                    .await
                    .unwrap();
                let disabled: serde_json::Value = serde_json::from_str(
                    &store
                        .plugin_host_request(user, session, &call(&token, list), 11)
                        .await
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(disabled["enabled"], false);
                assert_eq!(disabled["records"], json!([]));
                assert!(store.plugin_host_request(user, session, &call(&token, json!({"action":"delete","id":initial.id,"revision":initial.revision+1})), 11).await.is_err());
            }
        });
    }
    #[test]
    fn execution_grants_pin_authority_through_updates_and_removal_in_both_modes() {
        block_on(async {
            for mode in [WorkspaceMode::Local, WorkspaceMode::Remote] {
                let store = Store::new(RusqliteDb::open_in_memory().unwrap());
                store.migrate().await.unwrap();
                let owner = store
                    .insert_user("owner", "hash", UserRole::Admin, 0)
                    .await
                    .unwrap()
                    .id;
                let other = store
                    .insert_user("other", "hash", UserRole::User, 0)
                    .await
                    .unwrap()
                    .id;
                let project = store
                    .create_project(
                        &NewProject {
                            name: "p".into(),
                            mode,
                            path: Some("p".into()),
                        },
                        owner,
                        0,
                    )
                    .await
                    .unwrap()
                    .id;
                let session = store
                    .create_session("s", None, None, Some(project), owner, 0)
                    .await
                    .unwrap()
                    .id;
                let second = store
                    .create_session("second", None, None, Some(project), owner, 0)
                    .await
                    .unwrap()
                    .id;
                let original = receipt();
                let token = "a".repeat(32);
                assert!(
                    store
                        .issue_plugin_grant(owner, session, &original, &token, 1)
                        .await
                        .is_err()
                );
                let installed = store
                    .record_plugin(owner, &installation(&original, None), 1)
                    .await
                    .unwrap();
                store
                    .issue_plugin_grant(owner, session, &original, &token, 2)
                    .await
                    .unwrap();
                assert!(
                    store
                        .issue_plugin_grant(other, session, &original, &"d".repeat(32), 2)
                        .await
                        .is_err()
                );
                let call = request(&token);
                assert!(
                    store
                        .plugin_host_request(other, session, &call, 3)
                        .await
                        .is_err()
                );
                assert!(
                    store
                        .plugin_host_request(owner, second, &call, 3)
                        .await
                        .is_err()
                );
                let first: serde_json::Value = serde_json::from_str(
                    &store
                        .plugin_host_request(owner, session, &call, 3)
                        .await
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(first["records"][0]["value"]["text"], "owned");
                let mut updated = original.clone();
                updated.source.commit = "c".repeat(40);
                updated.digest = "c".repeat(64);
                updated.manifest.version = "0.2.0".into();
                updated.manifest.executable.as_mut().unwrap().capabilities = vec!["http".into()];
                let mut update = installation(&updated, Some(installed[0].revision));
                update.approved_capabilities = vec!["http".into()];
                let installed = store.record_plugin(owner, &update, 4).await.unwrap();
                let next_token = "b".repeat(32);
                store
                    .issue_plugin_grant(owner, session, &updated, &next_token, 5)
                    .await
                    .unwrap();
                assert!(
                    store
                        .issue_plugin_grant(owner, session, &original, &"e".repeat(32), 5)
                        .await
                        .is_err()
                );
                assert!(
                    store
                        .plugin_host_request(owner, session, &request(&next_token), 6)
                        .await
                        .is_err()
                );
                assert!(
                    store
                        .plugin_host_request(owner, session, &call, 6)
                        .await
                        .is_ok()
                );
                store
                    .remove_plugin(
                        owner,
                        &RemovePlugin {
                            source: updated.source,
                            revision: installed[0].revision,
                        },
                    )
                    .await
                    .unwrap();
                assert!(
                    store
                        .plugin_host_request(owner, session, &call, 8)
                        .await
                        .is_ok()
                );
                assert!(
                    store
                        .plugin_host_request(owner, session, &call, 86_402)
                        .await
                        .is_err()
                );
                store
                    .db
                    .execute(
                        "UPDATE sessions SET project_id=NULL WHERE id=?",
                        &[DbValue::Int(session)],
                    )
                    .await
                    .unwrap();
                assert!(
                    store
                        .plugin_host_request(owner, session, &call, 9)
                        .await
                        .is_err()
                );
                let rows = store
                    .db
                    .execute("SELECT COUNT(*) FROM plugin_records", &[])
                    .await
                    .unwrap();
                assert_eq!(rows.rows[0].get_int(0).unwrap(), 3);
            }
        });
    }
}
