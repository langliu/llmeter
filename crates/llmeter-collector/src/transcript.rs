use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Read},
    path::Path,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use llmeter_core::Provider;
use llmeter_storage::SessionSummary;
use rusqlite::{Connection, Row, types::ValueRef};
use serde_json::Value;

use crate::sqlite::{open_read_only, quote_identifier, table_columns};

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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TranscriptPhase {
    #[default]
    Unspecified,
    Progress,
    Final,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptMessage {
    pub role: TranscriptRole,
    pub phase: TranscriptPhase,
    pub content: String,
    pub timestamp: Option<DateTime<Utc>>,
    pub images: Vec<TranscriptImage>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptImage {
    pub source: String,
    pub mime: String,
    pub data: Option<Arc<[u8]>>,
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

    let mut transcript = match session.provider {
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
        Provider::ZCode => load_zcode_sqlite(path, session),
        Provider::Hermes => load_hermes_sqlite(path, session),
        Provider::Antigravity => load_antigravity_sqlite(path, session),
        Provider::Cursor | Provider::Trae => bail!(
            "{} only provides account usage data locally; its conversation content is not available",
            session.provider.display_name()
        ),
        Provider::Copilot | Provider::Cline | Provider::Roo | Provider::Kilo => bail!(
            "{} transcripts are not supported yet",
            session.provider.display_name()
        ),
    }?;
    resolve_transcript_images(&mut transcript, path);
    Ok(transcript)
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
            images: Vec::new(),
            phase: TranscriptPhase::Unspecified,
        });
    }

    fn push_image(
        &mut self,
        role: TranscriptRole,
        image: TranscriptImage,
        timestamp: Option<DateTime<Utc>>,
    ) {
        if let Some(last) = self.messages.last_mut().filter(|last| last.role == role) {
            last.images.push(image);
        } else if self.messages.len() < MAX_MESSAGES {
            self.messages.push(TranscriptMessage {
                role,
                content: String::new(),
                timestamp,
                images: vec![image],
                phase: TranscriptPhase::Unspecified,
            });
        } else {
            self.truncated = true;
        }
    }

    fn finish(self) -> SessionTranscript {
        SessionTranscript {
            messages: self.messages,
            truncated: self.truncated,
        }
    }
}

fn resolve_transcript_images(transcript: &mut SessionTranscript, source_path: &Path) {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let mut remaining = 64 * 1024 * 1024usize;
    for image in transcript
        .messages
        .iter_mut()
        .flat_map(|message| &mut message.images)
    {
        let source = if let Some(reference) = image.source.strip_prefix("zcode-artifact://") {
            let Some((session, artifact)) = reference.split_once('/') else {
                continue;
            };
            if session.contains(['/', '\\'])
                || artifact.contains(['/', '\\'])
                || session == ".."
                || artifact == ".."
            {
                continue;
            }
            let Some(root) = source_path.parent().and_then(Path::parent) else {
                continue;
            };
            let directory = root.join("artifacts").join(session);
            let Ok(entries) = std::fs::read_dir(directory) else {
                continue;
            };
            let Some(path) = entries.flatten().map(|entry| entry.path()).find(|path| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.ends_with(artifact))
            }) else {
                continue;
            };
            let Ok(metadata) = path.metadata() else {
                continue;
            };
            if metadata.len() > MAX_SOURCE_BYTES || metadata.len() as usize > remaining {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(path) else {
                continue;
            };
            source
        } else {
            image.source.clone()
        };
        if let Some(uri) = source.strip_prefix("data:") {
            let Some((header, payload)) = uri.split_once(',') else {
                continue;
            };
            let Some(mime) = header.strip_suffix(";base64") else {
                continue;
            };
            if !mime.starts_with("image/")
                || payload.len() > MAX_SOURCE_BYTES as usize
                || payload.len() > remaining
            {
                continue;
            }
            if let Ok(data) = STANDARD.decode(payload.trim()) {
                remaining -= data.len();
                image.mime = mime.to_string();
                image.data = Some(data.into());
            }
        } else {
            let path = Path::new(source.strip_prefix("file://").unwrap_or(&source));
            if !path.is_absolute() {
                continue;
            }
            let Ok(metadata) = path.metadata() else {
                continue;
            };
            if metadata.len() > MAX_SOURCE_BYTES || metadata.len() as usize > remaining {
                continue;
            }
            if let Ok(data) = std::fs::read(path) {
                remaining -= data.len();
                image.data = Some(data.into());
            }
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
        let start = builder.messages.len();
        parse_generic_record(value, None, builder, None);
        if matches!(provider, Provider::Pi | Provider::Omp) {
            for message in &mut builder.messages[start..] {
                if message.role == TranscriptRole::User {
                    message.content = pi_visible_request(&message.content);
                }
            }
        }
    }
}

