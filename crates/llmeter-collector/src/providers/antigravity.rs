use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use llmeter_core::{Provider, ProviderDetection, SourceFile, SourceFormat, TokenCounts};
use rusqlite::{Connection, OpenFlags};

use super::{ParsedUsage, ProviderAdapter, data_status, home_dir, project_name};
use crate::sqlite::table_has_columns;

const ANTIGRAVITY_PARSER_VERSION: u32 = 1;

pub struct AntigravityAdapter {
    roots: Vec<PathBuf>,
}

impl Default for AntigravityAdapter {
    fn default() -> Self {
        let home = home_dir();
        Self::with_roots(vec![
            home.join(".gemini").join("antigravity"),
            home.join(".gemini").join("antigravity-acp"),
            home.join(".gemini").join("antigravity-cli"),
            home.join(".gemini").join("antigravity-ide"),
        ])
    }
}

impl AntigravityAdapter {
    pub fn with_roots(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    fn conversation_dirs(&self) -> Vec<PathBuf> {
        self.roots
            .iter()
            .map(|root| root.join("conversations"))
            .collect()
    }

    fn sqlite_files(&self) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for dir in self.conversation_dirs() {
            if !dir.exists() {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() && path.extension().is_some_and(|ext| ext == "db") {
                        files.push(path);
                    }
                }
            }
        }
        files.sort();
        Ok(files)
    }
}

impl ProviderAdapter for AntigravityAdapter {
    fn provider(&self) -> Provider {
        Provider::Antigravity
    }

    fn parser_version(&self) -> u32 {
        ANTIGRAVITY_PARSER_VERSION
    }

    fn watch_roots(&self) -> Vec<PathBuf> {
        self.conversation_dirs()
    }

    fn detect(&self) -> Result<ProviderDetection> {
        let files = self.sqlite_files()?;
        let roots = self.roots.clone();
        if !files.is_empty() {
            return Ok(data_status(
                Provider::Antigravity,
                roots,
                true,
                Some(format!("{} conversation databases found", files.len())),
            ));
        }
        Ok(data_status(Provider::Antigravity, roots, false, None))
    }

    fn discover_sources(&self) -> Result<Vec<SourceFile>> {
        let files = self.sqlite_files()?;
        let mut sources = Vec::with_capacity(files.len());
        for path in files {
            let session_id = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned());
            sources.push(SourceFile {
                path,
                provider: Provider::Antigravity,
                format: SourceFormat::Sqlite,
                session_id,
                project_path: None,
                project_name: None,
            });
        }
        Ok(sources)
    }

    fn parse_line(&self, _source: &SourceFile, _line: &[u8]) -> Result<Option<ParsedUsage>> {
        Ok(None)
    }

    fn parse_sqlite(&self, source: &SourceFile) -> Result<Vec<ParsedUsage>> {
        parse_antigravity_sqlite(&source.path)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ProtoValue<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

pub(crate) struct ProtoReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ProtoReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn read_varint(&mut self) -> Option<u64> {
        let mut result = 0u64;
        let mut shift = 0;
        loop {
            if self.pos >= self.data.len() {
                return None;
            }
            let byte = self.data[self.pos];
            self.pos += 1;
            result |= u64::from(byte & 0x7f) << shift;
            if (byte & 0x80) == 0 {
                break;
            }
            shift += 7;
            if shift >= 64 {
                return None;
            }
        }
        Some(result)
    }

    pub(crate) fn next_field(&mut self) -> Option<(u32, ProtoValue<'a>)> {
        let key = self.read_varint()?;
        let field_num = (key >> 3) as u32;
        let wire_type = (key & 0x7) as u8;

        match wire_type {
            0 => {
                let value = self.read_varint()?;
                Some((field_num, ProtoValue::Varint(value)))
            }
            1 => {
                self.pos += 8;
                if self.pos > self.data.len() {
                    return None;
                }
                self.next_field()
            }
            2 => {
                let len = self.read_varint()? as usize;
                if self.pos + len > self.data.len() {
                    return None;
                }
                let slice = &self.data[self.pos..self.pos + len];
                self.pos += len;
                Some((field_num, ProtoValue::Bytes(slice)))
            }
            5 => {
                self.pos += 4;
                if self.pos > self.data.len() {
                    return None;
                }
                self.next_field()
            }
            _ => None,
        }
    }
}

pub(crate) fn parse_proto_fields<'a>(data: &'a [u8]) -> HashMap<u32, Vec<ProtoValue<'a>>> {
    let mut fields = HashMap::new();
    let mut reader = ProtoReader::new(data);
    while let Some((num, val)) = reader.next_field() {
        fields.entry(num).or_insert_with(Vec::new).push(val);
    }
    fields
}

