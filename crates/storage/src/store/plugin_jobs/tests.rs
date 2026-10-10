use super::*;
use crate::rusqlite_db::RusqliteDb;
use futures::executor::block_on;
use openwebide_core::{
    NewProject, WorkspaceMode,
    plugins::{
        PluginPackage, ProjectPluginCommand, RecordPlugin, RustPlugin, execution::PluginHostRequest,
    },
};
use serde_json::json;

struct Fixture {
    store: Store<RusqliteDb>,
    user: UserId,
    other: UserId,
    project: i64,
    prepared: PreparedPlugin,
    context: PluginExecutionContext,
}
impl Fixture {
    async fn new(mode: WorkspaceMode) -> Self {
        Self::with_database(mode, RusqliteDb::open_in_memory().unwrap()).await
    }
    async fn with_database(mode: WorkspaceMode, database: RusqliteDb) -> Self {
        let store = Store::new(database);
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
        let mut prepared = openwebide_core::plugins::testing::receipt();
        prepared.host_id = match mode {
            WorkspaceMode::Local => "paired-host",
            WorkspaceMode::Remote => "server-host",
        }
        .into();
        prepared.manifest.publisher = "community".into();
        prepared.manifest.name = "events".into();
        prepared.manifest.compatibility.plugin_api = 3;
        prepared.manifest.contributions.skills.clear();
        prepared.manifest.contributions.events = vec!["due".into()];
        prepared.manifest.executable = Some(RustPlugin {
            manifest: "Cargo.toml".into(),
            library: "events".into(),
            sdk_version: "0.1.0".into(),
            capabilities: vec!["jobs".into(), "records".into()],
        });
        let context = PluginExecutionContext {
            project_id: Some(project),
            session_id: None,
            primary: None,
            user_action: true,
        };
        let fixture = Self {
            store,
            user,
            other,
            project,
            prepared,
            context,
        };
        fixture.install(None).await;
        fixture
            .store
            .issue_plugin_context_grant(
                user,
                &fixture.context,
                &fixture.prepared,
                &"a".repeat(32),
                1,
            )
            .await
            .unwrap();
        fixture
    }
    async fn install(&self, revision: Option<i64>) {
        self.store
            .record_plugin(
                self.user,
                &RecordPlugin {
                    prepared: self.prepared.clone(),
                    revision,
                    approved_capabilities: vec![],
                    update_policy: None,
                    package: Some(Box::new(PluginPackage {
                        prepared: self.prepared.clone(),
                        skills: vec![],
                    })),
                },
                1,
            )
            .await
            .unwrap();
    }
    async fn request(&self, request: &JobRequest, now: i64) -> Result<JobResult, StorageError> {
        let response = self
            .store
            .plugin_context_host_request(
                self.user,
                &PluginHostRequest {
                    grant: "a".repeat(32),
                    capability: "jobs".into(),
                    payload: serde_json::to_string(request).unwrap(),
                },
                now,
            )
            .await?;
        decode(&response)
    }
    fn scheduled(key: &str) -> JobRequest {
        JobRequest::Schedule {
            key: key.into(),
            due_at: 20,
            expires_at: Some(1000),
            event: "due".into(),
            payload: json!({"sequence":1}),
        }
    }
}

