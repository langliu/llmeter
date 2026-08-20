use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use llmeter_core::{FileCursor, Provider, TokenCounts, UsageEvent};
use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

use crate::migrations;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("database mutex was poisoned")]
    MutexPoisoned,
    #[error("numeric value cannot be represented in SQLite")]
    NumericOverflow,
}

#[derive(Clone)]
pub struct Database {
    pub(crate) connection: Arc<Mutex<Connection>>,
    path: PathBuf,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InsertSummary {
    pub inserted: usize,
    pub tokens_added: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UpsertSummary {
    pub inserted: usize,
    pub updated: usize,
    pub tokens_added: u64,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(&path)?;
        migrations::run(&connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            path,
        })
    }

    pub fn open_in_memory() -> Result<Self, StorageError> {
        let connection = Connection::open_in_memory()?;
        migrations::run(&connection)?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            path: PathBuf::from(":memory:"),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, Connection>, StorageError> {
        self.connection
            .lock()
            .map_err(|_| StorageError::MutexPoisoned)
    }

    pub fn insert_usage_events(&self, events: &[UsageEvent]) -> Result<usize, StorageError> {
        Ok(self.insert_usage_events_with_summary(events)?.inserted)
    }

    pub fn insert_usage_events_with_summary(
        &self,
        events: &[UsageEvent],
    ) -> Result<InsertSummary, StorageError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let mut statement = transaction.prepare(
            "INSERT OR IGNORE INTO usage_events (
                id, provider, model, session_id, project_path, project_name, timestamp,
                input_tokens, cached_input_tokens, cache_creation_input_tokens, output_tokens,
                reasoning_tokens, total_tokens, reported_cost_usd, estimated_cost_usd,
                source_file, source_event_id, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        )?;
        let mut summary = InsertSummary::default();
        for event in events {
            let inserted = statement.execute(params![
                event.id,
                event.provider.as_str(),
                event.model,
                event.session_id,
                event
                    .project_path
                    .as_ref()
                    .map(|value| value.to_string_lossy().to_string()),
                event.project_name,
                event.timestamp.timestamp(),
                to_sqlite_i64(event.input_tokens)?,
                to_sqlite_i64(event.cached_input_tokens)?,
                to_sqlite_i64(event.cache_creation_input_tokens)?,
                to_sqlite_i64(event.output_tokens)?,
                to_sqlite_i64(event.reasoning_tokens)?,
                to_sqlite_i64(event.total_tokens)?,
                event.reported_cost_usd,
                event.estimated_cost_usd,
                event
                    .source_file
                    .as_ref()
                    .map(|value| value.to_string_lossy().to_string()),
                event.source_event_id,
                chrono::Utc::now().timestamp(),
            ])?;
            if inserted > 0 {
                summary.inserted += inserted;
                summary.tokens_added = summary.tokens_added.saturating_add(event.total_tokens);
            }
        }
        drop(statement);
        transaction.commit()?;
        Ok(summary)
    }