// Pi file references are serialized as editor context markers inside user text.
// Keep the referenced path and surrounding request, removing only the wrapper.
fn pi_visible_request(text: &str) -> String {
    text.replace("[Context] file:///", "/").trim().to_string()
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
        } else if matches!(item_type.as_str(), "function_call" | "custom_tool_call") {
            let mut call = payload.clone();
            if let Some(input) = payload.get("input") {
                call["arguments"] = input.clone();
            }
            builder.push(TranscriptRole::Tool, tool_call_text(&call), timestamp);
        } else if matches!(
            item_type.as_str(),
            "function_call_output" | "custom_tool_call_output"
        ) {
            if let Some(output) = payload.get("output") {
                append_content(builder, TranscriptRole::Tool, output, timestamp);
            }
        } else if item_type == "message" {
            // Response items include hidden system/developer prompts. Only
            // explicit conversation roles belong in the visible transcript.
            let Some(role) = string_field(payload, "role").and_then(parse_role) else {
                return;
            };
            if let Some(content) = field(payload, "content") {
                if role == TranscriptRole::User {
                    append_codex_user(builder, payload, content, timestamp);
                } else {
                    let phase = match payload.get("phase").and_then(Value::as_str) {
                        Some("commentary") => TranscriptPhase::Progress,
                        Some("final_answer") => TranscriptPhase::Final,
                        _ => TranscriptPhase::Unspecified,
                    };
                    let mut parts = TranscriptBuilder::default();
                    append_content(&mut parts, role, content, timestamp);
                    let text = parts
                        .messages
                        .iter()
                        .map(|message| message.content.as_str())
                        .filter(|text| !text.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    let images = parts
                        .messages
                        .into_iter()
                        .flat_map(|message| message.images)
                        .collect::<Vec<_>>();
                    if text.is_empty() && images.is_empty() {
                        return;
                    }
                    builder.push(role, text, timestamp);
                    for image in images {
                        builder.push_image(role, image, timestamp);
                    }
                    if let Some(last) = builder.messages.last_mut().filter(|last| last.role == role)
                    {
                        last.phase = phase;
                    }
                }
            }
        }
        return;
    }

    parse_generic_record(value, None, builder, None);
}

/// Remove generated attachment/app envelopes while retaining the actual request.
fn codex_visible_request(text: &str) -> String {
    let text = if text.starts_with("# Files mentioned by the user:")
        || text.starts_with("# Applications mentioned by the user:")
    {
        text.split_once("## My request:")
            .map_or(text, |(_, request)| request.trim())
    } else {
        text
    };
    let mut result = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        result.push_str(&rest[..start]);
        rest = &rest[start..];
        // A link label must close before another opening bracket. Otherwise
        // preserve this bracket and keep looking, rather than consuming an
        // array, unmatched bracket, or outer wrapper as part of the label.
        let Some(label_end) = rest[1..].find(['[', ']']).map(|offset| offset + 1) else {
            break;
        };
        if !rest[label_end..].starts_with("](") {
            result.push('[');
            rest = &rest[1..];
            continue;
        }
        let target_start = label_end + 2;
        let Some(target_end) = rest[target_start..].find(')') else {
            break;
        };
        let target_end = target_start + target_end;
        let target = &rest[target_start..target_end];
        if target.starts_with("plugin://") || target.starts_with("app://") {
            result.push_str(&rest[1..label_end]);
        } else {
            result.push_str(&rest[..target_end + 1]);
        }
        rest = &rest[target_end + 1..];
    }
    result.push_str(rest);
    result
}