#[test]
fn jobs_are_owned_idempotent_version_pinned_and_leased_in_both_modes() {
    block_on(async {
        for mode in [WorkspaceMode::Local, WorkspaceMode::Remote] {
            let mut fixture = Fixture::new(mode).await;
            let initial = fixture
                .request(&Fixture::scheduled("one"), 2)
                .await
                .unwrap()
                .jobs
                .remove(0);
            let replay = fixture
                .request(&Fixture::scheduled("one"), 21)
                .await
                .unwrap()
                .jobs
                .remove(0);
            assert_eq!(initial, replay);
            assert_eq!(initial.state, JobState::Pending);
            let mut different = Fixture::scheduled("one");
            if let JobRequest::Schedule { payload, .. } = &mut different {
                *payload = json!({"sequence":2});
            }
            assert!(fixture.request(&different, 2).await.is_err());
            let old = fixture.prepared.clone();
            fixture.prepared.source.commit = "b".repeat(40);
            fixture.prepared.digest = "b".repeat(64);
            fixture.prepared.manifest.version = "0.2.0".into();
            fixture.install(Some(1)).await;
            assert!(
                fixture
                    .store
                    .claim_plugin_jobs("wrong-host", 0, &"b".repeat(32), 20)
                    .await
                    .unwrap()
                    .jobs
                    .is_empty()
            );
            assert!(
                fixture
                    .store
                    .claim_plugin_jobs(&old.host_id, 0, &"b".repeat(32), 19)
                    .await
                    .unwrap()
                    .jobs
                    .is_empty()
            );
            let delivery = fixture
                .store
                .claim_plugin_jobs(&old.host_id, 0, &"b".repeat(32), 20)
                .await
                .unwrap()
                .jobs
                .remove(0);
            assert_eq!(delivery.prepared, old);
            assert_eq!(delivery.job.attempts, 1);
            assert!(
                fixture
                    .store
                    .claim_plugin_jobs(&old.host_id, 0, &"c".repeat(32), 21)
                    .await
                    .unwrap()
                    .jobs
                    .is_empty()
            );
            assert!(
                fixture
                    .store
                    .issue_plugin_job_grant(
                        fixture.other,
                        &old.host_id,
                        initial.id,
                        &delivery.lease,
                        &"d".repeat(32),
                        21
                    )
                    .await
                    .is_err()
            );
            let (pinned, context) = fixture
                .store
                .issue_plugin_job_grant(
                    fixture.user,
                    &old.host_id,
                    initial.id,
                    &delivery.lease,
                    &"d".repeat(32),
                    21,
                )
                .await
                .unwrap();
            assert_eq!(pinned, old);
            assert!(!context.user_action);
            assert!(context.session_id.is_none());
            let callback = PluginHostRequest {grant:"d".repeat(32),capability:"records".into(),payload:json!({"collection":"events","operation":{"action":"create","value":{"delivered":true}}}).to_string()};
            fixture
                .store
                .plugin_context_host_request(fixture.user, &callback, 22)
                .await
                .unwrap();
            assert!(
                fixture
                    .store
                    .renew_plugin_job("wrong-host", initial.id, &delivery.lease, 30)
                    .await
                    .is_err()
            );
            assert_eq!(
                fixture
                    .store
                    .renew_plugin_job(&old.host_id, initial.id, &delivery.lease, 30)
                    .await
                    .unwrap(),
                150
            );
            assert!(
                fixture
                    .store
                    .claim_plugin_jobs(&old.host_id, 0, &"c".repeat(32), 149)
                    .await
                    .unwrap()
                    .jobs
                    .is_empty()
            );
            let recovered = fixture
                .store
                .claim_plugin_jobs(&old.host_id, 0, &"c".repeat(32), 150)
                .await
                .unwrap()
                .jobs
                .remove(0);
            assert_eq!(recovered.job.attempts, 2);
            assert!(
                fixture
                    .store
                    .plugin_context_host_request(fixture.user, &callback, 151)
                    .await
                    .is_err()
            );
            assert!(
                fixture
                    .store
                    .finish_plugin_job(
                        &old.host_id,
                        initial.id,
                        &delivery.lease,
                        true,
                        "old result",
                        151
                    )
                    .await
                    .is_err()
            );
            fixture
                .store
                .finish_plugin_job(
                    &old.host_id,
                    initial.id,
                    &recovered.lease,
                    true,
                    "delivered",
                    151,
                )
                .await
                .unwrap();
            fixture
                .store
                .finish_plugin_job(
                    &old.host_id,
                    initial.id,
                    &recovered.lease,
                    true,
                    "delivered",
                    152,
                )
                .await
                .unwrap();
            assert!(
                fixture
                    .store
                    .finish_plugin_job(
                        &old.host_id,
                        initial.id,
                        &recovered.lease,
                        false,
                        "different result",
                        152
                    )
                    .await
                    .is_err()
            );
            assert_eq!(
                fixture
                    .request(&JobRequest::Read { id: initial.id }, 152)
                    .await
                    .unwrap()
                    .jobs[0]
                    .state,
                JobState::Completed
            );
        }
    });
}