pub(crate) fn parse_antigravity_sqlite(path: &Path) -> Result<Vec<ParsedUsage>> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed to open Antigravity database {}", path.display()))?;

    // Antigravity creates the conversation file before its schema, so
    // abandoned conversations can stay table-less forever. They hold no
    // usage; skipping them keeps one empty file from failing (and, via the
    // sync loop's `?`, aborting) every other conversation database.
    let supported = table_has_columns(&connection, "steps", &["idx", "metadata", "step_payload"])?
        && table_has_columns(&connection, "gen_metadata", &["idx", "data"])?;
    if !supported {
        tracing::debug!(
            path = %path.display(),
            "antigravity conversation has no usage schema; skipping"
        );
        return Ok(Vec::new());
    }

    // Step 1: read project path and name if present
    let (project_path, project_name) = extract_project_info(&connection);

    // Step 2: read step timestamps
    let step_times = read_step_timestamps(&connection)?;

    // Step 3: read default timestamp from file modification time or fallback
    let default_timestamp = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(Utc::now);

    let session_id = path.file_stem().map(|s| s.to_string_lossy().into_owned());

    // Step 4: query gen_metadata
    let mut stmt = connection.prepare("SELECT idx, data FROM gen_metadata ORDER BY idx")?;

    let mut usages = Vec::new();
    let mut rows = stmt.query([])?;

    while let Some(row) = rows.next()? {
        let idx: i64 = row.get(0)?;
        let data: Vec<u8> = row.get(1)?;

        let top_fields = parse_proto_fields(&data);
        let Some(f1_list) = top_fields.get(&1) else {
            continue;
        };
        let Some(ProtoValue::Bytes(f1_bytes)) = f1_list.first() else {
            continue;
        };

        let f1_fields = parse_proto_fields(f1_bytes);

        // Model name: field 19
        let model = f1_fields
            .get(&19)
            .and_then(|vals| vals.first())
            .and_then(|v| match v {
                ProtoValue::Bytes(b) => std::str::from_utf8(b).ok(),
                _ => None,
            })
            .map(|s| s.to_string());

        // Token counts: field 4 (UsageMetadata)
        let Some(f4_list) = f1_fields.get(&4) else {
            continue;
        };
        let Some(ProtoValue::Bytes(f4_bytes)) = f4_list.first() else {
            continue;
        };

        let f4_fields = parse_proto_fields(f4_bytes);

        let input_tokens = f4_fields
            .get(&2)
            .and_then(|vals| vals.first())
            .map(|v| match v {
                ProtoValue::Varint(val) => *val,
                _ => 0,
            })
            .unwrap_or(0);

        let output_tokens = f4_fields
            .get(&3)
            .and_then(|vals| vals.first())
            .map(|v| match v {
                ProtoValue::Varint(val) => *val,
                _ => 0,
            })
            .unwrap_or(0);

        let cached_input_tokens = f4_fields
            .get(&5)
            .and_then(|vals| vals.first())
            .map(|v| match v {
                ProtoValue::Varint(val) => *val,
                _ => 0,
            })
            .unwrap_or(0);

        let reasoning_tokens = f4_fields
            .get(&9)
            .and_then(|vals| vals.first())
            .map(|v| match v {
                ProtoValue::Varint(val) => *val,
                _ => 0,
            })
            .unwrap_or(0);

        // Find last_step_index from repeated field 20
        let mut last_step_index = None;
        if let Some(f20_list) = f1_fields.get(&20) {
            for v in f20_list {
                if let ProtoValue::Bytes(pair_bytes) = v {
                    let pair_fields = parse_proto_fields(pair_bytes);
                    let k =
                        pair_fields
                            .get(&1)
                            .and_then(|vals| vals.first())
                            .and_then(|pv| match pv {
                                ProtoValue::Bytes(b) => std::str::from_utf8(b).ok(),
                                _ => None,
                            });
                    let val_str =
                        pair_fields
                            .get(&2)
                            .and_then(|vals| vals.first())
                            .and_then(|pv| match pv {
                                ProtoValue::Bytes(b) => std::str::from_utf8(b).ok(),
                                _ => None,
                            });
                    if k == Some("last_step_index")
                        && let Some(s) = val_str
                    {
                        last_step_index = s.parse::<i64>().ok();
                    }
                }
            }
        }

        let timestamp = last_step_index
            .and_then(|step_idx| step_times.get(&step_idx).copied())
            .unwrap_or(default_timestamp);

        let counts = TokenCounts {
            input_tokens,
            cached_input_tokens,
            cache_creation_input_tokens: 0,
            output_tokens,
            reasoning_tokens,
            total_tokens: input_tokens
                .saturating_add(cached_input_tokens)
                .saturating_add(output_tokens),
        };

        usages.push(ParsedUsage {
            counts,
            cumulative_snapshot: None,
            timestamp,
            model,
            session_id: session_id.clone(),
            project_name: project_name.clone(),
            project_path: project_path.clone(),
            source_event_id: Some(format!("gen_metadata:{idx}")),
            reported_cost_usd: None,
        });
    }

    Ok(usages)
}