/// Recover the visible input as one message instead of one bubble per content item.
fn append_codex_user(
    builder: &mut TranscriptBuilder,
    payload: &Value,
    content: &Value,
    timestamp: Option<DateTime<Utc>>,
) {
    let kinds = payload
        .pointer("/internal_chat_message_metadata_passthrough/content_item_kinds")
        .and_then(Value::as_array);
    let mut input = TranscriptBuilder::default();
    if let Some(items) = content.as_array() {
        for (index, item) in items.iter().enumerate() {
            if kinds.is_some_and(|kinds| {
                !kinds
                    .get(index)
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind.starts_with("user."))
            }) {
                continue;
            }
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                let text = text.trim();
                if text == "</image>" || (text.starts_with("<image name=") && text.ends_with('>')) {
                    continue;
                }
                let text = codex_visible_request(text);
                input.push(TranscriptRole::User, text, timestamp);
            } else {
                append_content(&mut input, TranscriptRole::User, item, timestamp);
            }
        }
    } else {
        append_content(&mut input, TranscriptRole::User, content, timestamp);
    }
    let text = input
        .messages
        .iter()
        .filter(|message| !message.content.is_empty())
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let images = input
        .messages
        .into_iter()
        .flat_map(|message| message.images)
        .collect::<Vec<_>>();
    if text.is_empty() && images.is_empty() {
        return;
    }
    if builder.messages.len() >= MAX_MESSAGES {
        builder.truncated = true;
        return;
    }
    if let Some(last) = builder
        .messages
        .last_mut()
        .filter(|last| last.role == TranscriptRole::User && last.content == text)
    {
        if last.images.is_empty() {
            last.images = images;
        }
        return;
    }
    builder.messages.push(TranscriptMessage {
        role: TranscriptRole::User,
        content: text,
        timestamp,
        images,
        phase: TranscriptPhase::Unspecified,
    });
}

/// Runtime-injected user-role records are work context, not human turn boundaries.
fn transcript_content_role(role: TranscriptRole, value: &Value) -> TranscriptRole {
    let synthetic = value.get("synthetic").and_then(Value::as_bool) == Some(true);
    let model_only = value.get("visibility").and_then(Value::as_str) == Some("model-only")
        || value
            .pointer("/metadata/visibility")
            .and_then(Value::as_str)
            == Some("model-only");
    if role == TranscriptRole::User && (synthetic || model_only) {
        TranscriptRole::Tool
    } else {
        role
    }
}

