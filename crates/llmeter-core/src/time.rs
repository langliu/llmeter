use chrono::{DateTime, NaiveDateTime, Utc};

pub fn parse_timestamp(value: Option<&serde_json::Value>) -> DateTime<Utc> {
    let Some(value) = value else {
        return Utc::now();
    };
    if let Some(number) = value.as_i64() {
        let seconds = if number > 10_000_000_000 {
            number / 1_000
        } else {
            number
        };
        return DateTime::<Utc>::from_timestamp(seconds, 0).unwrap_or_else(Utc::now);
    }
    let Some(text) = value.as_str() else {
        return Utc::now();
    };
    DateTime::parse_from_rfc3339(text)
        .map(|date| date.with_timezone(&Utc))
        .or_else(|_| {
            NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
                .map(|date| DateTime::<Utc>::from_naive_utc_and_offset(date, Utc))
        })
        .unwrap_or_else(|_| Utc::now())
}
