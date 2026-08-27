use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Read},
    path::Path,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use llmeter_core::Provider;
use llmeter_storage::SessionSummary;
use rusqlite::{Connection, OpenFlags, Row, types::ValueRef};
use serde_json::Value;

const MAX_SOURCE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_MESSAGES: usize = 1_000;
const MAX_MESSAGE_CHARS: usize = 120_000;

/// The kinds of conversation records that can be recovered from a provider's
/// local session store. Thinking is only shown when the provider actually
/// persisted it in the local log; LLMeter never reconstructs or invents it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TranscriptRole {
    User,
    Assistant,
    Thinking,
    Tool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptMessage {
    pub role: TranscriptRole,
    pub content: String,
    pub timestamp: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionTranscript {
    pub messages: Vec<TranscriptMessage>,
    pub truncated: bool,
}

/// Read a session's original local source on demand.
///
/// This is deliberately a read-only, bounded operation. Transcript content is
/// returned to the caller and is never inserted into the usage database.
pub fn load_session_transcript(session: &SessionSummary) -> Result<SessionTranscript> {
    let source = session
        .source_file
        .as_deref()
        .context("this session has no local transcript source")?;
    let path = Path::new(source);
    if !path.is_file() {
        bail!("the local session source is no longer available");
    }

    match session.provider {
        Provider::Claude | Provider::Codex | Provider::Pi | Provider::Omp => {
            load_jsonl(path, session)
        }
        Provider::Grok => {
            let history = path
                .parent()
                .map(|parent| parent.join("chat_history.jsonl"))
                .filter(|candidate| candidate.is_file())
                .unwrap_or_else(|| path.to_path_buf());
            load_jsonl(&history, session)
        }
        Provider::OpenCode => {
            if looks_like_sqlite(path) {
                load_opencode_sqlite(path, session)
            } else {
                load_jsonl(path, session)
            }
        }
        Provider::Qoder => load_qoder_sqlite(path, session),
        Provider::Zed => load_zed_sqlite(path, session),
        Provider::Hermes => load_hermes_sqlite(path, session),
        Provider::Cursor | Provider::Trae => bail!(
            "{} only provides account usage data locally; its conversation content is not available",
            session.provider.display_name()
        ),
    }
}

#[derive(Default)]
struct TranscriptBuilder {
    messages: Vec<TranscriptMessage>,
    truncated: bool,
}

impl TranscriptBuilder {
    fn push(
        &mut self,
        role: TranscriptRole,
        content: impl Into<String>,
        timestamp: Option<DateTime<Utc>>,
    ) {
        if self.messages.len() >= MAX_MESSAGES {
            self.truncated = true;
            return;
        }
        let content = content.into();
        let content = content.trim();
        if content.is_empty() {
            return;
        }
        let content = if content.chars().count() > MAX_MESSAGE_CHARS {
            self.truncated = true;
            let prefix = content.chars().take(MAX_MESSAGE_CHARS).collect::<String>();
            format!("{prefix}\n…")
        } else {
            content.to_string()
        };
        if self
            .messages
            .last()
            .is_some_and(|last| last.role == role && last.content == content)
        {
            return;
        }
        self.messages.push(TranscriptMessage {
            role,
            content,
            timestamp,
        });
    }

    fn finish(self) -> SessionTranscript {
        SessionTranscript {
            messages: self.messages,
            truncated: self.truncated,
        }
    }
}

fn load_jsonl(path: &Path, session: &SessionSummary) -> Result<SessionTranscript> {
    let file = File::open(path)
        .with_context(|| format!("open local session transcript {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut builder = TranscriptBuilder::default();
    let mut line = String::new();
    let mut bytes_read = 0u64;

    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        let bytes = bytes as u64;
        if bytes_read.saturating_add(bytes) > MAX_SOURCE_BYTES {
            builder.truncated = true;
            break;
        }
        bytes_read = bytes_read.saturating_add(bytes);

        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if !record_matches_session(&value, session.session_id.as_deref()) {
            continue;
        }
        parse_json_record(session.provider, &value, &mut builder);
        if builder.messages.len() >= MAX_MESSAGES {
            builder.truncated = true;
            break;
        }
    }

    Ok(builder.finish())
}

fn parse_json_record(provider: Provider, value: &Value, builder: &mut TranscriptBuilder) {
    if provider == Provider::Codex {
        parse_codex_record(value, builder);
    } else {
        parse_generic_record(value, None, builder);
    }
}

fn parse_codex_record(value: &Value, builder: &mut TranscriptBuilder) {
    let timestamp = record_timestamp(value);
    let root_type = string_field(value, "type")
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();

    if root_type == "event_msg" {
        let payload = field(value, "payload").unwrap_or(value);
        let event_type = string_field(payload, "type")
            .map(|value| value.to_ascii_lowercase())
            .unwrap_or_default();
        let role = match event_type.as_str() {
            "user_message" => Some(TranscriptRole::User),
            "agent_message" | "assistant_message" => Some(TranscriptRole::Assistant),
            _ => None,
        };
        if let Some(role) = role {
            let content = field(payload, "message")
                .or_else(|| field(payload, "content"))
                .or_else(|| field(payload, "text"));
            if let Some(content) = content {
                append_content(builder, role, content, timestamp);
            }
        }
        return;
    }

    if root_type == "response_item" {
        let payload = field(value, "payload").unwrap_or(value);
        let item_type = string_field(payload, "type")
            .map(|value| value.to_ascii_lowercase())
            .unwrap_or_default();
        if item_type == "reasoning" {
            let content = field(payload, "summary")
                .or_else(|| field(payload, "content"))
                .or_else(|| field(payload, "text"));
            if let Some(content) = content {
                append_content(builder, TranscriptRole::Thinking, content, timestamp);
            }
        } else if item_type == "message" {
            let role = string_field(payload, "role")
                .and_then(parse_role)
                .unwrap_or(TranscriptRole::Assistant);
            if let Some(content) = field(payload, "content") {
                append_content(builder, role, content, timestamp);
            }
        }
        return;
    }

    parse_generic_record(value, None, builder);
}

fn parse_generic_record(
    value: &Value,
    fallback_role: Option<TranscriptRole>,
    builder: &mut TranscriptBuilder,
) {
    let timestamp = record_timestamp(value);

    if let Some(message) = field(value, "message")
        && let Some(message_object) = message.as_object()
        && let Some(role) = string_field(message, "role").and_then(parse_role)
    {
        let content = field(message, "content")
            .or_else(|| field(message, "text"))
            .or_else(|| field(message, "output_text"));
        if let Some(content) = content {
            append_content(builder, role, content, timestamp);
        }
        // A few formats put the useful fields next to `role` in the message
        // object. Do not recurse through metadata when no content was found.
        if content.is_some() || message_object.is_empty() {
            return;
        }
    }

    let role = string_field(value, "role")
        .and_then(parse_role)
        .or_else(|| string_field(value, "sender").and_then(parse_role))
        .or_else(|| string_field(value, "author").and_then(parse_role))
        .or_else(|| string_field(value, "type").and_then(parse_role))
        .or(fallback_role);
    let Some(role) = role else {
        return;
    };

    let content = field(value, "content")
        .or_else(|| field(value, "text"))
        .or_else(|| field(value, "output_text"))
        .or_else(|| field(value, "thinking"))
        .or_else(|| field(value, "reasoning"));
    if let Some(content) = content {
        append_content(builder, role, content, timestamp);
    }
}

fn append_content(
    builder: &mut TranscriptBuilder,
    role: TranscriptRole,
    value: &Value,
    timestamp: Option<DateTime<Utc>>,
) {
    match value {
        Value::String(content) => builder.push(role, content, timestamp),
        Value::Array(values) => {
            for value in values {
                append_content(builder, role, value, timestamp);
                if builder.messages.len() >= MAX_MESSAGES {
                    builder.truncated = true;
                    break;
                }
            }
        }
        Value::Object(_) => append_content_object(builder, role, value, timestamp),
        _ => {}
    }
}

fn append_content_object(
    builder: &mut TranscriptBuilder,
    role: TranscriptRole,
    value: &Value,
    timestamp: Option<DateTime<Utc>>,
) {
    let kind = string_field(value, "type")
        .map(|kind| kind.to_ascii_lowercase())
        .unwrap_or_default();
    match kind.as_str() {
        "thinking" | "reasoning" | "reasoning_summary" | "summary_text" => {
            let content = field(value, "thinking")
                .or_else(|| field(value, "reasoning"))
                .or_else(|| field(value, "summary"))
                .or_else(|| field(value, "text"))
                .or_else(|| field(value, "content"));
            if let Some(content) = content {
                append_content(builder, TranscriptRole::Thinking, content, timestamp);
            }
        }
        "text" | "output_text" | "input_text" => {
            let content = field(value, "text")
                .or_else(|| field(value, "output_text"))
                .or_else(|| field(value, "input_text"))
                .or_else(|| field(value, "content"));
            if let Some(content) = content {
                append_content(builder, role, content, timestamp);
            }
        }
        "tool_use" | "tool_call" | "function_call" => {
            let content = tool_call_text(value);
            if !content.is_empty() {
                builder.push(TranscriptRole::Tool, content, timestamp);
            }
        }
        "tool_result" | "tool" => {
            let content = field(value, "content")
                .or_else(|| field(value, "result"))
                .or_else(|| field(value, "output"))
                .or_else(|| field(value, "text"));
            if let Some(content) = content {
                append_content(builder, TranscriptRole::Tool, content, timestamp);
            }
        }
        _ => {
            if let Some(content) = field(value, "content") {
                append_content(builder, role, content, timestamp);
                return;
            }
            if let Some(content) = field(value, "text")
                .or_else(|| field(value, "thinking"))
                .or_else(|| field(value, "reasoning"))
            {
                append_content(
                    builder,
                    if field(value, "thinking").is_some() || field(value, "reasoning").is_some() {
                        TranscriptRole::Thinking
                    } else {
                        role
                    },
                    content,
                    timestamp,
                );
                return;
            }

            // Tagged enums used by some local agents look like {"Text": "..."}
            // or {"Thinking": "..."} rather than carrying a `type` field.
            if let Some(object) = value.as_object()
                && object.len() == 1
                && let Some((tag, content)) = object.iter().next()
            {
                match tag.to_ascii_lowercase().as_str() {
                    "text" | "markdown" => append_content(builder, role, content, timestamp),
                    "thinking" | "reasoning" => {
                        append_content(builder, TranscriptRole::Thinking, content, timestamp)
                    }
                    _ => {}
                }
            }
        }
    }
}

fn tool_call_text(value: &Value) -> String {
    let name = string_field(value, "name")
        .or_else(|| string_field(value, "tool_name"))
        .unwrap_or_else(|| "tool".to_string());
    let arguments = field(value, "input")
        .or_else(|| field(value, "arguments"))
        .or_else(|| field(value, "parameters"));
    match arguments {
        Some(Value::String(arguments)) if !arguments.trim().is_empty() => {
            format!("{name}\n{arguments}")
        }
        Some(arguments) => serde_json::to_string_pretty(arguments)
            .map(|arguments| format!("{name}\n{arguments}"))
            .unwrap_or(name),
        None => name,
    }
}

fn parse_role(value: String) -> Option<TranscriptRole> {
    let value = value.to_ascii_lowercase().replace(['-', ' '], "_");
    if value == "user"
        || value == "human"
        || value.ends_with("_user")
        || value.ends_with("_user_message")
    {
        return Some(TranscriptRole::User);
    }
    if value == "assistant"
        || value == "agent"
        || value == "model"
        || value.ends_with("_assistant")
        || value.ends_with("_assistant_message")
        || value == "agent_message"
    {
        return Some(TranscriptRole::Assistant);
    }
    if value == "thinking"
        || value == "reasoning"
        || value.contains("thinking")
        || value.contains("reasoning")
    {
        return Some(TranscriptRole::Thinking);
    }
    if value == "tool"
        || value == "tool_use"
        || value == "tool_call"
        || value == "tool_result"
        || value.contains("tool")
    {
        return Some(TranscriptRole::Tool);
    }
    None
}

fn record_matches_session(value: &Value, expected: Option<&str>) -> bool {
    let Some(expected) = expected.filter(|value| !value.trim().is_empty()) else {
        return true;
    };
    let scopes = [
        Some(value),
        field(value, "payload"),
        field(value, "message"),
        field(value, "data"),
    ];
    let names = [
        "session_id",
        "sessionId",
        "sessionID",
        "session",
        "conversation_id",
        "conversationId",
        "thread_id",
        "threadId",
    ];
    for scope in scopes.into_iter().flatten() {
        for name in names {
            if let Some(actual) = string_field(scope, name) {
                return actual == expected;
            }
        }
    }
    // A session-specific JSONL source generally omits the ID on individual
    // records, so absence of an ID is not evidence that the record is foreign.
    true
}

fn record_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    let scopes = [
        Some(value),
        field(value, "payload"),
        field(value, "message"),
    ];
    for scope in scopes.into_iter().flatten() {
        for name in [
            "timestamp",
            "created_at",
            "createdAt",
            "time_created",
            "timeCreated",
            "time",
        ] {
            if let Some(value) = field(scope, name)
                && let Some(timestamp) = parse_timestamp_value(value)
            {
                return Some(timestamp);
            }
        }
    }
    None
}

fn parse_timestamp_value(value: &Value) -> Option<DateTime<Utc>> {
    if let Some(number) = value.as_i64() {
        return if number > 10_000_000_000 {
            DateTime::<Utc>::from_timestamp_millis(number)
        } else {
            DateTime::<Utc>::from_timestamp(number, 0)
        };
    }
    if let Some(number) = value.as_f64() {
        let millis = (number * 1_000.0).round() as i64;
        return if number > 10_000_000_000.0 {
            DateTime::<Utc>::from_timestamp_millis(number.round() as i64)
        } else {
            DateTime::<Utc>::from_timestamp_millis(millis)
        };
    }
    let text = value.as_str()?.trim();
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|value| value.with_timezone(&Utc))
        .or_else(|| {
            text.parse::<i64>().ok().and_then(|number| {
                if number > 10_000_000_000 {
                    DateTime::<Utc>::from_timestamp_millis(number)
                } else {
                    DateTime::<Utc>::from_timestamp(number, 0)
                }
            })
        })
        .or_else(|| {
            text.parse::<f64>().ok().and_then(|number| {
                let millis = (number * 1_000.0).round() as i64;
                if number > 10_000_000_000.0 {
                    DateTime::<Utc>::from_timestamp_millis(number.round() as i64)
                } else {
                    DateTime::<Utc>::from_timestamp_millis(millis)
                }
            })
        })
}

fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    let object = value.as_object()?;
    object.get(name).or_else(|| {
        object
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    })
}

fn string_field(value: &Value, name: &str) -> Option<String> {
    field(value, name)
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn looks_like_sqlite(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut header = [0; 16];
    file.read_exact(&mut header).is_ok() && header == *b"SQLite format 3\0"
}

#[derive(Clone, Debug, Default)]
struct TableSpec {
    table: String,
    session: Option<String>,
    id: Option<String>,
    parent_id: Option<String>,
    data: Option<String>,
    role: Option<String>,
    content: Option<String>,
    reasoning: Option<String>,
    reasoning_content: Option<String>,
    reasoning_details: Option<String>,
    codex_reasoning_items: Option<String>,
    codex_message_items: Option<String>,
    tool_calls: Option<String>,
    tool_name: Option<String>,
    timestamp: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct RawRow {
    id: Option<String>,
    parent_id: Option<String>,
    data: Option<String>,
    role: Option<String>,
    content: Option<String>,
    reasoning: Option<String>,
    reasoning_content: Option<String>,
    reasoning_details: Option<String>,
    codex_reasoning_items: Option<String>,
    codex_message_items: Option<String>,
    tool_calls: Option<String>,
    tool_name: Option<String>,
    timestamp: Option<String>,
}

fn load_opencode_sqlite(path: &Path, session: &SessionSummary) -> Result<SessionTranscript> {
    let session_id = session
        .session_id
        .as_deref()
        .context("OpenCode session has no session ID")?;
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open OpenCode database {}", path.display()))?;
    let mut builder = TranscriptBuilder::default();

    // OpenCode's current session_v2 schema stores the transcript in
    // session_message. Older installations use message + part instead.
    if let Some(spec) = find_table_spec(&connection, &["session_message"], true)? {
        for row in query_raw_rows(&connection, &spec, session_id)? {
            let role = infer_raw_role(&row, None);
            parse_raw_row(&row, role, &mut builder);
            if builder.messages.len() >= MAX_MESSAGES {
                builder.truncated = true;
                break;
            }
        }
        if !builder.messages.is_empty() {
            return Ok(builder.finish());
        }
    }

    let message_spec = find_table_spec(&connection, &["message", "messages"], true)?;
    let part_spec = find_table_spec(&connection, &["part", "parts"], true)?;
    let mut roles = HashMap::<String, TranscriptRole>::new();

    if let Some(spec) = message_spec {
        for row in query_raw_rows(&connection, &spec, session_id)? {
            let role = infer_raw_role(&row, None);
            if let (Some(id), Some(role)) = (row.id.clone(), role) {
                roles.insert(id, role);
            }
            parse_raw_row(&row, role, &mut builder);
        }
    }
    if let Some(spec) = part_spec {
        for row in query_raw_rows(&connection, &spec, session_id)? {
            let role = row
                .parent_id
                .as_deref()
                .and_then(|id| roles.get(id).copied())
                .or_else(|| infer_raw_role(&row, None))
                .or(Some(TranscriptRole::Assistant));
            parse_raw_row(&row, role, &mut builder);
            if builder.messages.len() >= MAX_MESSAGES {
                builder.truncated = true;
                break;
            }
        }
    }

    Ok(builder.finish())
}

fn load_qoder_sqlite(path: &Path, session: &SessionSummary) -> Result<SessionTranscript> {
    let session_id = session
        .session_id
        .as_deref()
        .context("Qoder session has no session ID")?;
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open Qoder database {}", path.display()))?;
    let Some(spec) = find_table_spec(&connection, &["chat_message", "message"], true)? else {
        return Ok(SessionTranscript::default());
    };
    let mut builder = TranscriptBuilder::default();
    for row in query_raw_rows(&connection, &spec, session_id)? {
        let role = infer_raw_role(&row, Some(TranscriptRole::Assistant));
        parse_raw_row(&row, role, &mut builder);
        if builder.messages.len() >= MAX_MESSAGES {
            builder.truncated = true;
            break;
        }
    }
    Ok(builder.finish())
}

fn load_hermes_sqlite(path: &Path, session: &SessionSummary) -> Result<SessionTranscript> {
    let session_id = session
        .session_id
        .as_deref()
        .context("Hermes session has no session ID")?;
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open Hermes database {}", path.display()))?;
    let mut builder = TranscriptBuilder::default();

    if let Some(spec) = find_table_spec(&connection, &["messages", "message"], true)? {
        for row in query_raw_rows(&connection, &spec, session_id)? {
            let role = infer_raw_role(&row, None);
            parse_raw_row(&row, role, &mut builder);
            if builder.messages.len() >= MAX_MESSAGES {
                builder.truncated = true;
                break;
            }
        }
        if !builder.messages.is_empty() {
            return Ok(builder.finish());
        }
    }

    // Older Hermes databases may only retain the task on the session record.
    if let Some(spec) = find_task_columns(&connection)? {
        let sql = format!(
            "SELECT {}, {}, {} FROM {} WHERE {} = ?1 LIMIT 1",
            quote_identifier(&spec.id),
            quote_identifier(&spec.task),
            spec.timestamp
                .as_deref()
                .map(quote_identifier)
                .unwrap_or_else(|| "NULL".to_string()),
            quote_identifier(&spec.table),
            quote_identifier(&spec.id),
        );
        if let Ok((Some(task), timestamp)) = connection.query_row(&sql, [session_id], |row| {
            Ok((
                row_text(row, 1)?,
                row_text(row, 2)?.and_then(|value| parse_timestamp_value(&Value::String(value))),
            ))
        }) {
            builder.push(TranscriptRole::User, task, timestamp);
        }
    }
    Ok(builder.finish())
}

fn load_zed_sqlite(path: &Path, session: &SessionSummary) -> Result<SessionTranscript> {
    let session_id = session
        .session_id
        .as_deref()
        .context("Zed session has no thread ID")?;
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open Zed threads database {}", path.display()))?;
    let (data_type, data): (String, Vec<u8>) = connection
        .query_row(
            "SELECT data_type, data FROM threads WHERE id = ?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .with_context(|| format!("read Zed thread {session_id}"))?;
    let data = match data_type.as_str() {
        "json" => data,
        "zstd" => zstd::decode_all(data.as_slice())
            .with_context(|| format!("decompress Zed thread {session_id}"))?,
        other => bail!("unsupported Zed thread data type: {other}"),
    };
    let thread: Value =
        serde_json::from_slice(&data).with_context(|| format!("decode Zed thread {session_id}"))?;
    let mut builder = TranscriptBuilder::default();
    let Some(messages) = field(&thread, "messages").and_then(Value::as_array) else {
        return Ok(builder.finish());
    };

    for message in messages {
        let timestamp = record_timestamp(message);
        if let Some(object) = message.as_object() {
            for (tag, payload) in object {
                let Some(role) = parse_role(tag.clone()) else {
                    continue;
                };
                let content = field(payload, "content")
                    .or_else(|| field(payload, "text"))
                    .or_else(|| field(payload, "message"))
                    .unwrap_or(payload);
                append_content(&mut builder, role, content, timestamp);
            }
        } else {
            parse_generic_record(message, None, &mut builder);
        }
        if builder.messages.len() >= MAX_MESSAGES {
            builder.truncated = true;
            break;
        }
    }
    Ok(builder.finish())
}

fn record_role(value: &Value, fallback: Option<TranscriptRole>) -> Option<TranscriptRole> {
    if let Some(message) = field(value, "message")
        && let Some(role) = string_field(message, "role").and_then(parse_role)
    {
        return Some(role);
    }
    string_field(value, "role")
        .and_then(parse_role)
        .or_else(|| string_field(value, "type").and_then(parse_role))
        .or(fallback)
}

fn infer_raw_role(row: &RawRow, fallback: Option<TranscriptRole>) -> Option<TranscriptRole> {
    row.role
        .clone()
        .and_then(parse_role)
        .or_else(|| {
            row.data
                .as_deref()
                .and_then(|data| serde_json::from_str::<Value>(data).ok())
                .and_then(|value| record_role(&value, None))
        })
        .or_else(|| {
            row.data
                .as_deref()
                .and_then(|data| serde_json::from_str::<Value>(data).ok())
                .and_then(|value| {
                    if field(&value, "content").is_some() {
                        Some(TranscriptRole::Assistant)
                    } else if field(&value, "text").is_some() {
                        Some(TranscriptRole::User)
                    } else {
                        None
                    }
                })
        })
        .or(fallback)
}

fn parse_raw_row(row: &RawRow, fallback: Option<TranscriptRole>, builder: &mut TranscriptBuilder) {
    let timestamp = row
        .timestamp
        .as_deref()
        .and_then(|value| parse_timestamp_value(&Value::String(value.to_string())));
    let before = builder.messages.len();
    if let Some(data) = row.data.as_deref()
        && let Ok(value) = serde_json::from_str::<Value>(data)
    {
        parse_generic_record(&value, fallback, builder);
    }
    if builder.messages.len() == before
        && let Some(content) = row.content.as_deref()
        && let Some(role) = fallback
    {
        if let Ok(value) = serde_json::from_str::<Value>(content) {
            append_content(builder, role, &value, timestamp);
        } else {
            builder.push(role, content, timestamp);
        }
    }
    if let Some(role) = fallback {
        for (value, thinking) in [
            (row.reasoning.as_deref(), true),
            (row.reasoning_content.as_deref(), true),
            (row.reasoning_details.as_deref(), true),
            (row.codex_reasoning_items.as_deref(), true),
            (row.codex_message_items.as_deref(), false),
        ] {
            let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
                continue;
            };
            let role = if thinking {
                TranscriptRole::Thinking
            } else {
                role
            };
            if let Ok(json) = serde_json::from_str::<Value>(value) {
                append_content(builder, role, &json, timestamp);
            } else {
                builder.push(role, value, timestamp);
            }
        }
        if let Some(tool_calls) = row.tool_calls.as_deref()
            && let Ok(value) = serde_json::from_str::<Value>(tool_calls)
        {
            append_tool_calls(builder, &value, timestamp);
        }
        if let Some(tool_name) = row.tool_name.as_deref()
            && !tool_name.trim().is_empty()
        {
            builder.push(TranscriptRole::Tool, tool_name, timestamp);
        }
    }
}

fn append_tool_calls(
    builder: &mut TranscriptBuilder,
    value: &Value,
    timestamp: Option<DateTime<Utc>>,
) {
    match value {
        Value::Array(values) => {
            for value in values {
                append_tool_calls(builder, value, timestamp);
            }
        }
        Value::Object(_) => builder.push(TranscriptRole::Tool, tool_call_text(value), timestamp),
        Value::String(value) => builder.push(TranscriptRole::Tool, value, timestamp),
        _ => {}
    }
}

struct TaskSpec {
    table: String,
    id: String,
    task: String,
    timestamp: Option<String>,
}

fn find_task_columns(connection: &Connection) -> Result<Option<TaskSpec>> {
    for table in table_names(connection)? {
        let columns = table_columns(connection, &table)?;
        let Some(id) = find_column(&columns, &["id", "session_id", "sessionId"]) else {
            continue;
        };
        let Some(task) = find_column(&columns, &["task", "prompt", "user_prompt"]) else {
            continue;
        };
        let timestamp = find_column(&columns, &["ended_at", "started_at", "time_created"]);
        return Ok(Some(TaskSpec {
            table,
            id,
            task,
            timestamp,
        }));
    }
    Ok(None)
}

fn find_table_spec(
    connection: &Connection,
    preferred_names: &[&str],
    require_session: bool,
) -> Result<Option<TableSpec>> {
    let tables = table_names(connection)?;
    for table in tables {
        if !preferred_names
            .iter()
            .any(|name| table.eq_ignore_ascii_case(name))
        {
            continue;
        }
        let columns = table_columns(connection, &table)?;
        let session = find_column(&columns, &["session_id", "sessionId", "sessionID"]);
        if require_session && session.is_none() {
            continue;
        }
        let data = find_column(&columns, &["data", "json", "payload"]);
        let content = find_column(&columns, &["content", "text", "message", "output", "body"]);
        let reasoning = find_column(&columns, &["reasoning"]);
        let reasoning_content = find_column(&columns, &["reasoning_content"]);
        let reasoning_details = find_column(&columns, &["reasoning_details"]);
        let codex_reasoning_items = find_column(&columns, &["codex_reasoning_items"]);
        let codex_message_items = find_column(&columns, &["codex_message_items"]);
        let tool_calls = find_column(&columns, &["tool_calls"]);
        let tool_name = find_column(&columns, &["tool_name"]);
        if data.is_none()
            && content.is_none()
            && reasoning.is_none()
            && reasoning_content.is_none()
            && reasoning_details.is_none()
            && codex_reasoning_items.is_none()
            && codex_message_items.is_none()
            && tool_calls.is_none()
            && tool_name.is_none()
        {
            continue;
        }
        return Ok(Some(TableSpec {
            table,
            session,
            id: find_column(&columns, &["id", "message_id", "messageId"]),
            parent_id: find_column(&columns, &["message_id", "messageId"]),
            data,
            role: find_column(&columns, &["role", "type", "sender"]),
            content,
            reasoning,
            reasoning_content,
            reasoning_details,
            codex_reasoning_items,
            codex_message_items,
            tool_calls,
            tool_name,
            timestamp: find_column(
                &columns,
                &["timestamp", "created_at", "createdAt", "time_created"],
            ),
        }));
    }
    Ok(None)
}

fn query_raw_rows(
    connection: &Connection,
    spec: &TableSpec,
    session_id: &str,
) -> Result<Vec<RawRow>> {
    let session = spec
        .session
        .as_deref()
        .context("transcript table has no session column")?;
    let expression = |column: Option<&String>, alias: &str| {
        column
            .map(|column| {
                format!(
                    "{} AS {}",
                    quote_identifier(column),
                    quote_identifier(alias)
                )
            })
            .unwrap_or_else(|| format!("NULL AS {}", quote_identifier(alias)))
    };
    let sql = format!(
        "SELECT {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {} FROM {} WHERE {} = ?1 ORDER BY rowid LIMIT {}",
        expression(spec.id.as_ref(), "__id"),
        expression(spec.parent_id.as_ref(), "__parent_id"),
        expression(spec.data.as_ref(), "__data"),
        expression(spec.role.as_ref(), "__role"),
        expression(spec.content.as_ref(), "__content"),
        expression(spec.reasoning.as_ref(), "__reasoning"),
        expression(spec.reasoning_content.as_ref(), "__reasoning_content"),
        expression(spec.reasoning_details.as_ref(), "__reasoning_details"),
        expression(
            spec.codex_reasoning_items.as_ref(),
            "__codex_reasoning_items"
        ),
        expression(spec.codex_message_items.as_ref(), "__codex_message_items"),
        expression(spec.tool_calls.as_ref(), "__tool_calls"),
        expression(spec.tool_name.as_ref(), "__tool_name"),
        expression(spec.timestamp.as_ref(), "__timestamp"),
        quote_identifier(&spec.table),
        quote_identifier(session),
        MAX_MESSAGES * 2,
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map([session_id], |row| {
        Ok(RawRow {
            id: row_text(row, 0)?,
            parent_id: row_text(row, 1)?,
            data: row_text(row, 2)?,
            role: row_text(row, 3)?,
            content: row_text(row, 4)?,
            reasoning: row_text(row, 5)?,
            reasoning_content: row_text(row, 6)?,
            reasoning_details: row_text(row, 7)?,
            codex_reasoning_items: row_text(row, 8)?,
            codex_message_items: row_text(row, 9)?,
            tool_calls: row_text(row, 10)?,
            tool_name: row_text(row, 11)?,
            timestamp: row_text(row, 12)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn table_names(connection: &Connection) -> Result<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let rows = statement.query_map([], |row| row.get(0))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn table_columns(connection: &Connection, table: &str) -> Result<Vec<String>> {
    let sql = format!("PRAGMA table_info({})", quote_identifier(table));
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map([], |row| row.get(1))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn find_column(columns: &[String], candidates: &[&str]) -> Option<String> {
    candidates.iter().find_map(|candidate| {
        columns
            .iter()
            .find(|column| column.eq_ignore_ascii_case(candidate))
            .cloned()
    })
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn row_text(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<String>> {
    match row.get_ref(index)? {
        ValueRef::Null => Ok(None),
        ValueRef::Text(value) | ValueRef::Blob(value) => {
            Ok(Some(String::from_utf8_lossy(value).into_owned()))
        }
        ValueRef::Integer(value) => Ok(Some(value.to_string())),
        ValueRef::Real(value) => Ok(Some(value.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use rusqlite::Connection;

    use super::*;

    fn session(provider: Provider, path: PathBuf, id: &str) -> SessionSummary {
        let now = Utc::now();
        SessionSummary {
            provider,
            session_id: Some(id.into()),
            source_file: Some(path.to_string_lossy().into_owned()),
            project_name: None,
            project_path: None,
            model: None,
            started_at: now,
            ended_at: now,
            turn_count: 2,
            total_tokens: 1,
            estimated_cost_usd: None,
        }
    }

    #[test]
    fn reads_claude_questions_answers_and_thinking_without_persisting_them() {
        let path = std::env::temp_dir().join(format!(
            "llmeter-transcript-claude-{}.jsonl",
            std::process::id()
        ));
        fs::write(
            &path,
            r#"{"type":"user","sessionId":"s1","message":{"role":"user","content":"Fix the parser"}}
{"type":"assistant","sessionId":"s1","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Inspect the input first."},{"type":"text","text":"I found the issue."}]}}
{"type":"assistant","sessionId":"other","message":{"role":"assistant","content":"ignore me"}}
"#,
        )
        .unwrap();

        let transcript =
            load_session_transcript(&session(Provider::Claude, path.clone(), "s1")).unwrap();

        assert_eq!(transcript.messages.len(), 3);
        assert_eq!(transcript.messages[0].role, TranscriptRole::User);
        assert_eq!(transcript.messages[0].content, "Fix the parser");
        assert_eq!(transcript.messages[1].role, TranscriptRole::Thinking);
        assert_eq!(transcript.messages[2].role, TranscriptRole::Assistant);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn reads_codex_event_messages_and_reasoning() {
        let path = std::env::temp_dir().join(format!(
            "llmeter-transcript-codex-{}.jsonl",
            std::process::id()
        ));
        fs::write(
            &path,
            r#"{"type":"event_msg","payload":{"type":"user_message","message":"What changed?"}}
{"type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"Compare the two files."}]}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"The parser now preserves IDs."}]}}
"#,
        )
        .unwrap();

        let transcript =
            load_session_transcript(&session(Provider::Codex, path.clone(), "codex")).unwrap();

        assert_eq!(transcript.messages.len(), 3);
        assert_eq!(transcript.messages[0].role, TranscriptRole::User);
        assert_eq!(transcript.messages[1].role, TranscriptRole::Thinking);
        assert_eq!(transcript.messages[2].role, TranscriptRole::Assistant);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn reads_opencode_v2_session_messages() {
        let path = std::env::temp_dir().join(format!(
            "llmeter-transcript-opencode-{}.db",
            std::process::id()
        ));
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session_message (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    type TEXT NOT NULL,
                    seq INTEGER NOT NULL,
                    time_created INTEGER NOT NULL,
                    time_updated INTEGER NOT NULL,
                    data TEXT NOT NULL
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO session_message VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    "user-1",
                    "s1",
                    "user",
                    1,
                    1_700_000_000_000i64,
                    1_700_000_000_000i64,
                    r#"{"text":"Show the failing test","time":{"created":1700000000000}}"#
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO session_message VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    "assistant-1",
                    "s1",
                    "assistant",
                    2,
                    1_700_000_001_000i64,
                    1_700_000_001_000i64,
                    r#"{"content":[{"type":"reasoning","text":"Read the assertion."},{"type":"text","text":"The test needs a fixture."}]}"#
                ],
            )
            .unwrap();
        drop(connection);

        let transcript =
            load_session_transcript(&session(Provider::OpenCode, path.clone(), "s1")).unwrap();

        assert_eq!(transcript.messages.len(), 3);
        assert_eq!(transcript.messages[0].role, TranscriptRole::User);
        assert_eq!(transcript.messages[1].role, TranscriptRole::Thinking);
        assert_eq!(transcript.messages[2].role, TranscriptRole::Assistant);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn reads_hermes_messages_and_reasoning_from_the_local_database() {
        let path = std::env::temp_dir().join(format!(
            "llmeter-transcript-hermes-{}.db",
            std::process::id()
        ));
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE messages (
                    id INTEGER PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    role TEXT NOT NULL,
                    content TEXT,
                    reasoning TEXT,
                    timestamp REAL NOT NULL
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO messages VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    1,
                    "s1",
                    "user",
                    "Explain this error",
                    Option::<String>::None,
                    1_700_000_000f64
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO messages VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    2,
                    "s1",
                    "assistant",
                    "The error comes from parsing.",
                    "Inspect the input shape first.",
                    1_700_000_001f64
                ],
            )
            .unwrap();
        drop(connection);

        let transcript =
            load_session_transcript(&session(Provider::Hermes, path.clone(), "s1")).unwrap();

        assert_eq!(transcript.messages.len(), 3);
        assert_eq!(transcript.messages[0].role, TranscriptRole::User);
        assert_eq!(transcript.messages[1].role, TranscriptRole::Assistant);
        assert_eq!(transcript.messages[2].role, TranscriptRole::Thinking);
        let _ = fs::remove_file(path);
    }
}
