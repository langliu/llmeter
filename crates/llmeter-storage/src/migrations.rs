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
            reported_cost_usd REAL,
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

        CREATE TABLE IF NOT EXISTS limit_snapshots (
            provider TEXT PRIMARY KEY,
            payload_json TEXT NOT NULL,
            captured_at INTEGER NOT NULL
        );
        "#,
    )?;

    if !has_column(connection, "file_cursors", "source_metadata_json")? {
        connection.execute(
            "ALTER TABLE file_cursors ADD COLUMN source_metadata_json TEXT",
            [],
        )?;
    }
    if !has_column(connection, "usage_events", "reported_cost_usd")? {
        connection.execute(
            "ALTER TABLE usage_events ADD COLUMN reported_cost_usd REAL",
            [],
        )?;
    }
    connection.pragma_update(None, "user_version", 4)?;
    Ok(())
}

fn has_column(connection: &Connection, table: &str, expected: &str) -> Result<bool, StorageError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    Ok(columns
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|column| column == expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_reported_cost_column_without_losing_existing_usage() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE usage_events (
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
                INSERT INTO usage_events (
                    id, provider, timestamp, total_tokens, created_at
                ) VALUES ('existing', 'grok', 1, 42, 1);",
            )
            .unwrap();

        run(&connection).unwrap();

        assert!(has_column(&connection, "usage_events", "reported_cost_usd").unwrap());
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM usage_events", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            4
        );
    }
}
