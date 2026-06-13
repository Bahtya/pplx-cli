use bytes::{Bytes, BytesMut};
use futures_util::Stream;
use memchr::memmem;
use serde::Deserialize;
use std::pin::Pin;
use std::sync::LazyLock;
use std::task::{Context, Poll};

use crate::error::{Error, Result};

const EVENT_MESSAGE_PREFIX: &[u8] = b"event: message\r\n";
const EVENT_END_OF_STREAM_PREFIX: &[u8] = b"event: end_of_stream\r\n";
const EVENT_PING_PREFIX: &[u8] = b"event: ping\r\n";
const DATA_PREFIX: &[u8] = b"data: ";
const DELIMITER: &[u8] = b"\r\n\r\n";
static DELIMITER_FINDER: LazyLock<memmem::Finder<'static>> =
    LazyLock::new(|| memmem::Finder::new(DELIMITER));

/// A parsed SSE event from the Perplexity stream.
#[derive(Debug, Clone)]
pub enum SseEvent {
    /// Streaming text chunk.
    Delta { text: String },
    /// Complete answer with optional web results.
    Answer {
        text: String,
        web_results: Vec<WebResult>,
        backend_uuid: Option<String>,
        read_write_token: Option<String>,
        /// True when the server silently routed a non-turbo request to the free
        /// `turbo` model. The full answer/sources/uuids are still carried so the
        /// client can render the response, clean up the thread, and warn the user.
        downgraded: bool,
    },
    /// Stream completed. Carries identifiers for follow-up and thread cleanup.
    Done {
        backend_uuid: Option<String>,
        read_write_token: Option<String>,
    },
    /// Web results returned during search phase.
    WebResults { items: Vec<WebResult> },
    /// Search planning progress.
    SearchStatus { progress: String },
    /// Metadata: thread title, related queries, display model.
    Metadata {
        thread_title: Option<String>,
        related_queries: Vec<String>,
        display_model: Option<String>,
    },
    /// Server error.
    Error { message: String },
}

#[derive(Debug, Clone, Deserialize)]
pub struct WebResult {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub snippet: String,
}

pin_project_lite::pin_project! {
    pub struct SseStream<S> {
        #[pin]
        inner: S,
        buffer: BytesMut,
        finished: bool,
        // model_preference we sent — used for downgrade detection
        requested_model: Option<String>,
        downgrade_detected: bool,
        done_sent: bool,
    }
}

impl<S> SseStream<S>
where
    S: Stream<Item = std::result::Result<Bytes, rquest::Error>>,
{
    pub fn new(inner: S, requested_model: Option<String>) -> Self {
        Self {
            inner,
            buffer: BytesMut::new(),
            finished: false,
            requested_model,
            downgrade_detected: false,
            done_sent: false,
        }
    }
}

impl<S> Stream for SseStream<S>
where
    S: Stream<Item = std::result::Result<Bytes, rquest::Error>>,
{
    type Item = Result<SseEvent>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        if *this.finished {
            return Poll::Ready(None);
        }

        loop {
            // Try to parse accumulated events
            if let Some(event) = try_parse_events(this.buffer, this.finished, this.done_sent, this.requested_model, this.downgrade_detected) {
                return Poll::Ready(Some(Ok(event)));
            }

            if *this.finished {
                return Poll::Ready(None);
            }

            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    this.buffer.extend_from_slice(&chunk);
                }
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(Error::SearchRequest(e))));
                }
                Poll::Ready(None) => {
                    *this.finished = true;
                    // If we haven't sent done yet, try to flush remaining
                    if this.buffer.is_empty() {
                        return Poll::Ready(None);
                    }
                    // Loop back to try parsing remaining data
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }
    }
}

/// Try to extract the next complete event from the buffer.
fn try_parse_events(
    buffer: &mut BytesMut,
    finished: &mut bool,
    done_sent: &mut bool,
    requested_model: &Option<String>,
    downgrade_detected: &mut bool,
) -> Option<SseEvent> {
    let pos = DELIMITER_FINDER.find(buffer)?;
    let event_bytes = buffer.split_to(pos + DELIMITER.len());
    let event_data = &event_bytes[..pos];

    // end_of_stream
    if event_data.starts_with(EVENT_END_OF_STREAM_PREFIX) {
        *finished = true;
        if !*done_sent {
            *done_sent = true;
            return Some(SseEvent::Done {
                backend_uuid: None,
                read_write_token: None,
            });
        }
        return None;
    }

    // ping — skip
    if event_data.starts_with(EVENT_PING_PREFIX) {
        return None; // will loop again
    }

    // message event
    if event_data.starts_with(EVENT_MESSAGE_PREFIX) {
        let after_event = &event_data[EVENT_MESSAGE_PREFIX.len()..];
        let data_start = memmem::find(after_event, DATA_PREFIX)?;
        let json_bytes = &after_event[data_start + DATA_PREFIX.len()..];
        let json_str = std::str::from_utf8(json_bytes).ok()?;

        let event = parse_message_event(json_str, requested_model, *downgrade_detected);
        if matches!(event, SseEvent::Answer { downgraded: true, .. }) {
            *downgrade_detected = true;
        }
        if matches!(event, SseEvent::Done { .. }) {
            *done_sent = true;
        }
        return Some(event);
    }

    None
}

