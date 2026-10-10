use super::*;
use crate::rusqlite_db::RusqliteDb;
use futures::executor::block_on;
use openwebide_core::plugins::{
    PluginPackage, RecordPlugin, RustPlugin, execution::PluginHostRequest,
};
use serde_json::json;

struct Fixture {
    store: Store<RusqliteDb>,
    user: UserId,
    other: UserId,
    project: i64,
    session: i64,
    plugin: PreparedPlugin,
}
impl Fixture {
    async fn new(mode: WorkspaceMode) -> Self {
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
                    name: "project".into(),
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
            .create_session("origin", None, None, Some(project), user, 0)
            .await
            .unwrap()
            .id;
        let mut plugin = openwebide_core::plugins::testing::receipt();
        plugin.manifest.compatibility.plugin_api = 3;
        plugin.manifest.contributions.skills.clear();
        plugin.manifest.contributions.events = vec!["due".into()];
        plugin.manifest.executable = Some(RustPlugin {
            manifest: "Cargo.toml".into(),
            library: "community_runs".into(),
            sdk_version: "0.1.0".into(),
            capabilities: vec!["runs".into(), "jobs".into()],
        });
        store
            .record_plugin(
                user,
                &RecordPlugin {
                    prepared: plugin.clone(),
                    revision: None,
                    approved_capabilities: vec![],
                    update_policy: None,
                    package: Some(Box::new(PluginPackage {
                        prepared: plugin.clone(),
                        skills: vec![],
                    })),
                },
                0,
            )
            .await
            .unwrap();
        store
            .issue_plugin_context_grant(
                user,
                &PluginExecutionContext {
                    project_id: Some(project),
                    session_id: Some(session),
                    ..Default::default()
                },
                &plugin,
                &"a".repeat(32),
                1,
            )
            .await
            .unwrap();
        Self {
            store,
            user,
            other,
            project,
            session,
            plugin,
        }
    }
    async fn request(&self, command: &RunRequest) -> Result<RunResult, StorageError> {
        let result = self
            .store
            .plugin_host_request(
                self.user,
                self.session,
                &PluginHostRequest {
                    grant: "a".repeat(32),
                    capability: "runs".into(),
                    payload: encode(command).unwrap(),
                },
                2,
            )
            .await?;
        serde_json::from_str(&result).map_err(|error| StorageError::Db(error.to_string()))
    }
    fn submission(key: &str) -> RunRequest {
        RunRequest::Submit {
            key: key.into(),
            prompt: "Plugin-owned prompt".into(),
            target: RunTarget::Origin,
            model: None,
        }
    }
}
#[test]
fn raw_submissions_are_owned_idempotent_cancellable_and_queue_atomic_in_both_modes() {
    block_on(async {
        for mode in [WorkspaceMode::Local, WorkspaceMode::Remote] {
            let f = Fixture::new(mode).await;
            let first = f
                .request(&Fixture::submission("one"))
                .await
                .unwrap()
                .runs
                .remove(0);
            assert_eq!(first.session_id, Some(f.session));
            assert_eq!(first.state, RunState::Pending);
            assert_eq!(
                f.request(&Fixture::submission("one")).await.unwrap().runs[0].id,
                first.id
            );
            let mut changed = Fixture::submission("one");
            if let RunRequest::Submit { prompt, .. } = &mut changed {
                *prompt = "changed".into();
            }
            assert!(f.request(&changed).await.is_err());
            let queue = f
                .store
                .list_queued_prompts(f.user, f.session)
                .await
                .unwrap();
            assert_eq!(queue.len(), 1);
            assert!(
                f.store
                    .update_queued_prompt(f.user, f.session, queue[0].key(), "changed")
                    .await
                    .is_err()
            );
            assert!(
                f.store
                    .consume_queued_prompt(f.user, f.session, queue[0].key(), &queue[0].content, 3)
                    .await
                    .is_err()
            );
            assert!(
                f.request(&RunRequest::Delete {
                    id: first.id,
                    revision: first.revision
                })
                .await
                .is_err()
            );
            assert!(
                f.request(&RunRequest::Cancel {
                    id: first.id,
                    revision: 99
                })
                .await
                .is_err()
            );
            let cancelled = f
                .request(&RunRequest::Cancel {
                    id: first.id,
                    revision: first.revision,
                })
                .await
                .unwrap()
                .runs
                .remove(0);
            assert_eq!(cancelled.state, RunState::Cancelled);
            assert_eq!(cancelled.revision, 2);
            assert!(
                f.store
                    .list_queued_prompts(f.user, f.session)
                    .await
                    .unwrap()
                    .is_empty()
            );
            f.request(&RunRequest::Delete {
                id: first.id,
                revision: cancelled.revision,
            })
            .await
            .unwrap();
            assert!(f.request(&RunRequest::Read { id: first.id }).await.is_err());
            let owned = f
                .store
                .create_session("other", None, None, None, f.other, 0)
                .await
                .unwrap()
                .id;
            let other_project = f
                .store
                .create_project(
                    &NewProject {
                        name: "sibling".into(),
                        mode,
                        path: Some("sibling".into()),
                    },
                    f.user,
                    0,
                )
                .await
                .unwrap()
                .id;
            let sibling = f
                .store
                .create_session("sibling", None, None, Some(other_project), f.user, 0)
                .await
                .unwrap()
                .id;
            for session in [owned, sibling] {
                let request = RunRequest::Submit {
                    key: format!("cross:{session}"),
                    prompt: "x".into(),
                    target: RunTarget::Session { id: session },
                    model: None,
                };
                assert!(f.request(&request).await.is_err());
            }
            assert!(
                f.request(&RunRequest::List { after: 0 })
                    .await
                    .unwrap()
                    .runs
                    .is_empty()
            );
        }
    });
}
#[test]
fn queue_limits_roll_back_new_conversations_and_manual_removal_cancels_submissions() {
    block_on(async {
        for mode in [WorkspaceMode::Local, WorkspaceMode::Remote] {
            let f = Fixture::new(mode).await;
            for index in 0..8 {
                f.store
                    .enqueue_prompt(f.user, f.session, &format!("manual:{index}"), 0)
                    .await
                    .unwrap();
            }
            assert!(f.request(&Fixture::submission("full")).await.is_err());
            assert!(
                f.request(&RunRequest::List { after: 0 })
                    .await
                    .unwrap()
                    .runs
                    .is_empty()
            );
            let new = RunRequest::Submit {
                key: "new".into(),
                prompt: "new conversation".into(),
                target: RunTarget::New {
                    title: "Plugin supplied title".into(),
                },
                model: None,
            };
            let run = f.request(&new).await.unwrap().runs.remove(0);
            let session = run.session_id.unwrap();
            assert_eq!(
                f.store.get_session(session, f.user).await.unwrap().name,
                "Plugin supplied title"
            );
            assert_eq!(
                f.request(&new).await.unwrap().runs[0].session_id,
                Some(session)
            );
            let queued = f
                .store
                .list_queued_prompts(f.user, session)
                .await
                .unwrap()
                .remove(0);
            f.store
                .remove_queued_prompt(f.user, session, queued.key())
                .await
                .unwrap();
            assert_eq!(
                f.request(&RunRequest::Read { id: run.id })
                    .await
                    .unwrap()
                    .runs[0]
                    .state,
                RunState::Cancelled
            );
            let second = f
                .request(&RunRequest::Submit {
                    key: "deleted".into(),
                    prompt: "x".into(),
                    target: RunTarget::New {
                        title: "temporary".into(),
                    },
                    model: None,
                })
                .await
                .unwrap()
                .runs
                .remove(0);
            f.store
                .delete_session(second.session_id.unwrap(), f.user)
                .await
                .unwrap();
            let state = f
                .request(&RunRequest::Read { id: second.id })
                .await
                .unwrap()
                .runs
                .remove(0);
            assert_eq!(state.state, RunState::Cancelled);
            assert!(state.session_id.is_none());
        }
    });
}
#[test]
fn consumed_submissions_are_never_reinjected_and_cancel_only_their_own_active_run() {
    block_on(async {
        for mode in [WorkspaceMode::Local, WorkspaceMode::Remote] {
            let f = Fixture::new(mode).await;
            let first = f
                .request(&Fixture::submission("one"))
                .await
                .unwrap()
                .runs
                .remove(0);
            let queued = f
                .store
                .list_queued_prompts(f.user, f.session)
                .await
                .unwrap()
                .remove(0);
            f.store.db.execute("UPDATE plugin_runs SET state='leased',lease='opaque',lease_expires_at=120 WHERE id=?", &[DbValue::Int(first.id)]).await.unwrap();
            f.store
                .session_run_lease(
                    f.user,
                    f.session,
                    &format!("plugin-run-{}-opaque", first.id),
                    false,
                    0,
                )
                .await
                .unwrap();
            let message = f
                .store
                .consume_queued_prompt(f.user, f.session, queued.key(), &queued.content, 3)
                .await
                .unwrap();
            let running = f
                .request(&RunRequest::Read { id: first.id })
                .await
                .unwrap()
                .runs
                .remove(0);
            assert_eq!(running.state, RunState::Running);
            assert_eq!(running.message_id, Some(message.id));
            assert!(
                f.store
                    .consume_queued_prompt(f.user, f.session, queued.key(), &queued.content, 4)
                    .await
                    .is_err()
            );
            assert_eq!(
                f.request(&Fixture::submission("one")).await.unwrap().runs[0].id,
                first.id
            );
            assert!(
                f.store
                    .list_queued_prompts(f.user, f.session)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let cancelled = f
                .request(&RunRequest::Cancel {
                    id: first.id,
                    revision: running.revision,
                })
                .await
                .unwrap()
                .runs
                .remove(0);
            assert_eq!(cancelled.state, RunState::Cancelling);
            assert!(f.store.cancel_requested_since(f.session, 0).await.unwrap());
        }
    });
}
#[test]
fn sessionless_event_grants_retain_origin_for_raw_submissions() {
    block_on(async {
        for mode in [WorkspaceMode::Local, WorkspaceMode::Remote] {
            let f = Fixture::new(mode).await;
            let response=f.store.plugin_host_request(f.user,f.session,&PluginHostRequest{grant:"a".repeat(32),capability:"jobs".into(),payload:json!({"action":"schedule","key":"job","due_at":20,"event":"due","payload":null}).to_string()},2).await.unwrap();
            assert!(!response.is_empty());
            let delivery = f
                .store
                .claim_plugin_jobs(&f.plugin.host_id, 0, &"b".repeat(32), 20)
                .await
                .unwrap()
                .jobs
                .remove(0);
            let (plugin, context) = f
                .store
                .issue_plugin_job_grant(
                    f.user,
                    &f.plugin.host_id,
                    delivery.job.id,
                    &delivery.lease,
                    &"c".repeat(32),
                    21,
                )
                .await
                .unwrap();
            assert_eq!(plugin, f.plugin);
            assert!(context.session_id.is_none());
            let request = PluginHostRequest {
                grant: "c".repeat(32),
                capability: "runs".into(),
                payload: encode(&Fixture::submission("background")).unwrap(),
            };
            let result = f
                .store
                .plugin_context_host_request(f.user, &request, 21)
                .await
                .unwrap();
            let result: RunResult = serde_json::from_str(&result).unwrap();
            assert_eq!(result.runs[0].session_id, Some(f.session));
            f.store
                .finish_plugin_job(
                    &f.plugin.host_id,
                    delivery.job.id,
                    &delivery.lease,
                    true,
                    "done",
                    22,
                )
                .await
                .unwrap();
            assert!(
                f.store
                    .plugin_context_host_request(f.user, &request, 23)
                    .await
                    .is_err()
            );
        }
    });
}

#[test]
fn run_models_are_pinned_without_mutating_conversation_and_project_deletion_cleans_queue() {
    block_on(async {
        for mode in [WorkspaceMode::Local, WorkspaceMode::Remote] {
            let f = Fixture::new(mode).await;
            let connection = f
                .store
                .insert_connection(&NewConnection {
                    name: "primary".into(),
                    kind: ProviderKind::Ollama,
                    base_url: "http://model.test".into(),
                    model: Some("default".into()),
                    context_limit: None,
                })
                .await
                .unwrap();
            let selected = ModelSelection {
                server_id: connection.id,
                model: "override".into(),
            };
            let command = RunRequest::Submit {
                key: "pinned".into(),
                prompt: "x".into(),
                target: RunTarget::Origin,
                model: Some(selected.clone()),
            };
            let run = f.request(&command).await.unwrap().runs.remove(0);
            let queued = f
                .store
                .list_queued_prompts(f.user, f.session)
                .await
                .unwrap()
                .remove(0);
            assert_eq!(queued.plugin_run, Some(run.id));
            assert!(queued.is_host_delivered());
            assert_eq!(
                f.store
                    .plugin_prompt_model(f.user, f.session, queued.key())
                    .await
                    .unwrap(),
                Some(selected)
            );
            assert!(
                f.store
                    .plugin_prompt_model(f.other, f.session, queued.key())
                    .await
                    .is_err()
            );
            assert!(
                f.store
                    .get_session(f.session, f.user)
                    .await
                    .unwrap()
                    .connection_id
                    .is_none()
            );
            assert_eq!(
                f.store
                    .get_session(f.session, f.user)
                    .await
                    .unwrap()
                    .project_id,
                Some(f.project)
            );
            f.store.delete_project(f.project, f.user).await.unwrap();
            assert!(
                f.store
                    .db
                    .execute("SELECT 1 FROM plugin_runs", &[])
                    .await
                    .unwrap()
                    .rows
                    .is_empty()
            );
            assert!(
                f.store
                    .db
                    .execute("SELECT 1 FROM queued_prompts", &[])
                    .await
                    .unwrap()
                    .rows
                    .is_empty()
            );
        }
    });
}
#[test]
fn run_history_is_paged_bounded_replay_safe_and_released_by_terminal_deletion() {
    block_on(async {
        let f = Fixture::new(WorkspaceMode::Remote).await;
        let first = f
            .request(&Fixture::submission("one"))
            .await
            .unwrap()
            .runs
            .remove(0);
        f.request(&RunRequest::Cancel {
            id: first.id,
            revision: first.revision,
        })
        .await
        .unwrap();
        f.store.db.execute("WITH RECURSIVE sequence(value) AS (SELECT 1 UNION ALL SELECT value+1 FROM sequence WHERE value<999) INSERT INTO plugin_runs(user_id,project_scope,plugin,run_key,request,prepared,context,host_id,session_id,state,created_at) SELECT r.user_id,r.project_scope,r.plugin,'history:'||sequence.value,r.request,r.prepared,r.context,r.host_id,r.session_id,'completed',r.created_at FROM plugin_runs r,sequence WHERE r.id=?", &[DbValue::Int(first.id)]).await.unwrap();
        assert!(f.request(&Fixture::submission("full")).await.is_err());
        assert_eq!(
            f.request(&Fixture::submission("one")).await.unwrap().runs[0].id,
            first.id
        );
        let mut after = 0;
        let mut count = 0;
        loop {
            let page = f.request(&RunRequest::List { after }).await.unwrap();
            assert!(page.runs.len() <= 16);
            count += page.runs.len();
            if let Some(next) = page.next_after {
                assert!(next > after);
                after = next;
            } else {
                break;
            }
        }
        assert_eq!(count, 1000);
        f.request(&RunRequest::Delete {
            id: first.id,
            revision: 2,
        })
        .await
        .unwrap();
        assert!(f.request(&Fixture::submission("new")).await.is_ok());
        f.store
            .db
            .execute("PRAGMA user_version=50", &[])
            .await
            .unwrap();
        f.store.migrate().await.unwrap();
        assert_eq!(
            f.store
                .db
                .execute("SELECT count(*) FROM plugin_runs", &[])
                .await
                .unwrap()
                .rows[0]
                .get_int(0)
                .unwrap(),
            1000
        );
    });
}

#[test]
fn projectless_runs_keep_a_separate_owned_namespace() {
    block_on(async {
        let f = Fixture::new(WorkspaceMode::Remote).await;
        let session = f
            .store
            .create_session("global", None, None, None, f.user, 0)
            .await
            .unwrap()
            .id;
        f.store
            .issue_plugin_context_grant(
                f.user,
                &PluginExecutionContext {
                    session_id: Some(session),
                    ..Default::default()
                },
                &f.plugin,
                &"d".repeat(32),
                1,
            )
            .await
            .unwrap();
        let request = PluginHostRequest {
            grant: "d".repeat(32),
            capability: "runs".into(),
            payload: encode(&Fixture::submission("global")).unwrap(),
        };
        let result = f
            .store
            .plugin_host_request(f.user, session, &request, 2)
            .await
            .unwrap();
        let result: RunResult = serde_json::from_str(&result).unwrap();
        assert_eq!(result.runs[0].session_id, Some(session));
        assert!(
            f.request(&RunRequest::Read {
                id: result.runs[0].id
            })
            .await
            .is_err()
        );
        assert!(
            f.store
                .plugin_host_request(f.other, session, &request, 2)
                .await
                .is_err()
        );
        assert!(
            f.store
                .plugin_context_host_request(f.user, &request, 2)
                .await
                .is_err()
        );
    });
}

#[test]
fn run_claims_are_host_bound_and_recover_only_unconsumed_prompts_in_both_modes() {
    block_on(async {
        for mode in [WorkspaceMode::Local, WorkspaceMode::Remote] {
            let f = Fixture::new(mode).await;
            if mode == WorkspaceMode::Local {
                f.store
                    .set_user_setting(
                        f.user,
                        &format!("scheduled_host_{}", f.project),
                        &encode(&openwebide_core::scheduled::HostBinding {
                            host_id: f.plugin.host_id.clone(),
                            path: "repos/local".into(),
                        })
                        .unwrap(),
                    )
                    .await
                    .unwrap();
            }
            let submitted = f
                .request(&Fixture::submission("one"))
                .await
                .unwrap()
                .runs
                .remove(0);
            let original = f
                .store
                .list_queued_prompts(f.user, f.session)
                .await
                .unwrap()
                .remove(0);
            assert!(
                f.store
                    .claim_plugin_runs("other-host", 0, &"b".repeat(32), 3)
                    .await
                    .unwrap()
                    .runs
                    .is_empty()
            );
            let first = f
                .store
                .claim_plugin_runs(&f.plugin.host_id, 0, &"b".repeat(32), 3)
                .await
                .unwrap()
                .runs
                .remove(0);
            assert_eq!(first.run.id, submitted.id);
            assert_eq!(first.prompt.revision, original.revision + 1);
            assert_eq!(first.host_path.is_some(), mode == WorkspaceMode::Local);
            assert!(
                f.store
                    .claim_plugin_runs(&f.plugin.host_id, 0, &"c".repeat(32), 4)
                    .await
                    .unwrap()
                    .runs
                    .is_empty()
            );
            f.store.renew_plugin_run(&first.lease, 4).await.unwrap();
            let recovered = f
                .store
                .claim_plugin_runs(&f.plugin.host_id, 0, &"d".repeat(32), 124)
                .await
                .unwrap()
                .runs
                .remove(0);
            assert_ne!(first.run_id(), recovered.run_id());
            assert_eq!(recovered.prompt.revision, first.prompt.revision + 1);
            assert!(f.store.renew_plugin_run(&first.lease, 125).await.is_err());
            f.store
                .session_run_lease(f.user, f.session, &recovered.run_id(), false, 125)
                .await
                .unwrap();
            assert!(
                f.store
                    .consume_queued_prompt(
                        f.user,
                        f.session,
                        first.prompt.key(),
                        &first.prompt.content,
                        125
                    )
                    .await
                    .is_err()
            );
            f.store
                .consume_queued_prompt(
                    f.user,
                    f.session,
                    recovered.prompt.key(),
                    &recovered.prompt.content,
                    125,
                )
                .await
                .unwrap();
            let blocked = RunReport {
                state: RunState::Blocked,
                detail: "Awaiting approval".into(),
                permission_id: Some("permission".into()),
            };
            f.store
                .report_plugin_run(&recovered.lease, &blocked, 126)
                .await
                .unwrap();
            assert!(
                f.store
                    .report_plugin_run(&first.lease, &blocked, 126)
                    .await
                    .is_err()
            );
            assert!(
                f.store
                    .release_plugin_run(&recovered.lease, 127)
                    .await
                    .is_err()
            );
            assert!(
                f.store
                    .claim_plugin_runs(&f.plugin.host_id, 0, &"e".repeat(32), 246)
                    .await
                    .unwrap()
                    .runs
                    .is_empty()
            );
            let current = f
                .request(&RunRequest::Read { id: submitted.id })
                .await
                .unwrap()
                .runs
                .remove(0);
            assert_eq!(current.state, RunState::Interrupted);
            assert!(current.permission_id.is_none());
            assert!(
                f.store
                    .list_queued_prompts(f.user, f.session)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    });
}
#[test]
fn run_status_acknowledgements_release_busy_claims_and_reject_wrong_identity() {
    block_on(async {
        let f = Fixture::new(WorkspaceMode::Remote).await;
        let submitted = f
            .request(&Fixture::submission("one"))
            .await
            .unwrap()
            .runs
            .remove(0);
        let first = f
            .store
            .claim_plugin_runs(&f.plugin.host_id, 0, &"b".repeat(32), 3)
            .await
            .unwrap()
            .runs
            .remove(0);
        f.store.release_plugin_run(&first.lease, 4).await.unwrap();
        assert!(f.store.release_plugin_run(&first.lease, 4).await.is_err());
        let delivery = f
            .store
            .claim_plugin_runs(&f.plugin.host_id, 0, &"c".repeat(32), 5)
            .await
            .unwrap()
            .runs
            .remove(0);
        let mut wrong = delivery.lease.clone();
        wrong.host_id = "foreign".into();
        assert!(f.store.renew_plugin_run(&wrong, 6).await.is_err());
        let done = RunReport {
            state: RunState::Completed,
            detail: "Source interpretation belongs in the plugin".into(),
            permission_id: None,
        };
        assert!(
            f.store
                .report_plugin_run(&delivery.lease, &done, 6)
                .await
                .is_err()
        );
        f.store
            .session_run_lease(f.user, f.session, &delivery.run_id(), false, 6)
            .await
            .unwrap();
        f.store
            .consume_queued_prompt(
                f.user,
                f.session,
                delivery.prompt.key(),
                &delivery.prompt.content,
                6,
            )
            .await
            .unwrap();
        f.store
            .report_plugin_run(&delivery.lease, &done, 7)
            .await
            .unwrap();
        f.store
            .report_plugin_run(&delivery.lease, &done, 8)
            .await
            .unwrap();
        let changed = RunReport {
            detail: "changed".into(),
            ..done
        };
        assert!(
            f.store
                .report_plugin_run(&delivery.lease, &changed, 8)
                .await
                .is_err()
        );
        assert_eq!(
            f.request(&RunRequest::Read { id: submitted.id })
                .await
                .unwrap()
                .runs[0]
                .state,
            RunState::Completed
        );
    });
}
