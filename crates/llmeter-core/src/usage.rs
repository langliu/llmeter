use std::{collections::HashMap, path::PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::{Provider, SourceMetadata};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TokenCounts {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
}

impl TokenCounts {
    pub fn normalize(mut self) -> Self {
        if self.total_tokens == 0 {
            // cached_input_tokens is normally a subset of input_tokens. Cache
            // creation is a separate category in Claude-style payloads.
            self.total_tokens = self
                .input_tokens
                .saturating_add(self.cache_creation_input_tokens)
                .saturating_add(self.output_tokens)
                .saturating_add(self.reasoning_tokens);
        }
        self
    }

    pub fn is_zero(self) -> bool {
        self.input_tokens == 0
            && self.cached_input_tokens == 0
            && self.cache_creation_input_tokens == 0
            && self.output_tokens == 0
            && self.reasoning_tokens == 0
            && self.total_tokens == 0
    }

    pub fn saturating_add(self, rhs: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(rhs.input_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_add(rhs.cached_input_tokens),
            cache_creation_input_tokens: self
                .cache_creation_input_tokens
                .saturating_add(rhs.cache_creation_input_tokens),
            output_tokens: self.output_tokens.saturating_add(rhs.output_tokens),
            reasoning_tokens: self.reasoning_tokens.saturating_add(rhs.reasoning_tokens),
            total_tokens: self.total_tokens.saturating_add(rhs.total_tokens),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
}

impl From<TokenCounts> for UsageSnapshot {
    fn from(value: TokenCounts) -> Self {
        let value = value.normalize();
        Self {
            input_tokens: value.input_tokens,
            cached_input_tokens: value.cached_input_tokens,
            cache_creation_input_tokens: value.cache_creation_input_tokens,
            output_tokens: value.output_tokens,
            reasoning_tokens: value.reasoning_tokens,
            total_tokens: value.total_tokens,
        }
    }
}

impl UsageSnapshot {
    pub fn is_zero(self) -> bool {
        TokenCounts::from(self).is_zero()
    }

    pub fn componentwise_ge(self, previous: Self) -> bool {
        self.input_tokens >= previous.input_tokens
            && self.cached_input_tokens >= previous.cached_input_tokens
            && self.cache_creation_input_tokens >= previous.cache_creation_input_tokens
            && self.output_tokens >= previous.output_tokens
            && self.reasoning_tokens >= previous.reasoning_tokens
            && self.total_tokens >= previous.total_tokens
    }

    pub fn delta_from(self, previous: Self) -> TokenCounts {
        TokenCounts {
            input_tokens: self.input_tokens.saturating_sub(previous.input_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_sub(previous.cached_input_tokens),
            cache_creation_input_tokens: self
                .cache_creation_input_tokens
                .saturating_sub(previous.cache_creation_input_tokens),
            output_tokens: self.output_tokens.saturating_sub(previous.output_tokens),
            reasoning_tokens: self
                .reasoning_tokens
                .saturating_sub(previous.reasoning_tokens),
            total_tokens: self.total_tokens.saturating_sub(previous.total_tokens),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CumulativeDelta {
    pub counts: TokenCounts,
    pub duplicate: bool,
    pub reset: bool,
}

#[derive(Clone, Debug, Default)]
pub struct CumulativeUsageTracker {
    previous: HashMap<String, UsageSnapshot>,
}

impl CumulativeUsageTracker {
    pub fn observe(&mut self, key: impl Into<String>, current: UsageSnapshot) -> CumulativeDelta {
        let key = key.into();
        let current = UsageSnapshot::from(TokenCounts::from(current));
        let Some(previous) = self.previous.get(&key).copied() else {
            self.previous.insert(key, current);
            return CumulativeDelta {
                counts: TokenCounts::from(current),
                duplicate: false,
                reset: false,
            };
        };

        if current == previous {
            return CumulativeDelta {
                counts: TokenCounts::default(),
                duplicate: true,
                reset: false,
            };
        }

        let reset = !current.componentwise_ge(previous);
        let counts = if reset {
            TokenCounts::from(current)
        } else {
            current.delta_from(previous)
        };
        self.previous.insert(key, current);
        CumulativeDelta {
            counts,
            duplicate: false,
            reset,
        }
    }

    pub fn seed(&mut self, key: impl Into<String>, snapshot: UsageSnapshot) {
        self.previous.insert(key.into(), snapshot);
    }

    pub fn get(&self, key: &str) -> Option<UsageSnapshot> {
        self.previous.get(key).copied()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileCursor {
    pub path: PathBuf,
    pub provider: Provider,
    pub file_identity: Option<String>,
    pub byte_offset: u64,
    pub file_size: u64,
    pub modified_at: Option<i64>,
    pub parser_version: u32,
    pub last_event_hash: Option<String>,
    pub last_cumulative: Option<UsageSnapshot>,
    pub source_metadata: SourceMetadata,
    pub updated_at: i64,
}

impl FileCursor {
    pub fn new(path: PathBuf, provider: Provider, parser_version: u32) -> Self {
        Self {
            path,
            provider,
            file_identity: None,
            byte_offset: 0,
            file_size: 0,
            modified_at: None,
            parser_version,
            last_event_hash: None,
            last_cumulative: None,
            source_metadata: SourceMetadata::default(),
            updated_at: Utc::now().timestamp(),
        }
    }
}

impl From<UsageSnapshot> for TokenCounts {
    fn from(value: UsageSnapshot) -> Self {
        Self {
            input_tokens: value.input_tokens,
            cached_input_tokens: value.cached_input_tokens,
            cache_creation_input_tokens: value.cache_creation_input_tokens,
            output_tokens: value.output_tokens,
            reasoning_tokens: value.reasoning_tokens,
            total_tokens: value.total_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(total: u64) -> UsageSnapshot {
        UsageSnapshot {
            input_tokens: total,
            total_tokens: total,
            ..Default::default()
        }
    }

    #[test]
    fn cumulative_snapshots_are_converted_to_deltas() {
        let mut tracker = CumulativeUsageTracker::default();
        let values = [1000, 1500, 1800]
            .into_iter()
            .map(|total| {
                tracker
                    .observe("session", snapshot(total))
                    .counts
                    .total_tokens
            })
            .collect::<Vec<_>>();
        assert_eq!(values, vec![1000, 500, 300]);
        assert_eq!(values.into_iter().sum::<u64>(), 1800);
    }

    #[test]
    fn duplicate_snapshot_is_zero() {
        let mut tracker = CumulativeUsageTracker::default();
        assert_eq!(
            tracker
                .observe("session", snapshot(1000))
                .counts
                .total_tokens,
            1000
        );
        assert_eq!(
            tracker
                .observe("session", snapshot(1500))
                .counts
                .total_tokens,
            500
        );
        let duplicate = tracker.observe("session", snapshot(1500));
        assert!(duplicate.duplicate);
        assert_eq!(duplicate.counts, TokenCounts::default());
        assert_eq!(
            tracker
                .observe("session", snapshot(1800))
                .counts
                .total_tokens,
            300
        );
    }

    #[test]
    fn reset_starts_a_new_counter_period_without_negative_tokens() {
        let mut tracker = CumulativeUsageTracker::default();
        tracker.observe("session", snapshot(1000));
        tracker.observe("session", snapshot(1500));
        let reset = tracker.observe("session", snapshot(200));
        assert!(reset.reset);
        assert_eq!(reset.counts.total_tokens, 200);
        assert_eq!(
            tracker
                .observe("session", snapshot(350))
                .counts
                .total_tokens,
            150
        );
    }
}
