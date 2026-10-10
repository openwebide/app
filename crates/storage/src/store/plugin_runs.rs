//! Durable raw prompt submissions, scoped by existing plugin authority.
use super::*;
use crate::db::QueryRow;
use openwebide_core::plugins::{PreparedPlugin, execution::PluginExecutionContext, runs::*};
use openwebide_core::{ModelSelection, QueuedPromptKey};

const COLUMNS: &str =
    "id,revision,run_key,session_id,state,created_at,detail,message_id,permission_id";
fn encode(value: &impl serde::Serialize) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|error| StorageError::Db(error.to_string()))
}
fn run(row: &QueryRow) -> Result<PluginRun, StorageError> {
    Ok(PluginRun {
        id: row.get_int(0)?,
        revision: row.get_int(1)?,
        key: row.get_text(2)?.into(),
        session_id: row.get_int_opt(3),
        state: serde_json::from_value(serde_json::Value::String(row.get_text(4)?.into()))
            .map_err(|error| StorageError::Db(error.to_string()))?,
        created_at: row.get_int(5)?,
        detail: row.get_text(6)?.into(),
        message_id: row.get_int_opt(7),
        permission_id: row.get_text_opt(8).map(str::to_owned),
    })
}
impl<D: Db> Store<D> {
    pub(super) async fn plugin_runs_in_transaction(
        &self,
        user: UserId,
        plugin: &PreparedPlugin,
        context: &PluginExecutionContext,
        request: &RunRequest,
        now: i64,
    ) -> Result<RunResult, StorageError> {
        request.validate().map_err(StorageError::InvalidRequest)?;
        if now < 0 {
            return Err(StorageError::InvalidRequest("Invalid run clock".into()));
        }
        let scope = vec![
            DbValue::Int(user.get()),
            DbValue::Int(context.project_id.unwrap_or(0)),
            DbValue::Text(plugin.storage_namespace()),
        ];
        let mut id = None;
        match request {
            RunRequest::Submit {
                key,
                prompt,
                target,
                model,
            } => {
                if !self.plugin_namespace_enabled(user, context, plugin).await? {
                    return Err(StorageError::Conflict("Plugin is no longer enabled".into()));
                }
                let encoded = encode(request)?;
                let mut params = scope.clone();
                params.push(DbValue::Text(key.clone()));
                let existing = self.db.execute(&format!("SELECT {COLUMNS},request FROM plugin_runs WHERE user_id=? AND project_scope=? AND plugin=? AND run_key=?"), &params).await?;
                if let Some(row) = existing.rows.first() {
                    if row.get_text(9)? != encoded {
                        return Err(StorageError::Conflict(
                            "Run key already refers to another submission".into(),
                        ));
                    }
                    // A retry after source updates returns the original immutable submission.
                    return Ok(RunResult {
                        runs: vec![run(row)?],
                        next_after: None,
                    });
                }
                let count = self.db.execute("SELECT count(*) FROM plugin_runs WHERE user_id=? AND project_scope=? AND plugin=?", &scope).await?.rows[0].get_int(0)?;
                if count >= MAX_RUNS {
                    return Err(StorageError::Conflict(
                        "Plugin run history holds at most 1,000 entries; delete terminal runs"
                            .into(),
                    ));
                }
                let model = model.as_ref().or(context.primary.as_ref());
                if let Some(model) = model
                    && !self.get_connection(model.server_id).await?.enabled
                {
                    return Err(StorageError::InvalidRequest(
                        "Run model server is disabled".into(),
                    ));
                }
                let session = match target {
                    RunTarget::Origin => context.session_id.ok_or_else(|| {
                        StorageError::InvalidRequest("Run has no originating conversation".into())
                    })?,
                    RunTarget::Session { id } => *id,
                    RunTarget::New { title } => {
                        self.create_session_unlocked(
                            title,
                            model.map(|model| model.server_id),
                            None,
                            context.project_id,
                            user,
                            now,
                        )
                        .await?
                        .id
                    }
                };
                if self.get_session(session, user).await?.project_id != context.project_id {
                    return Err(StorageError::InvalidRequest(
                        "Choose a conversation in this project".into(),
                    ));
                }
                let queued = self
                    .enqueue_prompt_in_transaction(user, session, prompt, now, false)
                    .await?;
                params.extend([
                    DbValue::Text(encoded),
                    DbValue::Text(encode(plugin)?),
                    DbValue::Text(encode(context)?),
                    DbValue::Text(plugin.host_id.clone()),
                    DbValue::Int(session),
                    DbValue::Int(queued.id),
                    model
                        .map(encode)
                        .transpose()?
                        .map_or(DbValue::Null, DbValue::Text),
                    DbValue::Int(now),
                ]);
                id = Some(self.db.execute("INSERT INTO plugin_runs(user_id,project_scope,plugin,run_key,request,prepared,context,host_id,session_id,queued_id,model,created_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)", &params).await?.last_insert_rowid);
            }
            RunRequest::Cancel {
                id: selected,
                revision,
            }
            | RunRequest::Delete {
                id: selected,
                revision,
            } => {
                let mut params = scope.clone();
                params.push(DbValue::Int(*selected));
                let rows = self.db.execute(&format!("SELECT {COLUMNS},queued_id FROM plugin_runs WHERE user_id=? AND project_scope=? AND plugin=? AND id=?"), &params).await?;
                let row = rows
                    .rows
                    .first()
                    .ok_or_else(|| StorageError::NotFound("Plugin run".into()))?;
                let current = run(row)?;
                if current.revision != *revision {
                    return Err(StorageError::Conflict("Plugin run changed".into()));
                }
                if matches!(request, RunRequest::Delete { .. }) {
                    if !current.state.terminal() {
                        return Err(StorageError::Conflict(
                            "Only terminal runs can be deleted".into(),
                        ));
                    }
                    self.db
                        .execute(
                            "DELETE FROM plugin_runs WHERE id=?",
                            &[DbValue::Int(*selected)],
                        )
                        .await?;
                    return Ok(RunResult {
                        runs: Vec::new(),
                        next_after: None,
                    });
                }
                if current.state.terminal() || current.state == RunState::Cancelling {
                    return Err(StorageError::Conflict(
                        "Run is already finished or cancelling".into(),
                    ));
                }
                let delivered = current.message_id.is_some();
                let state = if delivered { "cancelling" } else { "cancelled" };
                self.db.execute("UPDATE plugin_runs SET state=?,revision=revision+1,detail='Cancellation requested',permission_id=NULL WHERE id=?", &[DbValue::Text(state.into()), DbValue::Int(*selected)]).await?;
                if let Some(queued) = row.get_int_opt(9) {
                    self.db
                        .execute(
                            "DELETE FROM queued_prompts WHERE id=?",
                            &[DbValue::Int(queued)],
                        )
                        .await?;
                }
                if delivered && let Some(session) = current.session_id {
                    let active = self.db.execute("SELECT 1 FROM session_run_leases WHERE session_id=? AND token=? AND expires_at>?", &[DbValue::Int(session), DbValue::Text(format!("plugin-run-{selected}")), DbValue::Int(now)]).await?;
                    if !active.rows.is_empty() {
                        self.request_cancel(session, now.saturating_mul(1000).saturating_add(1))
                            .await?;
                    }
                }
                id = Some(*selected);
            }
            RunRequest::Read { id: selected } => id = Some(*selected),
            RunRequest::List { .. } => (),
        }
        let mut params = scope;
        let (filter, limit) = match id {
            Some(id) => {
                params.push(DbValue::Int(id));
                ("id=?", 1)
            }
            None => {
                let RunRequest::List { after } = request else {
                    unreachable!("submission returned an ID")
                };
                params.push(DbValue::Int(*after));
                ("id>?", RUN_PAGE_SIZE + 1)
            }
        };
        let rows = self.db.execute(&format!("SELECT {COLUMNS} FROM plugin_runs WHERE user_id=? AND project_scope=? AND plugin=? AND {filter} ORDER BY id LIMIT {limit}"), &params).await?;
        if id.is_some() && rows.rows.is_empty() {
            return Err(StorageError::NotFound("Plugin run".into()));
        }
        let mut runs = rows.rows.iter().map(run).collect::<Result<Vec<_>, _>>()?;
        let next_after = if runs.len() > usize::try_from(RUN_PAGE_SIZE).expect("bounded page size")
        {
            runs.pop();
            runs.last().map(|run| run.id)
        } else {
            None
        };
        Ok(RunResult { runs, next_after })
    }
    /// Selected model belongs to the immutable queued submission, not its conversation.
    pub async fn plugin_prompt_model(
        &self,
        user: UserId,
        session: i64,
        key: QueuedPromptKey,
    ) -> Result<Option<ModelSelection>, StorageError> {
        self.get_session(session, user).await?;
        let result = self.db.execute("SELECT r.model FROM plugin_runs r JOIN queued_prompts q ON q.id=r.queued_id WHERE r.user_id=? AND r.session_id=? AND q.id=? AND q.revision=?", &[DbValue::Int(user.get()), DbValue::Int(session), DbValue::Int(key.id), DbValue::Int(key.revision)]).await?;
        result
            .rows
            .first()
            .and_then(|row| row.get_text_opt(0))
            .map(serde_json::from_str)
            .transpose()
            .map_err(|error| StorageError::Db(error.to_string()))
    }
}

#[cfg(test)]
mod tests;