#[test]
fn jobs_stop_future_delivery_and_revoke_cancelled_callback_authority_in_both_modes() {
    block_on(async {
        for mode in [WorkspaceMode::Local, WorkspaceMode::Remote] {
            let fixture = Fixture::new(mode).await;
            let initial = fixture
                .request(&Fixture::scheduled("one"), 2)
                .await
                .unwrap()
                .jobs
                .remove(0);
            let expired = fixture
                .request(
                    &JobRequest::Schedule {
                        key: "expired".into(),
                        due_at: 20,
                        expires_at: Some(21),
                        event: "due".into(),
                        payload: json!({}),
                    },
                    2,
                )
                .await
                .unwrap()
                .jobs
                .remove(0);
            let binding = fixture
                .store
                .project_plugins(fixture.user, fixture.project)
                .await
                .unwrap()
                .remove(0);
            fixture
                .store
                .project_plugin_command(
                    fixture.user,
                    fixture.project,
                    &ProjectPluginCommand::Disable {
                        id: binding.id,
                        revision: binding.revision,
                    },
                    3,
                )
                .await
                .unwrap();
            assert!(
                fixture
                    .store
                    .claim_plugin_jobs(&fixture.prepared.host_id, 0, &"b".repeat(32), 22)
                    .await
                    .unwrap()
                    .jobs
                    .is_empty()
            );
            assert_eq!(
                fixture
                    .request(&JobRequest::Read { id: expired.id }, 22)
                    .await
                    .unwrap()
                    .jobs[0]
                    .state,
                JobState::Expired
            );
            assert!(
                fixture
                    .request(&Fixture::scheduled("disabled"), 3)
                    .await
                    .is_err()
            );
            // Existing authority can cancel its own pending delivery, but cannot forge a scope.
            let cancelled = fixture
                .request(
                    &JobRequest::Cancel {
                        id: initial.id,
                        revision: initial.revision,
                    },
                    4,
                )
                .await
                .unwrap();
            assert_eq!(cancelled.jobs[0].state, JobState::Cancelled);
            assert!(
                fixture
                    .request(
                        &JobRequest::Cancel {
                            id: initial.id,
                            revision: initial.revision
                        },
                        4
                    )
                    .await
                    .is_err()
            );
            let request = PluginHostRequest {
                grant: "a".repeat(32),
                capability: "jobs".into(),
                payload: serde_json::to_string(&JobRequest::Read { id: initial.id }).unwrap(),
            };
            assert!(
                fixture
                    .store
                    .plugin_context_host_request(fixture.other, &request, 4)
                    .await
                    .is_err()
            );
        }
    });
}

#[test]
fn leased_jobs_and_job_grants_survive_migration_replay_and_cancel_revokes_callbacks() {
    block_on(async {
        let fixture = Fixture::new(WorkspaceMode::Local).await;
        let initial = fixture
            .request(&Fixture::scheduled("one"), 2)
            .await
            .unwrap()
            .jobs
            .remove(0);
        let delivery = fixture
            .store
            .claim_plugin_jobs(&fixture.prepared.host_id, 0, &"b".repeat(32), 20)
            .await
            .unwrap()
            .jobs
            .remove(0);
        fixture
            .store
            .issue_plugin_job_grant(
                fixture.user,
                &fixture.prepared.host_id,
                initial.id,
                &delivery.lease,
                &"d".repeat(32),
                21,
            )
            .await
            .unwrap();
        crate::migrations::apply_through(&fixture.store.db, 50)
            .await
            .unwrap();
        let callback = PluginHostRequest {
            grant: "d".repeat(32),
            capability: "records".into(),
            payload: json!({"collection":"events","operation":{"action":"list"}}).to_string(),
        };
        fixture
            .store
            .plugin_context_host_request(fixture.user, &callback, 22)
            .await
            .unwrap();
        fixture
            .request(
                &JobRequest::Cancel {
                    id: initial.id,
                    revision: delivery.job.revision,
                },
                22,
            )
            .await
            .unwrap();
        assert!(
            fixture
                .store
                .plugin_context_host_request(fixture.user, &callback, 23)
                .await
                .is_err()
        );
        assert!(
            fixture
                .store
                .renew_plugin_job(&fixture.prepared.host_id, initial.id, &delivery.lease, 23)
                .await
                .is_err()
        );
        assert!(
            fixture
                .store
                .finish_plugin_job(
                    &fixture.prepared.host_id,
                    initial.id,
                    &delivery.lease,
                    true,
                    "late",
                    23
                )
                .await
                .is_err()
        );
    });
}

