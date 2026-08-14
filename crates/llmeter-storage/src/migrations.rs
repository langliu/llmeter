use rusqlite::Connection;

use crate::StorageError;

pub fn run(connection: &Connection) -> Result<(), StorageError> {
    connection.execute_batch(
        r#"
        PRAGMA foreign_keys = ON;
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;

        CREATE TABLE IF NOT EXISTS usage_events (
            id TEXT PRIMARY KEY,
            provider TEXT NOT NULL,
            model TEXT,
            session_id TEXT,
            project_path TEXT,
            project_name TEXT,
            timestamp INTEGER NOT NULL,
            input_tokens INTEGER NOT NULL DEFAULT 0,
            cached_input_tokens INTEGER NOT NULL DEFAULT 0,
            cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
            output_tokens INTEGER NOT NULL DEFAULT 0,
            reasoning_tokens INTEGER NOT NULL DEFAULT 0,
            total_tokens INTEGER NOT NULL DEFAULT 0,
            estimated_cost_usd REAL,
            source_file TEXT,
            source_event_id TEXT,
            created_at INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_usage_events_timestamp ON usage_events(timestamp);
        CREATE INDEX IF NOT EXISTS idx_usage_events_provider ON usage_events(provider);
        CREATE INDEX IF NOT EXISTS idx_usage_events_model ON usage_events(model);
        CREATE INDEX IF NOT EXISTS idx_usage_events_project_path ON usage_events(project_path);
        CREATE INDEX IF NOT EXISTS idx_usage_events_session_id ON usage_events(session_id);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_usage_events_provider_source_event
            ON usage_events(provider, source_event_id)
            WHERE source_event_id IS NOT NULL;

        CREATE TABLE IF NOT EXISTS file_cursors (
            path TEXT PRIMARY KEY,
            provider TEXT NOT NULL,
            file_identity TEXT,
            byte_offset INTEGER NOT NULL DEFAULT 0,
            file_size INTEGER NOT NULL DEFAULT 0,
            modified_at INTEGER,
            parser_version INTEGER NOT NULL,
            last_event_hash TEXT,
            last_cumulative_json TEXT,
            source_metadata_json TEXT,
            updated_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS app_settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        "#,
    )?;

    let has_source_metadata = {
        let mut statement = connection.prepare("PRAGMA table_info(file_cursors)")?;
        let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
        columns
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|column| column == "source_metadata_json")
    };
    if !has_source_metadata {
        connection.execute(
            "ALTER TABLE file_cursors ADD COLUMN source_metadata_json TEXT",
            [],
        )?;
    }
    connection.pragma_update(None, "user_version", 2)?;
    Ok(())
}