/// Parse a `data:` payload from a `message` SSE event.
fn parse_message_event(
    json_str: &str,
    requested_model: &Option<String>,
    downgrade_detected: bool,
) -> SseEvent {
    let outer: serde_json::Map<String, serde_json::Value> =
        match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(_) => return SseEvent::Error {
                message: "Invalid JSON in SSE event".into(),
            },
        };

    // Error responses
    let status_str = outer
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_uppercase();

    if status_str == "FAILED" || outer.get("error_code").is_some() {
        let msg = outer
            .get("text")
            .or_else(|| outer.get("error_code"))
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown error");
        return SseEvent::Error {
            message: format!("Perplexity: {msg}"),
        };
    }
    if status_str == "BLOCKED" {
        return SseEvent::Error {
            message: "请求被阻止（可能是付费墙或限速）".into(),
        };
    }

    // Extract blocks for streaming text (PENDING events with markdown_block)
    if status_str == "PENDING" || status_str.is_empty() {
        if let Some(blocks) = outer.get("blocks").and_then(|v| v.as_array()) {
            for block in blocks {
                // Search planning progress
                if let Some(pb) = block.get("plan_block") {
                    if let Some(progress) = pb.get("progress").and_then(|v| v.as_str()) {
                        return SseEvent::SearchStatus {
                            progress: progress.into(),
                        };
                    }
                }
                // Streaming markdown chunks
                if let Some(mb) = block.get("markdown_block") {
                    if let Some(chunks) = mb.get("chunks").and_then(|v| v.as_array()) {
                        let text: String = chunks
                            .iter()
                            .filter_map(|c| c.as_str())
                            .collect();
                        if !text.is_empty() {
                            return SseEvent::Delta { text };
                        }
                    }
                }
            }
        }
        return SseEvent::Delta { text: String::new() }; // skip empty pending
    }

    // COMPLETED event — extract answer from text steps
    if status_str == "COMPLETED" {
        let backend_uuid = outer
            .get("backend_uuid")
            .and_then(|v| v.as_str())
            .map(String::from);
        let read_write_token = outer
            .get("read_write_token")
            .and_then(|v| v.as_str())
            .map(String::from);

        // Detect a silent downgrade to the free `turbo` model (the server routed
        // a non-turbo request to turbo). We still emit the full Answer so the
        // response text, sources, and thread uuids are preserved; `downgraded`
        // lets the client print a heads-up. Only flagged once per stream.
        let downgraded = !downgrade_detected
            && outer.get("display_model").and_then(|v| v.as_str()) == Some("turbo")
            && matches!(requested_model.as_deref(), Some(r) if r != "turbo");

        // Try to extract answer from text/FINAL step
        if let Some(answer_text) = extract_final_answer(&outer) {
            let web_results = extract_web_results(&outer);
            return SseEvent::Answer {
                text: answer_text,
                web_results,
                backend_uuid,
                read_write_token,
                downgraded,
            };
        }

        // Fallback to top-level answer
        if let Some(answer) = outer.get("answer").and_then(|v| v.as_str()) {
            return SseEvent::Answer {
                text: answer.into(),
                web_results: Vec::new(),
                backend_uuid,
                read_write_token,
                downgraded,
            };
        }

        // Done with no answer text (e.g. search-only)
        SseEvent::Done {
            backend_uuid,
            read_write_token,
        }
    } else {
        // Unknown status — try to extract text
        if let Some(answer) = outer.get("answer").and_then(|v| v.as_str()) {
            return SseEvent::Delta { text: answer.into() };
        }
        SseEvent::Delta { text: String::new() }
    }
}

/// Extract the answer text from the FINAL step inside the `text` field.
fn extract_final_answer(outer: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let text_value = outer.get("text")?;

    // text may be a JSON string or already an array
    let steps: Vec<serde_json::Value> = if let Some(s) = text_value.as_str() {
        serde_json::from_str(s).ok()?
    } else if text_value.is_array() {
        serde_json::from_value(text_value.clone()).ok()?
    } else {
        return None;
    };

    for step in steps {
        let step_type = step.get("step_type").and_then(|v| v.as_str())?;
        if step_type != "FINAL" {
            continue;
        }
        let answer_str = step
            .get("content")?
            .get("answer")?
            .as_str()?
            .to_string();

        // The answer field itself may be a JSON string containing {answer, web_results}
        if let Ok(payload) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
            &answer_str,
        ) {
            return payload
                .get("answer")
                .and_then(|v| v.as_str())
                .map(String::from);
        }
        return Some(answer_str);
    }
    None
}