fn extract_project_info(connection: &Connection) -> (Option<PathBuf>, Option<String>) {
    // Try trajectory_metadata_blob first
    if let Ok(mut stmt) =
        connection.prepare("SELECT data FROM trajectory_metadata_blob WHERE id = 'main'")
        && let Ok(mut rows) = stmt.query([])
        && let Ok(Some(row)) = rows.next()
        && let Ok(data) = row.get::<_, Vec<u8>>(0)
        && !data.is_empty()
    {
        let fields = parse_proto_fields(&data);
        for vals in fields.values() {
            for v in vals {
                if let ProtoValue::Bytes(b) = v
                    && let Ok(s) = std::str::from_utf8(b)
                {
                    if let Some(stripped) = s.strip_prefix("file://") {
                        let path = PathBuf::from(stripped);
                        let name = project_name(Some(&path));
                        return (Some(path), name);
                    }
                    if s.starts_with('/') && !s.contains('\n') {
                        let path = PathBuf::from(s);
                        let name = project_name(Some(&path));
                        return (Some(path), name);
                    }
                }
            }
        }
    }

    // Fallback: search for Cwd or DirectoryPath or absolute_path in tool call arguments in steps
    if let Ok(mut stmt) =
        connection.prepare("SELECT metadata, step_payload FROM steps ORDER BY idx LIMIT 30")
        && let Ok(mut rows) = stmt.query([])
    {
        while let Ok(Some(row)) = rows.next() {
            let metadata: Vec<u8> = row.get(0).unwrap_or_default();
            let payload: Vec<u8> = row.get(1).unwrap_or_default();

            for blob in [&metadata, &payload] {
                if let Some(path) = find_project_path_in_json(blob) {
                    let name = project_name(Some(&path));
                    return (Some(path), name);
                }
            }
        }
    }

    (None, None)
}

fn find_project_path_in_json(data: &[u8]) -> Option<PathBuf> {
    let text = std::str::from_utf8(data).ok()?;
    for key in [
        "\"Cwd\":\"",
        "\"cwd\":\"",
        "\"DirectoryPath\":\"",
        "\"SearchPath\":\"",
        "\"absolute_path\":\"",
        "\"target_file\":\"",
    ] {
        if let Some(pos) = text.find(key) {
            let start = pos + key.len();
            if let Some(end) = text[start..].find('"') {
                let candidate = &text[start..start + end];
                if candidate.starts_with('/') && !candidate.contains('\n') {
                    let mut path = PathBuf::from(candidate);
                    if path.is_file()
                        && let Some(parent) = path.parent()
                    {
                        path = parent.to_path_buf();
                    }
                    return Some(path);
                }
            }
        }
    }
    None
}

fn read_step_timestamps(connection: &Connection) -> Result<HashMap<i64, DateTime<Utc>>> {
    let mut stmt = connection.prepare("SELECT idx, metadata FROM steps ORDER BY idx")?;

    let mut map = HashMap::new();
    let mut rows = stmt.query([])?;

    while let Some(row) = rows.next()? {
        let idx: i64 = row.get(0)?;
        let metadata: Vec<u8> = row.get(1)?;

        let fields = parse_proto_fields(&metadata);
        if let Some(f1_list) = fields.get(&1)
            && let Some(ProtoValue::Bytes(time_bytes)) = f1_list.first()
        {
            let time_fields = parse_proto_fields(time_bytes);
            if let Some(f1_sec) = time_fields.get(&1)
                && let Some(ProtoValue::Varint(seconds)) = f1_sec.first()
            {
                let nanos = time_fields
                    .get(&2)
                    .and_then(|vals| vals.first())
                    .map(|v| match v {
                        ProtoValue::Varint(n) => *n as u32,
                        _ => 0,
                    })
                    .unwrap_or(0);

                if let Some(dt) = DateTime::<Utc>::from_timestamp(*seconds as i64, nanos) {
                    map.insert(idx, dt);
                }
            }
        }
    }

    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_varint_and_fields() {
        let mut reader = ProtoReader::new(&[0x08, 0x96, 0x01]);
        assert_eq!(reader.next_field(), Some((1, ProtoValue::Varint(150))));
    }

    #[test]
    fn table_less_conversation_databases_parse_to_nothing() {
        // Antigravity leaves files like this behind for abandoned
        // conversations: created, never given a schema.
        let path = std::env::temp_dir().join(format!(
            "llmeter-antigravity-empty-{}.db",
            std::process::id()
        ));
        Connection::open(&path).unwrap().execute_batch("").unwrap();
        assert!(parse_antigravity_sqlite(&path).unwrap().is_empty());
        let _ = std::fs::remove_file(path);
    }
}