fn parse_generic_record(
    value: &Value,
    fallback_role: Option<TranscriptRole>,
    builder: &mut TranscriptBuilder,
    row_timestamp: Option<DateTime<Utc>>,
) {
    // SQLite rows often carry the creation time in a column while the payload
    // JSON has none; the row timestamp backstops the record's own field.
    let timestamp = record_timestamp(value).or(row_timestamp);
    if matches!(
        value.get("type").and_then(Value::as_str),
        Some("file" | "image" | "input_image" | "image_url")
    ) {
        append_content_object(
            builder,
            fallback_role.unwrap_or(TranscriptRole::User),
            value,
            timestamp,
        );
        return;
    }

    if let Some(message) = field(value, "message")
        && let Some(message_object) = message.as_object()
        && let Some(role) = string_field(message, "role").and_then(parse_role)
    {
        let content = field(message, "content")
            .or_else(|| field(message, "text"))
            .or_else(|| field(message, "output_text"));
        if let Some(content) = content {
            append_content(
                builder,
                transcript_content_role(role, message),
                content,
                timestamp,
            );
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
        append_content(
            builder,
            transcript_content_role(role, value),
            content,
            timestamp,
        );
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
    let role = transcript_content_role(role, value);
    let mime = string_field(value, "mime")
        .or_else(|| string_field(value, "media_type"))
        .or_else(|| {
            value
                .pointer("/source/media_type")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if matches!(kind, "image" | "input_image" | "image_url")
        || (kind == "file"
            && mime
                .as_deref()
                .is_some_and(|mime| mime.starts_with("image/")))
    {
        let source = string_field(value, "url")
            .or_else(|| string_field(value, "image_url"))
            .or_else(|| {
                value
                    .pointer("/image_url/url")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .or_else(|| {
                value
                    .pointer("/source/url")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .or_else(|| string_field(value, "path"))
            .or_else(|| {
                value
                    .pointer("/source/data")
                    .and_then(Value::as_str)
                    .map(|data| {
                        format!(
                            "data:{};base64,{data}",
                            mime.as_deref().unwrap_or("image/png")
                        )
                    })
            });
        builder.push_image(
            role,
            TranscriptImage {
                source: source.unwrap_or_default(),
                mime: mime.unwrap_or_else(|| "image/png".into()),
                data: None,
            },
            timestamp,
        );
        return;
    }
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
    parse_timestamp_text(value.as_str()?)
}

fn parse_timestamp_text(text: &str) -> Option<DateTime<Utc>> {
    let text = text.trim();
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
    let connection = open_read_only(path)
        .with_context(|| format!("open OpenCode database {}", path.display()))?;
    let mut builder = TranscriptBuilder::default();

    // OpenCode's current session_v2 schema stores the transcript in
    // session_message. Older installations use message + part instead.
    if let Some(spec) = find_table_spec(&connection, &["session_message"], true)? {
        for row in query_raw_rows(&connection, &spec, session_id)? {
            let data = parse_row_data(&row);
            let role = infer_raw_role(&row, data.as_ref(), None);
            parse_raw_row(&row, data.as_ref(), role, &mut builder);
            if builder.messages.len() >= MAX_MESSAGES {
                builder.truncated = true;
                break;
            }
        }
        if !builder.messages.is_empty() {
            return Ok(builder.finish());
        }
    }

    load_message_part_sqlite(
        &connection,
        session_id,
        &["message", "messages"],
        &["part", "parts"],
    )
}

/// Message + part transcript layout shared by OpenCode and ZCode: parent
/// messages carry the role, parts carry the content keyed by `message_id`.
fn load_message_part_sqlite(
    connection: &Connection,
    session_id: &str,
    message_tables: &[&str],
    part_tables: &[&str],
) -> Result<SessionTranscript> {
    let mut builder = TranscriptBuilder::default();
    let message_spec = find_table_spec(connection, message_tables, true)?;
    let part_spec = find_table_spec(connection, part_tables, true)?;
    let mut roles = HashMap::<String, TranscriptRole>::new();

    if let Some(spec) = message_spec {
        for row in query_raw_rows(connection, &spec, session_id)? {
            let data = parse_row_data(&row);
            let role = infer_raw_role(&row, data.as_ref(), None);
            if let (Some(id), Some(role)) = (row.id.clone(), role) {
                roles.insert(id, role);
            }
            parse_raw_row(&row, data.as_ref(), role, &mut builder);
        }
    }
    if let Some(spec) = part_spec {
        for row in query_raw_rows(connection, &spec, session_id)? {
            let data = parse_row_data(&row);
            let role = row
                .parent_id
                .as_deref()
                .and_then(|id| roles.get(id).copied())
                .or_else(|| infer_raw_role(&row, data.as_ref(), None))
                .or(Some(TranscriptRole::Assistant));
            parse_raw_row(&row, data.as_ref(), role, &mut builder);
            if builder.messages.len() >= MAX_MESSAGES {
                builder.truncated = true;
                break;
            }
        }
    }

    Ok(builder.finish())
}

fn load_zcode_sqlite(path: &Path, session: &SessionSummary) -> Result<SessionTranscript> {
    let session_id = session
        .session_id
        .as_deref()
        .context("ZCode session has no session ID")?;
    let connection =
        open_read_only(path).with_context(|| format!("open ZCode database {}", path.display()))?;
    load_message_part_sqlite(&connection, session_id, &["message"], &["part"])
}

fn load_qoder_sqlite(path: &Path, session: &SessionSummary) -> Result<SessionTranscript> {
    let session_id = session
        .session_id
        .as_deref()
        .context("Qoder session has no session ID")?;
    let connection =
        open_read_only(path).with_context(|| format!("open Qoder database {}", path.display()))?;
    let Some(spec) = find_table_spec(&connection, &["chat_message", "message"], true)? else {
        return Ok(SessionTranscript::default());
    };
    let mut builder = TranscriptBuilder::default();
    for row in query_raw_rows(&connection, &spec, session_id)? {
        let data = parse_row_data(&row);
        let role = infer_raw_role(&row, data.as_ref(), Some(TranscriptRole::Assistant));
        parse_raw_row(&row, data.as_ref(), role, &mut builder);
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
    let connection =
        open_read_only(path).with_context(|| format!("open Hermes database {}", path.display()))?;
    let mut builder = TranscriptBuilder::default();

    if let Some(spec) = find_table_spec(&connection, &["messages", "message"], true)? {
        for row in query_raw_rows(&connection, &spec, session_id)? {
            let data = parse_row_data(&row);
            let role = infer_raw_role(&row, data.as_ref(), None);
            parse_raw_row(&row, data.as_ref(), role, &mut builder);
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
    let connection = open_read_only(path)
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
            parse_generic_record(message, None, &mut builder, None);
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

/// Parses `row.data` once per row; the value is shared by role inference and
/// record parsing instead of being re-parsed for each step.
fn parse_row_data(row: &RawRow) -> Option<Value> {
    row.data
        .as_deref()
        .and_then(|data| serde_json::from_str::<Value>(data).ok())
}

fn infer_raw_role(
    row: &RawRow,
    data: Option<&Value>,
    fallback: Option<TranscriptRole>,
) -> Option<TranscriptRole> {
    row.role
        .clone()
        .and_then(parse_role)
        .or_else(|| data.and_then(|value| record_role(value, None)))
        .or_else(|| {
            data.and_then(|value| {
                if field(value, "content").is_some() {
                    Some(TranscriptRole::Assistant)
                } else if field(value, "text").is_some() {
                    Some(TranscriptRole::User)
                } else {
                    None
                }
            })
        })
        .or(fallback)
        .map(|role| data.map_or(role, |value| transcript_content_role(role, value)))
}

fn parse_raw_row(
    row: &RawRow,
    data: Option<&Value>,
    fallback: Option<TranscriptRole>,
    builder: &mut TranscriptBuilder,
) {
    let timestamp = row.timestamp.as_deref().and_then(parse_timestamp_text);
    let before = builder.messages.len();
    if let Some(value) = data {
        parse_generic_record(value, fallback, builder, timestamp);
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

fn find_column(columns: &[String], candidates: &[&str]) -> Option<String> {
    candidates.iter().find_map(|candidate| {
        columns
            .iter()
            .find(|column| column.eq_ignore_ascii_case(candidate))
            .cloned()
    })
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

fn load_antigravity_sqlite(path: &Path, _session: &SessionSummary) -> Result<SessionTranscript> {
    use crate::providers::antigravity::{ProtoValue, parse_proto_fields};

    let connection = open_read_only(path)?;
    let mut stmt = connection
        .prepare("SELECT idx, step_type, step_payload, metadata FROM steps ORDER BY idx")?;

    let mut builder = TranscriptBuilder::default();
    let mut rows = stmt.query([])?;

    while let Some(row) = rows.next()? {
        let _idx: i64 = row.get(0)?;
        let _step_type: i64 = row.get(1)?;
        let payload: Vec<u8> = row.get(2)?;
        let metadata: Vec<u8> = row.get(3)?;

        let mut timestamp = None;
        let meta_fields = parse_proto_fields(&metadata);
        if let Some(f1_list) = meta_fields.get(&1)
            && let Some(ProtoValue::Bytes(time_bytes)) = f1_list.first()
        {
            let time_fields = parse_proto_fields(time_bytes);
            if let Some(f1_sec) = time_fields.get(&1)
                && let Some(ProtoValue::Varint(sec)) = f1_sec.first()
            {
                let nanos = time_fields
                    .get(&2)
                    .and_then(|vals| vals.first())
                    .map(|v| match v {
                        ProtoValue::Varint(n) => *n as u32,
                        _ => 0,
                    })
                    .unwrap_or(0);
                timestamp = DateTime::<Utc>::from_timestamp(*sec as i64, nanos);
            }
        }

        let p_fields = parse_proto_fields(&payload);

        // User message: field 19 -> field 2
        if let Some(f19_list) = p_fields.get(&19)
            && let Some(ProtoValue::Bytes(f19_bytes)) = f19_list.first()
        {
            let user_fields = parse_proto_fields(f19_bytes);
            if let Some(f2_list) = user_fields.get(&2)
                && let Some(ProtoValue::Bytes(text_bytes)) = f2_list.first()
                && let Ok(text) = std::str::from_utf8(text_bytes)
                && !text.trim().is_empty()
            {
                builder.push(TranscriptRole::User, text, timestamp);
            }
        }

        // Assistant message: field 20 -> field 1 (or 8)
        if let Some(f20_list) = p_fields.get(&20)
            && let Some(ProtoValue::Bytes(f20_bytes)) = f20_list.first()
        {
            let asst_fields = parse_proto_fields(f20_bytes);
            let text_bytes = asst_fields
                .get(&1)
                .and_then(|vals| vals.first())
                .or_else(|| asst_fields.get(&8).and_then(|vals| vals.first()));

            if let Some(ProtoValue::Bytes(tb)) = text_bytes
                && let Ok(text) = std::str::from_utf8(tb)
                && !text.trim().is_empty()
            {
                builder.push(TranscriptRole::Assistant, text, timestamp);
            }
        }
    }

    Ok(builder.finish())
}

#[cfg(test)]
mod tests {
    #[test]
    fn pi_file_context_wrappers_preserve_the_request_and_reference() {
        for provider in [Provider::Pi, Provider::Omp] {
            let mut builder = TranscriptBuilder::default();
            let value = serde_json::json!({"type":"message", "message":{
                "role":"user", "content":[{"type":"text",
                    "text":"\n[Context] file:///Users/test/transcript.rs 的更改的作用是什么？"}]}});
            parse_json_record(provider, &value, &mut builder);
            let transcript = builder.finish();
            assert_eq!(transcript.messages.len(), 1);
            assert_eq!(
                transcript.messages[0].content,
                "/Users/test/transcript.rs 的更改的作用是什么？"
            );
        }
        assert_eq!(
            pi_visible_request("将 \n[Context] file:///tmp/ui/ 改为公共组件"),
            "将 \n/tmp/ui/ 改为公共组件"
        );
        assert_eq!(
            pi_visible_request("普通 [Context] 文字和 file:///tmp/file"),
            "普通 [Context] 文字和 file:///tmp/file"
        );
        let mut builder = TranscriptBuilder::default();
        parse_json_record(
            Provider::Pi,
            &serde_json::json!({"message":{
            "role":"assistant", "content":"[Context] file:///tmp/example"}}),
            &mut builder,
        );
        assert_eq!(
            builder.finish().messages[0].content,
            "[Context] file:///tmp/example"
        );
    }

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
    fn codex_app_links_do_not_consume_preceding_brackets() {
        for (input, expected) in [
            (
                "数组 [1, 2]，然后使用 [Notion](app://notion)",
                "数组 [1, 2]，然后使用 Notion",
            ),
            ("[broken and [Notion](plugin://notion)", "[broken and Notion"),
            ("[[Notion](app://notion)]", "[Notion]"),
            (
                "[array] [docs](https://example.com) [App](app://example)",
                "[array] [docs](https://example.com) App",
            ),
            ("[App](app://incomplete", "[App](app://incomplete"),
        ] {
            assert_eq!(codex_visible_request(input), expected, "input: {input}");
        }
    }

    #[test]
    fn codex_app_envelopes_display_only_request_and_app_names() {
        let wrapped = "# Applications mentioned by the user:\n\n[@Notion](plugin://computer-use@app:notion.id)\n\n## My request:\n[@Notion](plugin://notion@openai-curated-remote) 帮我新建一篇文章，介绍一下 pi Agent。";
        assert_eq!(
            codex_visible_request(wrapped),
            "@Notion 帮我新建一篇文章，介绍一下 pi Agent。"
        );
        assert_eq!(
            codex_visible_request("Use [Notion](app://notion) and [docs](https://example.com)."),
            "Use Notion and [docs](https://example.com)."
        );
        assert_eq!(
            codex_visible_request("Please explain ## My request: and [broken"),
            "Please explain ## My request: and [broken"
        );
        assert_eq!(
            codex_visible_request(
                "# Applications mentioned by the user:\nquoted heading without envelope"
            ),
            "# Applications mentioned by the user:\nquoted heading without envelope"
        );
        let mut builder = TranscriptBuilder::default();
        parse_codex_record(
            &serde_json::json!({"type":"response_item","payload":{
                "type":"message","role":"user","content":[{"type":"input_text","text":wrapped}]
            }}),
            &mut builder,
        );
        assert_eq!(builder.messages.len(), 1);
        assert_eq!(
            builder.messages[0].content,
            "@Notion 帮我新建一篇文章，介绍一下 pi Agent。"
        );
    }

    #[test]
    fn codex_deduplicates_user_records_and_preserves_tools_and_complete_final() {
        let mut builder = TranscriptBuilder::default();
        parse_codex_record(
            &serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"hello"}}),
            &mut builder,
        );
        parse_codex_record(
            &serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}),
            &mut builder,
        );
        for kind in ["function_call", "custom_tool_call"] {
            parse_codex_record(
                &serde_json::json!({"type":"response_item","payload":{"type":kind,"name":"read","input":"file","arguments":"file"}}),
                &mut builder,
            );
            parse_codex_record(
                &serde_json::json!({"type":"response_item","payload":{"type":format!("{kind}_output"),"output":format!("{kind} result")}}),
                &mut builder,
            );
        }
        parse_codex_record(
            &serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"First part"},{"type":"output_text","text":"Second part"}]}}),
            &mut builder,
        );
        assert_eq!(
            builder
                .messages
                .iter()
                .filter(|m| m.role == TranscriptRole::User)
                .count(),
            1
        );
        assert_eq!(
            builder
                .messages
                .iter()
                .filter(|m| m.role == TranscriptRole::Tool)
                .count(),
            4
        );
        let final_reply = builder.messages.last().unwrap();
        assert_eq!(final_reply.phase, TranscriptPhase::Final);
        assert_eq!(final_reply.content, "First part\n\nSecond part");
    }

    #[test]
    fn codex_attachment_wrappers_merge_into_visible_input_and_progress_is_work() {
        let mut builder = TranscriptBuilder::default();
        parse_codex_record(
            &serde_json::json!({"type":"response_item", "payload":{
                "type":"message", "role":"user", "content":[
                    {"type":"input_text","text":"# Files mentioned by the user:\nmetadata\n## My request:\nFix the layout"},
                    {"type":"input_text","text":"<image name=[Image #1] path=\"/tmp/image.png\">"},
                    {"type":"input_image","image_url":"data:image/png;base64,aGVsbG8="},
                    {"type":"input_text","text":"</image>"}],
                "internal_chat_message_metadata_passthrough":{"content_item_kinds":["user.text","user.text","user.image","user.text"]}
            }}),
            &mut builder,
        );
        for phase in ["commentary", "final_answer"] {
            parse_codex_record(
                &serde_json::json!({"type":"response_item", "payload":{
                    "type":"message","role":"assistant","phase":phase,
                    "content":[{"type":"output_text","text":phase}]
                }}),
                &mut builder,
            );
        }
        let transcript = builder.finish();
        assert_eq!(transcript.messages.len(), 3);
        assert_eq!(transcript.messages[0].content, "Fix the layout");
        assert_eq!(transcript.messages[0].images.len(), 1);
        assert_eq!(transcript.messages[1].role, TranscriptRole::Assistant);
        assert_eq!(transcript.messages[1].phase, TranscriptPhase::Progress);
        assert_eq!(transcript.messages[2].role, TranscriptRole::Assistant);
    }

    #[test]
    fn codex_user_origin_hides_injected_context_and_preserves_real_input() {
        let mut builder = TranscriptBuilder::default();
        for (kind, text) in [
            ("agents_md.instructions", "project instructions"),
            ("environments.environment_context", "environment"),
            ("additional_content.codex_apps_open_page", "page metadata"),
            (
                "user.text",
                "# AGENTS.md instructions for a file I want you to edit",
            ),
        ] {
            parse_codex_record(
                &serde_json::json!({"type":"response_item", "payload":{
                    "type":"message", "role":"user", "content":[{"type":"input_text", "text":text}],
                    "internal_chat_message_metadata_passthrough":{"content_item_kinds":[kind]}
                }}),
                &mut builder,
            );
        }
        let transcript = builder.finish();
        assert_eq!(transcript.messages.len(), 1);
        assert_eq!(
            transcript.messages[0].content,
            "# AGENTS.md instructions for a file I want you to edit"
        );
    }

    #[test]
    fn codex_hidden_roles_are_not_conversation_messages() {
        let mut builder = TranscriptBuilder::default();
        for role in ["system", "developer", "unknown", "user", "assistant"] {
            parse_codex_record(
                &serde_json::json!({"type":"response_item", "payload":{
                    "type":"message", "role":role, "content":[{"type":"input_text", "text":role}]
                }}),
                &mut builder,
            );
        }
        let transcript = builder.finish();
        assert_eq!(
            transcript
                .messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>(),
            vec!["user", "assistant"]
        );
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
    fn zcode_image_attachment_resolves_artifact_and_stays_with_user_text() {
        let root = std::env::temp_dir().join(format!("llmeter-image-{}", uuid::Uuid::new_v4()));
        let artifacts = root.join("artifacts/s1");
        fs::create_dir_all(&artifacts).unwrap();
        fs::write(
            artifacts.join("prompt-upload-image1.txt"),
            "data:image/png;base64,aGVsbG8=",
        )
        .unwrap();
        let mut builder = TranscriptBuilder::default();
        builder.push(TranscriptRole::User, "Look at this image", None);
        parse_generic_record(
            &serde_json::json!({"type":"file", "mime":"image/jpeg",
            "url":"zcode-artifact://s1/image1"}),
            Some(TranscriptRole::User),
            &mut builder,
            None,
        );
        let mut transcript = builder.finish();
        resolve_transcript_images(&mut transcript, &root.join("db/db.sqlite"));
        assert_eq!(transcript.messages.len(), 1);
        assert_eq!(transcript.messages[0].content, "Look at this image");
        let image = &transcript.messages[0].images[0];
        assert_eq!(
            image.mime, "image/png",
            "artifact MIME overrides original metadata"
        );
        assert_eq!(image.data.as_deref(), Some(b"hello".as_slice()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn image_only_and_missing_attachments_remain_in_transcript() {
        let mut builder = TranscriptBuilder::default();
        append_content(
            &mut builder,
            TranscriptRole::User,
            &serde_json::json!([
                {"type":"image", "source":{"type":"base64", "media_type":"image/png", "data":"aGVsbG8="}},
                {"type":"input_image", "image_url":"/missing/image.png"}
            ]),
            None,
        );
        let mut transcript = builder.finish();
        resolve_transcript_images(&mut transcript, Path::new("/tmp/session.jsonl"));
        assert_eq!(transcript.messages.len(), 1);
        assert_eq!(transcript.messages[0].images.len(), 2);
        assert!(transcript.messages[0].images[0].data.is_some());
        assert!(transcript.messages[0].images[1].data.is_none());
    }

    #[test]
    fn zcode_runtime_reminders_do_not_create_user_turns() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE message (id TEXT, session_id TEXT, time_created INTEGER, data TEXT);
            CREATE TABLE part (id TEXT, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);").unwrap();
        let records = [
            (
                "user",
                serde_json::json!({"type":"text", "text":"开始优化"}),
                false,
            ),
            (
                "assistant",
                serde_json::json!({"type":"reasoning", "text":"inspect"}),
                false,
            ),
            (
                "assistant",
                serde_json::json!({"type":"text", "text":"progress"}),
                false,
            ),
            (
                "user",
                serde_json::json!({"type":"text", "text":"TodoWrite reminder", "synthetic":true,
                "metadata":{"source":"todo_reminder", "visibility":"model-only"}}),
                true,
            ),
            (
                "assistant",
                serde_json::json!({"type":"reasoning", "text":"continue"}),
                false,
            ),
            (
                "assistant",
                serde_json::json!({"type":"text", "text":"done"}),
                false,
            ),
        ];
        for (index, (role, part, synthetic)) in records.into_iter().enumerate() {
            let id = index.to_string();
            let parent = serde_json::json!({"role":role, "synthetic":synthetic});
            connection
                .execute(
                    "INSERT INTO message VALUES (?1,'s1',?2,?3)",
                    rusqlite::params![id, index as i64, parent.to_string()],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO part VALUES (?1,?1,'s1',?2,?3)",
                    rusqlite::params![id, index as i64, part.to_string()],
                )
                .unwrap();
        }
        let transcript =
            load_message_part_sqlite(&connection, "s1", &["message"], &["part"]).unwrap();
        let roles = transcript
            .messages
            .iter()
            .map(|message| message.role)
            .collect::<Vec<_>>();
        assert_eq!(
            roles,
            vec![
                TranscriptRole::User,
                TranscriptRole::Thinking,
                TranscriptRole::Assistant,
                TranscriptRole::Tool,
                TranscriptRole::Thinking,
                TranscriptRole::Assistant
            ]
        );
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

#[cfg(test)]
mod row_timestamp_tests {
    use super::*;

    #[test]
    fn row_timestamp_backstops_records_without_payload_time() {
        let mut builder = TranscriptBuilder::default();
        let row = RawRow {
            id: None,
            parent_id: None,
            data: Some(r#"{"role":"assistant","content":"done"}"#.into()),
            role: None,
            content: None,
            reasoning: None,
            reasoning_content: None,
            reasoning_details: None,
            codex_reasoning_items: None,
            codex_message_items: None,
            tool_calls: None,
            tool_name: None,
            timestamp: Some("1790773590614".into()),
        };
        parse_raw_row(
            &row,
            parse_row_data(&row).as_ref(),
            Some(TranscriptRole::Assistant),
            &mut builder,
        );
        let transcript = builder.finish();
        assert_eq!(transcript.messages.len(), 1);
        assert_eq!(
            transcript.messages[0].timestamp,
            DateTime::<Utc>::from_timestamp_millis(1_790_773_590_614),
        );
    }
}