/// Extract web results from the FINAL step.
fn extract_web_results(outer: &serde_json::Map<String, serde_json::Value>) -> Vec<WebResult> {
    let text_value = match outer.get("text") {
        Some(v) => v,
        None => return Vec::new(),
    };

    let steps: Vec<serde_json::Value> = if let Some(s) = text_value.as_str() {
        match serde_json::from_str(s) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        }
    } else if text_value.is_array() {
        match serde_json::from_value(text_value.clone()) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        }
    } else {
        return Vec::new();
    };

    for step in steps {
        let Some(step_type) = step.get("step_type").and_then(|v| v.as_str()) else {
            continue;
        };
        if step_type != "FINAL" {
            continue;
        }
        let Some(answer_str) = step
            .get("content")
            .and_then(|c| c.get("answer"))
            .and_then(|a| a.as_str())
        else {
            continue;
        };
        if let Ok(payload) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
            answer_str,
        ) {
            if let Some(wr) = payload.get("web_results") {
                if let Ok(results) = serde_json::from_value::<Vec<WebResult>>(wr.clone()) {
                    return results;
                }
            }
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::{parse_message_event, SseEvent};

    /// Build a COMPLETED event whose FINAL step carries an answer + one web
    /// result, served by `display_model`. Used to exercise the downgrade path.
    fn completed(display_model: &str, requested: Option<&str>, already_detected: bool) -> SseEvent {
        let payload = serde_json::json!({
            "answer": "42",
            "web_results": [{"name": "Source", "url": "https://example.com", "snippet": "snip"}]
        });
        let outer = serde_json::json!({
            "status": "COMPLETED",
            "display_model": display_model,
            "backend_uuid": "backend-1",
            "read_write_token": "rwt-1",
            "text": [{"step_type": "FINAL", "content": {"answer": payload.to_string()}}]
        });
        let requested = requested.map(String::from);
        parse_message_event(&outer.to_string(), &requested, already_detected)
    }

    #[test]
    fn turbo_downgrade_carries_answer_sources_and_uuids() {
        // Regression: the downgrade path used to discard these.
        match completed("turbo", Some("claude46sonnet"), false) {
            SseEvent::Answer {
                text, web_results, backend_uuid, read_write_token, downgraded,
            } => {
                assert!(downgraded, "should be flagged as a turbo downgrade");
                assert_eq!(text, "42");
                assert_eq!(backend_uuid.as_deref(), Some("backend-1"));
                assert_eq!(read_write_token.as_deref(), Some("rwt-1"));
                assert_eq!(web_results.len(), 1, "sources must survive the downgrade");
                assert_eq!(web_results[0].name, "Source");
                assert_eq!(web_results[0].url, "https://example.com");
            }
            other => panic!("expected Answer, got {other:?}"),
        }
    }

    #[test]
    fn no_downgrade_when_turbo_was_requested() {
        // search mode requests turbo -> a turbo response is not a downgrade.
        match completed("turbo", None, false) {
            SseEvent::Answer { downgraded, .. } => assert!(!downgraded),
            other => panic!("expected Answer, got {other:?}"),
        }
    }

    #[test]
    fn downgrade_flagged_only_once_per_stream() {
        match completed("turbo", Some("claude46sonnet"), true) {
            SseEvent::Answer { downgraded, .. } => assert!(!downgraded),
            other => panic!("expected Answer, got {other:?}"),
        }
    }

    #[test]
    fn honored_model_is_not_a_downgrade() {
        match completed("claude-4.6-sonnet", Some("claude46sonnet"), false) {
            SseEvent::Answer { text, downgraded, .. } => {
                assert!(!downgraded);
                assert_eq!(text, "42");
            }
            other => panic!("expected Answer, got {other:?}"),
        }
    }

    #[test]
    fn top_level_answer_fallback_flags_downgrade() {
        let outer = serde_json::json!({
            "status": "COMPLETED",
            "display_model": "turbo",
            "backend_uuid": "b2",
            "read_write_token": "r2",
            "answer": "fallback answer"
        });
        let req = Some("claude46sonnet".to_string());
        match parse_message_event(&outer.to_string(), &req, false) {
            SseEvent::Answer { text, web_results, downgraded, .. } => {
                assert!(downgraded);
                assert_eq!(text, "fallback answer");
                assert!(web_results.is_empty());
            }
            other => panic!("expected Answer, got {other:?}"),
        }
    }
}