    pub fn upsert_usage_events_with_summary(
        &self,
        events: &[UsageEvent],
    ) -> Result<UpsertSummary, StorageError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let mut existing_statement =
            transaction.prepare("SELECT 1 FROM usage_events WHERE id = ?1 LIMIT 1")?;
        let mut statement = transaction.prepare(
            "INSERT INTO usage_events (
                id, provider, model, session_id, project_path, project_name, timestamp,
                input_tokens, cached_input_tokens, cache_creation_input_tokens, output_tokens,
                reasoning_tokens, total_tokens, reported_cost_usd, estimated_cost_usd,
                source_file, source_event_id, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
            ON CONFLICT(id) DO UPDATE SET
                provider = excluded.provider,
                model = excluded.model,
                session_id = excluded.session_id,
                project_path = excluded.project_path,
                project_name = excluded.project_name,
                timestamp = excluded.timestamp,
                input_tokens = excluded.input_tokens,
                cached_input_tokens = excluded.cached_input_tokens,
                cache_creation_input_tokens = excluded.cache_creation_input_tokens,
                output_tokens = excluded.output_tokens,
                reasoning_tokens = excluded.reasoning_tokens,
                total_tokens = excluded.total_tokens,
                reported_cost_usd = excluded.reported_cost_usd,
                estimated_cost_usd = excluded.estimated_cost_usd,
                source_file = excluded.source_file,
                source_event_id = excluded.source_event_id",
        )?;
        let mut summary = UpsertSummary::default();
        for event in events {
            let exists = existing_statement
                .query_row(params![event.id], |_| Ok(()))
                .optional()?
                .is_some();
            statement.execute(params![
                event.id,
                event.provider.as_str(),
                event.model,
                event.session_id,
                event
                    .project_path
                    .as_ref()
                    .map(|value| value.to_string_lossy().to_string()),
                event.project_name,
                event.timestamp.timestamp(),
                to_sqlite_i64(event.input_tokens)?,
                to_sqlite_i64(event.cached_input_tokens)?,
                to_sqlite_i64(event.cache_creation_input_tokens)?,
                to_sqlite_i64(event.output_tokens)?,
                to_sqlite_i64(event.reasoning_tokens)?,
                to_sqlite_i64(event.total_tokens)?,
                event.reported_cost_usd,
                event.estimated_cost_usd,
                event
                    .source_file
                    .as_ref()
                    .map(|value| value.to_string_lossy().to_string()),
                event.source_event_id,
                chrono::Utc::now().timestamp(),
            ])?;
            if exists {
                summary.updated += 1;
            } else {
                summary.inserted += 1;
                summary.tokens_added = summary.tokens_added.saturating_add(event.total_tokens);
            }
        }
        drop(statement);
        drop(existing_statement);
        transaction.commit()?;
        Ok(summary)
    }

    pub fn get_cursor(&self, path: &Path) -> Result<Option<FileCursor>, StorageError> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT path, provider, file_identity, byte_offset, file_size, modified_at,
                        parser_version, last_event_hash, last_cumulative_json,
                        source_metadata_json, updated_at
                 FROM file_cursors WHERE path = ?1",
                params![path.to_string_lossy().to_string()],
                |row| {
                    let provider_text: String = row.get(1)?;
                    let provider = provider_text.parse::<Provider>().map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
                        )
                    })?;
                    let cumulative_json: Option<String> = row.get(8)?;
                    let last_cumulative = cumulative_json
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()
                        .map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                8,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?;
                    let source_metadata_json: Option<String> = row.get(9)?;
                    let source_metadata = source_metadata_json
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()
                        .map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                9,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?
                        .unwrap_or_default();
                    Ok(FileCursor {
                        path: PathBuf::from(row.get::<_, String>(0)?),
                        provider,
                        file_identity: row.get(2)?,
                        byte_offset: from_sqlite_u64(row.get(3)?),
                        file_size: from_sqlite_u64(row.get(4)?),
                        modified_at: row.get(5)?,
                        parser_version: row.get::<_, i64>(6)?.max(0) as u32,
                        last_event_hash: row.get(7)?,
                        last_cumulative,
                        source_metadata,
                        updated_at: row.get(10)?,
                    })
                },
            )
            .optional()
            .map_err(StorageError::from)
    }

    pub fn upsert_cursor(&self, cursor: &FileCursor) -> Result<(), StorageError> {
        let connection = self.lock()?;
        let cumulative_json = cursor
            .last_cumulative
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let source_metadata_json = serde_json::to_string(&cursor.source_metadata)?;
        connection.execute(
            "INSERT INTO file_cursors (
                path, provider, file_identity, byte_offset, file_size, modified_at,
                parser_version, last_event_hash, last_cumulative_json, source_metadata_json,
                updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ON CONFLICT(path) DO UPDATE SET
                provider = excluded.provider,
                file_identity = excluded.file_identity,
                byte_offset = excluded.byte_offset,
                file_size = excluded.file_size,
                modified_at = excluded.modified_at,
                parser_version = excluded.parser_version,
                last_event_hash = excluded.last_event_hash,
                last_cumulative_json = excluded.last_cumulative_json,
                source_metadata_json = excluded.source_metadata_json,
                updated_at = excluded.updated_at",
            params![
                cursor.path.to_string_lossy().to_string(),
                cursor.provider.as_str(),
                cursor.file_identity,
                to_sqlite_i64(cursor.byte_offset)?,
                to_sqlite_i64(cursor.file_size)?,
                cursor.modified_at,
                i64::from(cursor.parser_version),
                cursor.last_event_hash,
                cumulative_json,
                source_metadata_json,
                cursor.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>, StorageError> {
        let connection = self.lock()?;
        connection
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(StorageError::from)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), StorageError> {
        let connection = self.lock()?;
        connection.execute(
            "INSERT INTO app_settings(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn delete_usage_for_source(
        &self,
        path: &Path,
        provider: Provider,
    ) -> Result<usize, StorageError> {
        let connection = self.lock()?;
        connection
            .execute(
                "DELETE FROM usage_events WHERE provider = ?1 AND source_file = ?2",
                params![provider.as_str(), path.to_string_lossy().to_string()],
            )
            .map_err(StorageError::from)
    }

    pub fn clear_usage_and_cursors(&self) -> Result<(), StorageError> {
        let connection = self.lock()?;
        connection.execute_batch("DELETE FROM usage_events; DELETE FROM file_cursors;")?;
        Ok(())
    }

    pub fn list_usage_for_pricing(&self) -> Result<Vec<UsagePricingInput>, StorageError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id, provider, model, input_tokens, cached_input_tokens,
                    cache_creation_input_tokens, output_tokens, reasoning_tokens,
                    estimated_cost_usd
             FROM usage_events",
        )?;
        let rows = statement.query_map([], |row| {
            let provider_text: String = row.get(1)?;
            let provider = provider_text.parse::<Provider>().map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
                )
            })?;
            Ok(UsagePricingInput {
                id: row.get(0)?,
                provider,
                model: row.get(2)?,
                counts: TokenCounts {
                    input_tokens: from_sqlite_u64(row.get(3)?),
                    cached_input_tokens: from_sqlite_u64(row.get(4)?),
                    cache_creation_input_tokens: from_sqlite_u64(row.get(5)?),
                    output_tokens: from_sqlite_u64(row.get(6)?),
                    reasoning_tokens: from_sqlite_u64(row.get(7)?),
                    total_tokens: 0,
                },
                estimated_cost_usd: row.get(8)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    pub fn update_estimated_costs(
        &self,
        updates: &[(String, Option<f64>)],
    ) -> Result<usize, StorageError> {
        if updates.is_empty() {
            return Ok(0);
        }
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        {
            let mut statement = transaction
                .prepare("UPDATE usage_events SET estimated_cost_usd = ?2 WHERE id = ?1")?;
            for (id, cost) in updates {
                statement.execute(params![id, cost])?;
            }
        }
        transaction.commit()?;
        Ok(updates.len())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsagePricingInput {
    pub id: String,
    pub provider: Provider,
    pub model: Option<String>,
    pub counts: TokenCounts,
    pub estimated_cost_usd: Option<f64>,
}

fn to_sqlite_i64(value: u64) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::NumericOverflow)
}

pub(crate) fn from_sqlite_u64(value: i64) -> u64 {
    value.max(0) as u64
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::Utc;
    use llmeter_core::{Provider, UsageEvent, UsageSnapshot};

    use super::*;
    use crate::UsageRepository;

    fn event(id: &str, source_event_id: Option<&str>, total: u64) -> UsageEvent {
        UsageEvent {
            id: id.into(),
            provider: Provider::Codex,
            model: Some("gpt-5.4".into()),
            session_id: Some("session".into()),
            project_path: Some(PathBuf::from("/tmp/project")),
            project_name: Some("project".into()),
            timestamp: Utc::now(),
            input_tokens: total,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            total_tokens: total,
            reported_cost_usd: None,
            estimated_cost_usd: Some(0.1),
            source_file: Some(PathBuf::from("/tmp/session.jsonl")),
            source_event_id: source_event_id.map(str::to_string),
        }
    }

    #[test]
    fn duplicate_event_id_and_source_id_do_not_double_count() {
        let database = Database::open_in_memory().unwrap();
        let first = event("one", Some("official-1"), 100);
        let duplicate_id = event("one", Some("official-1"), 100);
        let duplicate_source = event("different-id", Some("official-1"), 100);
        let summary = database
            .insert_usage_events_with_summary(&[first, duplicate_id, duplicate_source])
            .unwrap();
        assert_eq!(summary.inserted, 1);
        assert_eq!(summary.tokens_added, 100);

        let repository = UsageRepository::new(database);
        let start = chrono::DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let overview = repository
            .get_overview(start, Utc::now() + chrono::Duration::seconds(1))
            .unwrap();
        assert_eq!(overview.total_tokens, 100);
        assert_eq!(overview.event_count, 1);
    }

    #[test]
    fn reported_cost_takes_priority_in_usage_aggregates() {
        let database = Database::open_in_memory().unwrap();
        let mut usage = event("reported", Some("reported-1"), 100);
        usage.cached_input_tokens = 60;
        usage.cache_creation_input_tokens = 7;
        usage.reported_cost_usd = Some(0.25);
        usage.estimated_cost_usd = Some(9.0);
        database.insert_usage_events(&[usage]).unwrap();

        let repository = UsageRepository::new(database);
        let start = chrono::DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let end = Utc::now() + chrono::Duration::seconds(1);
        assert_eq!(
            repository
                .get_overview(start, end)
                .unwrap()
                .estimated_cost_usd,
            Some(0.25)
        );
        assert_eq!(
            repository.get_daily_usage(start, end).unwrap()[0].estimated_cost_usd,
            Some(0.25)
        );
        let provider_usage = &repository.get_provider_usage(start, end).unwrap()[0];
        assert_eq!(provider_usage.cached_input_tokens, 60);
        assert_eq!(provider_usage.cache_creation_input_tokens, 7);
        assert_eq!(provider_usage.estimated_cost_usd, Some(0.25));
        assert_eq!(
            repository.get_model_usage(start, end).unwrap()[0].estimated_cost_usd,
            Some(0.25)
        );
        assert_eq!(
            repository.get_project_usage(start, end).unwrap()[0].estimated_cost_usd,
            Some(0.25)
        );
        assert_eq!(
            repository.get_sessions().unwrap()[0].estimated_cost_usd,
            Some(0.25)
        );
    }

    #[test]
    fn cursor_round_trip_preserves_identity_offset_cumulative_state_and_metadata() {
        let database = Database::open_in_memory().unwrap();
        let mut cursor = FileCursor::new(PathBuf::from("/tmp/session.jsonl"), Provider::Codex, 7);
        cursor.file_identity = Some("dev:ino".into());
        cursor.byte_offset = 42;
        cursor.file_size = 50;
        cursor.last_cumulative = Some(UsageSnapshot {
            input_tokens: 40,
            total_tokens: 42,
            ..Default::default()
        });
        cursor.source_metadata.model = Some("gpt-5.6-sol".into());
        cursor.source_metadata.project_path = Some(PathBuf::from("/tmp/project"));
        database.upsert_cursor(&cursor).unwrap();
        assert_eq!(database.get_cursor(&cursor.path).unwrap(), Some(cursor));
    }

    #[test]
    fn sessions_are_aggregated_from_usage_events() {
        let database = Database::open_in_memory().unwrap();
        let started = Utc::now();
        let mut first = event("one", Some("a"), 100);
        first.timestamp = started;
        first.session_id =
            Some("rollout-2026-08-14T15-01-51-019fff13-bd73-7c71-a844-4dbe59993141".into());
        first.project_name = Some("fe-ai-nexus".into());
        let mut second = event("two", Some("b"), 50);
        second.timestamp = started + chrono::Duration::minutes(12);
        second.session_id = first.session_id.clone();
        second.project_name = Some("fe-ai-nexus".into());
        database
            .insert_usage_events_with_summary(&[first, second])
            .unwrap();

        let sessions = UsageRepository::new(database).get_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title(), "fe-ai-nexus");
        assert_eq!(sessions[0].turn_count, 2);
        assert_eq!(sessions[0].total_tokens, 150);
        assert_eq!(
            sessions[0].resume_command().as_deref(),
            Some("codex resume 019fff13-bd73-7c71-a844-4dbe59993141")
        );
    }

    #[test]
    fn project_usage_merges_same_name_across_worktree_paths() {
        let database = Database::open_in_memory().unwrap();
        let started = Utc::now() - chrono::Duration::hours(1);
        let mut main = event("main", Some("a"), 100);
        main.timestamp = started;
        main.project_name = Some("workstation-web".into());
        main.project_path = Some(PathBuf::from(
            "/Users/liulang/WebstormProjects/workstation-web",
        ));
        let mut worktree = event("worktree", Some("b"), 50);
        worktree.timestamp = started + chrono::Duration::minutes(5);
        worktree.project_name = Some("workstation-web".into());
        worktree.project_path = Some(PathBuf::from(
            "/Users/liulang/WebstormProjects/worktrees/workstation-web/loyal-yak/workstation-web",
        ));
        let mut other = event("other", Some("c"), 25);
        other.timestamp = started + chrono::Duration::minutes(6);
        other.project_name = Some("llmeter".into());
        other.project_path = Some(PathBuf::from("/Users/liulang/lang-projects/llmeter"));
        database
            .insert_usage_events_with_summary(&[main, worktree, other])
            .unwrap();

        let start = chrono::DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let projects = UsageRepository::new(database)
            .get_project_usage(start, Utc::now() + chrono::Duration::seconds(1))
            .unwrap();
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].project_name, "workstation-web");
        assert_eq!(projects[0].total_tokens, 150);
        assert_eq!(projects[1].project_name, "llmeter");
        assert_eq!(projects[1].total_tokens, 25);
    }
}
