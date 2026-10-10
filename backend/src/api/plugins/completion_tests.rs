use super::*;
#[test]
fn completion_authority_is_checked_before_model_access_in_both_modes() {
    futures::executor::block_on(async {
        for mode in [
            openwebide_core::WorkspaceMode::Local,
            openwebide_core::WorkspaceMode::Remote,
        ] {
            let store =
                openwebide_storage::Store::new(crate::state::AppDb::open_in_memory().unwrap());
            store.migrate().await.unwrap();
            let user = store
                .insert_user("owner", "hash", openwebide_core::UserRole::Admin, 0)
                .await
                .unwrap()
                .id;
            let other = store
                .insert_user("other", "hash", openwebide_core::UserRole::User, 0)
                .await
                .unwrap()
                .id;
            let project = store
                .create_project(
                    &openwebide_core::NewProject {
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
            let mut plugin = openwebide_core::plugins::testing::receipt();
            plugin.manifest.compatibility.plugin_api = 3;
            plugin.manifest.contributions.skills.clear();
            plugin.manifest.contributions.tools = vec![openwebide_core::plugins::PluginTool {
                name: "summarize".into(),
                description: "Summarize supplied text".into(),
                parameters: json!({"type":"object"}),
                requires_approval: false,
            }];
            plugin.manifest.executable = Some(openwebide_core::plugins::RustPlugin {
                manifest: "Cargo.toml".into(),
                library: "summary".into(),
                sdk_version: "0.1.0".into(),
                capabilities: vec!["completion".into()],
            });
            store
                .record_plugin(
                    user,
                    &RecordPlugin {
                        prepared: plugin.clone(),
                        revision: None,
                        update_policy: None,
                        approved_capabilities: Vec::new(),
                        package: Some(Box::new(openwebide_core::plugins::PluginPackage {
                            prepared: plugin.clone(),
                            skills: Vec::new(),
                        })),
                    },
                    now(),
                )
                .await
                .unwrap();
            let token = "a".repeat(32);
            store
                .issue_plugin_grant(user, session, &plugin, &token, now())
                .await
                .unwrap();
            let request = openwebide_core::plugins::execution::PluginHostRequest {
                grant: token,
                capability: "completion".into(),
                payload:
                    json!({"system_prompt":"Plugin policy","prompt":"Fact","max_output_tokens":64})
                        .to_string(),
            };
            let authorized = execute_host_request(&store, user, session, &request)
                .await
                .unwrap_err();
            assert_eq!(
                authorized.public_message(),
                "Session has no model connection"
            );
            assert!(
                execute_host_request(&store, other, session, &request)
                    .await
                    .unwrap_err()
                    .into_response()
                    .status()
                    .as_u16()
                    == 404
            );
            let mut invalid = request.clone();
            invalid.grant = "b".repeat(32);
            assert_eq!(
                execute_host_request(&store, user, session, &invalid)
                    .await
                    .unwrap_err()
                    .into_response()
                    .status()
                    .as_u16(),
                404
            );
            invalid = request.clone();
            invalid.capability = "collections".into();
            assert_eq!(
                execute_host_request(&store, user, session, &invalid)
                    .await
                    .unwrap_err()
                    .into_response()
                    .status()
                    .as_u16(),
                400
            );
            invalid = request.clone();
            invalid.payload =
                json!({"system_prompt":"x","prompt":"Fact","max_output_tokens":0}).to_string();
            assert!(
                execute_host_request(&store, user, session, &invalid)
                    .await
                    .unwrap_err()
                    .public_message()
                    .contains("1–1024")
            );
        }
    });
}