#[test]
fn job_pages_are_bounded_and_terminal_cleanup_releases_quota() {
    block_on(async {
        let fixture = Fixture::new(WorkspaceMode::Local).await;
        for index in 0..20 {
            fixture
                .request(
                    &JobRequest::Schedule {
                        key: format!("large-{index}"),
                        due_at: 20,
                        expires_at: None,
                        event: "due".into(),
                        payload: json!({"data":"x".repeat(64000)}),
                    },
                    2,
                )
                .await
                .unwrap();
        }
        let page = fixture
            .request(&JobRequest::List { after: 0 }, 2)
            .await
            .unwrap();
        assert!(encode(&page).unwrap().len() <= MAX_JOB_PAGE_BYTES);
        assert!(page.jobs.len() < 20);
        let next = fixture
            .request(
                &JobRequest::List {
                    after: page.next_after.unwrap(),
                },
                2,
            )
            .await
            .unwrap();
        assert_eq!(page.jobs.len() + next.jobs.len(), 20);
        let pending = page.jobs[0].clone();
        assert!(
            fixture
                .request(
                    &JobRequest::Delete {
                        id: pending.id,
                        revision: pending.revision
                    },
                    2
                )
                .await
                .is_err()
        );
        let mut ids = std::collections::BTreeSet::new();
        let mut after = 0;
        for lease in ["b", "c", "d"] {
            let page = fixture
                .store
                .claim_plugin_jobs(&fixture.prepared.host_id, after, &lease.repeat(32), 20)
                .await
                .unwrap();
            assert!(page.jobs.len() <= MAX_JOB_DELIVERIES);
            assert!(encode(&page).unwrap().len() <= MAX_JOB_DELIVERY_BYTES);
            for delivery in page.jobs {
                assert!(ids.insert(delivery.job.id));
            }
            after = page.next_after.unwrap_or(0);
        }
        assert_eq!(ids.len(), 20);
        // Populate the rest of the quota directly; scheduling must still enforce the bound.
        for index in 20..MAX_JOBS {
            fixture.store.db.execute("INSERT INTO plugin_jobs(user_id,project_scope,plugin,job_key,due_at,event,payload,prepared,context,host_id) VALUES(?,?,?,?,20,'due','{}',?,?,?)", &[DbValue::Int(fixture.user.get()),DbValue::Int(fixture.project),DbValue::Text(fixture.prepared.storage_namespace()),DbValue::Text(format!("quota-{index}")),DbValue::Text(encode(&fixture.prepared).unwrap()),DbValue::Text(encode(&fixture.context).unwrap()),DbValue::Text(fixture.prepared.host_id.clone())]).await.unwrap();
        }
        assert!(
            fixture
                .request(&Fixture::scheduled("full"), 2)
                .await
                .is_err()
        );
        fixture
            .store
            .finish_plugin_job(
                &fixture.prepared.host_id,
                pending.id,
                &"b".repeat(32),
                true,
                "done",
                21,
            )
            .await
            .unwrap();
        let completed = fixture
            .request(&JobRequest::Read { id: pending.id }, 21)
            .await
            .unwrap()
            .jobs
            .remove(0);
        assert!(
            fixture
                .request(
                    &JobRequest::Delete {
                        id: pending.id,
                        revision: pending.revision
                    },
                    21
                )
                .await
                .is_err()
        );
        assert!(
            fixture
                .request(
                    &JobRequest::Delete {
                        id: pending.id,
                        revision: completed.revision
                    },
                    21
                )
                .await
                .unwrap()
                .jobs
                .is_empty()
        );
        assert!(
            fixture
                .request(&JobRequest::Read { id: pending.id }, 21)
                .await
                .is_err()
        );
        fixture
            .request(
                &JobRequest::Schedule {
                    key: "new".into(),
                    due_at: 30,
                    expires_at: None,
                    event: "due".into(),
                    payload: json!({}),
                },
                22,
            )
            .await
            .unwrap();
    });
}
#[test]
fn persisted_jobs_recover_after_database_reopen_and_invalid_requests_do_not_enqueue() {
    block_on(async {
        for mode in [WorkspaceMode::Local, WorkspaceMode::Remote] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("jobs.db");
            let fixture = Fixture::with_database(mode, RusqliteDb::open(&path).unwrap()).await;
            for payload in [
                json!({"action":"schedule","key":"forged","due_at":20,"expires_at":null,"event":"due","payload":null,"user_id":fixture.other.get()}),
                json!({"action":"schedule","key":"unknown","due_at":20,"expires_at":null,"event":"undeclared","payload":null}),
                json!({"action":"schedule","key":"past","due_at":1,"expires_at":null,"event":"due","payload":null}),
                json!({"action":"schedule","key":"empty-window","due_at":20,"expires_at":20,"event":"due","payload":null}),
                json!({"action":"grant","user_id":fixture.user.get()}),
                json!({"action":"schedule","key":"large","due_at":20,"expires_at":null,"event":"due","payload":"x".repeat(65536)}),
            ] {
                let request = PluginHostRequest {
                    grant: "a".repeat(32),
                    capability: "jobs".into(),
                    payload: payload.to_string(),
                };
                assert!(
                    fixture
                        .store
                        .plugin_context_host_request(fixture.user, &request, 2)
                        .await
                        .is_err()
                );
            }
            assert!(
                fixture
                    .request(&JobRequest::List { after: 0 }, 2)
                    .await
                    .unwrap()
                    .jobs
                    .is_empty()
            );
            let created = fixture
                .request(&Fixture::scheduled("durable"), 2)
                .await
                .unwrap()
                .jobs
                .remove(0);
            let first_lease = "b".repeat(32);
            let second_lease = "c".repeat(32);
            let (first, second) = futures::join!(
                fixture
                    .store
                    .claim_plugin_jobs(&fixture.prepared.host_id, 0, &first_lease, 20),
                fixture
                    .store
                    .claim_plugin_jobs(&fixture.prepared.host_id, 0, &second_lease, 20)
            );
            assert_eq!(first.unwrap().jobs.len() + second.unwrap().jobs.len(), 1);
            let host = fixture.prepared.host_id.clone();
            let pinned = fixture.prepared.clone();
            drop(fixture);
            let store = Store::new(RusqliteDb::open(&path).unwrap());
            store.migrate().await.unwrap();
            assert!(
                store
                    .claim_plugin_jobs(&host, 0, &"d".repeat(32), 139)
                    .await
                    .unwrap()
                    .jobs
                    .is_empty()
            );
            let recovered = store
                .claim_plugin_jobs(&host, 0, &"d".repeat(32), 140)
                .await
                .unwrap()
                .jobs
                .remove(0);
            assert_eq!(recovered.job.id, created.id);
            assert_eq!(recovered.job.attempts, 2);
            assert_eq!(recovered.prepared, pinned);
        }
    });
}

