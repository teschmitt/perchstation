//! Parse the station's JSON-on-stderr stream into structured events and
//! provide ergonomic accessors for assertions.
//!
//! The station emits one JSON object per line to stderr (see
//! `contracts/log-events.md`). Tests capture stderr from the subprocess,
//! call [`parse_json_events`] to turn it into `Vec<serde_json::Value>`,
//! then drive assertions via [`find_event`] or [`event_codes`].

use serde_json::Value;

/// Parse every UTF-8 line of `stderr` that decodes as a JSON object.
/// Lines that aren't valid JSON (e.g., a panic backtrace from
/// `unimplemented!()`) are silently skipped — RED tests need to inspect
/// the events that did fire without choking on the noise that
/// accompanies a panic.
#[must_use]
pub fn parse_json_events(stderr: &[u8]) -> Vec<Value> {
    let text = std::str::from_utf8(stderr).unwrap_or("");
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .filter(Value::is_object)
        .collect()
}

/// Return the first event in `events` whose `event` field equals `code`.
#[must_use]
pub fn find_event<'a>(events: &'a [Value], code: &str) -> Option<&'a Value> {
    events.iter().find(|ev| ev.get("event").and_then(Value::as_str) == Some(code))
}

/// Return every event in `events` whose `event` field equals `code`.
#[must_use]
pub fn find_events<'a>(events: &'a [Value], code: &str) -> Vec<&'a Value> {
    events.iter().filter(|ev| ev.get("event").and_then(Value::as_str) == Some(code)).collect()
}

/// Collect the `event` codes from `events` in order. Useful for asserting
/// an exact ordering of events fired (e.g., `enrollment.qr_decoded` →
/// `enrollment.csr_generated` → `enrollment.sent` → `enrollment.persisted`).
#[must_use]
pub fn event_codes(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|ev| ev.get("event").and_then(Value::as_str).map(str::to_string))
        .collect()
}