#[test]
fn job_reads_cannot_cross_projects_and_project_deletion_removes_jobs_and_authority() {
    block_on(async {
        let fixture = Fixture::new(WorkspaceMode::Local).await;
        let created = fixture
            .request(&Fixture::scheduled("one"), 2)
            .await
            .unwrap()
            .jobs
            .remove(0);
        let other_project = fixture
            .store
            .create_project(
                &NewProject {
                    name: "another".into(),
                    mode: WorkspaceMode::Local,
                    path: None,
                },
                fixture.user,
                3,
            )
            .await
            .unwrap()
            .id;
        fixture
            .store
            .issue_plugin_context_grant(
                fixture.user,
                &PluginExecutionContext {
                    project_id: Some(other_project),
                    ..Default::default()
                },
                &fixture.prepared,
                &"e".repeat(32),
                3,
            )
            .await
            .unwrap();
        let other_read = PluginHostRequest {
            grant: "e".repeat(32),
            capability: "jobs".into(),
            payload: encode(&JobRequest::Read { id: created.id }).unwrap(),
        };
        assert!(
            fixture
                .store
                .plugin_context_host_request(fixture.user, &other_read, 4)
                .await
                .is_err()
        );
        let delivery = fixture
            .store
            .claim_plugin_jobs(&fixture.prepared.host_id, 0, &"b".repeat(32), 20)
            .await
            .unwrap()
            .jobs
            .remove(0);
        fixture
            .store
            .issue_plugin_job_grant(
                fixture.user,
                &fixture.prepared.host_id,
                created.id,
                &delivery.lease,
                &"d".repeat(32),
                21,
            )
            .await
            .unwrap();
        fixture
            .store
            .delete_project(fixture.project, fixture.user)
            .await
            .unwrap();
        let rows = fixture
            .store
            .db
            .execute("SELECT COUNT(*) FROM plugin_jobs", &[])
            .await
            .unwrap();
        assert_eq!(rows.rows[0].get_int(0).unwrap(), 0);
        let rows = fixture
            .store
            .db
            .execute(
                "SELECT COUNT(*) FROM plugin_execution_grants WHERE job_id=?",
                &[DbValue::Int(created.id)],
            )
            .await
            .unwrap();
        assert_eq!(rows.rows[0].get_int(0).unwrap(), 0);
        assert!(
            fixture
                .store
                .claim_plugin_jobs(&fixture.prepared.host_id, 0, &"c".repeat(32), 22)
                .await
                .unwrap()
                .jobs
                .is_empty()
        );
    });
}

#[test]
fn chained_jobs_keep_conversation_origin_without_inheriting_chat_or_manual_authority() {
    block_on(async {
        for mode in [WorkspaceMode::Local, WorkspaceMode::Remote] {
            let fixture = Fixture::new(mode).await;
            let session = fixture
                .store
                .create_session("origin", None, None, Some(fixture.project), fixture.user, 0)
                .await
                .unwrap()
                .id;
            let context = PluginExecutionContext {
                session_id: Some(session),
                ..fixture.context.clone()
            };
            fixture
                .store
                .issue_plugin_context_grant(
                    fixture.user,
                    &context,
                    &fixture.prepared,
                    &"e".repeat(32),
                    1,
                )
                .await
                .unwrap();
            fixture
                .store
                .plugin_host_request(
                    fixture.user,
                    session,
                    &PluginHostRequest {
                        grant: "e".repeat(32),
                        capability: "jobs".into(),
                        payload: encode(&Fixture::scheduled("origin")).unwrap(),
                    },
                    2,
                )
                .await
                .unwrap();
            let delivery = fixture
                .store
                .claim_plugin_jobs(&fixture.prepared.host_id, 0, &"b".repeat(32), 20)
                .await
                .unwrap()
                .jobs
                .remove(0);
            fixture
                .store
                .issue_plugin_job_grant(
                    fixture.user,
                    &fixture.prepared.host_id,
                    delivery.job.id,
                    &delivery.lease,
                    &"d".repeat(32),
                    21,
                )
                .await
                .unwrap();
            let child = JobRequest::Schedule {
                key: "child".into(),
                due_at: 40,
                expires_at: None,
                event: "due".into(),
                payload: json!({}),
            };
            let request = PluginHostRequest {
                grant: "d".repeat(32),
                capability: "jobs".into(),
                payload: encode(&child).unwrap(),
            };
            assert!(
                fixture
                    .store
                    .plugin_host_request(fixture.user, session, &request, 22)
                    .await
                    .is_err()
            );
            fixture
                .store
                .plugin_context_host_request(fixture.user, &request, 22)
                .await
                .unwrap();
            let child = fixture
                .store
                .claim_plugin_jobs(&fixture.prepared.host_id, 0, &"c".repeat(32), 40)
                .await
                .unwrap()
                .jobs
                .remove(0);
            assert_eq!(child.context.session_id, Some(session));
            assert!(!child.context.user_action);
            fixture
                .store
                .delete_session(session, fixture.user)
                .await
                .unwrap();
            assert!(
                fixture
                    .store
                    .plugin_context_host_request(
                        fixture.user,
                        &PluginHostRequest {
                            payload: encode(&JobRequest::Schedule {
                                key: "orphan".into(),
                                due_at: 50,
                                expires_at: None,
                                event: "due".into(),
                                payload: json!({})
                            })
                            .unwrap(),
                            ..request
                        },
                        41
                    )
                    .await
                    .is_err()
            );
            assert!(
                fixture
                    .store
                    .claim_plugin_jobs(&fixture.prepared.host_id, 0, &"f".repeat(32), 160)
                    .await
                    .unwrap()
                    .jobs
                    .is_empty()
            );
        }
    });
}
