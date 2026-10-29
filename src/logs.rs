//! Paginated log snapshots, decoded from Docker's stream as it arrives.
use super::{ApiError, ApiResult, AppState, diagnostics, docker, engine, unavailable, valid_id};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

pub(super) const PAGE_SIZE: usize = 100;
pub(super) const MAX_LINES: usize = 10_000;
const MAX_PAGE: usize = MAX_LINES / PAGE_SIZE - 1;
pub(super) const MAX_LOG_BYTES: usize = 8 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 16 * 1024;
const SNAPSHOT_TTL: Duration = Duration::from_secs(300);
const MAX_SNAPSHOTS: usize = 8;

pub(super) struct Snapshot {
    id: String,
    until: String,
    capture: Capture,
    created: Instant,
}

#[derive(Clone)]
struct Capture {
    name: String,
    since: Option<String>,
    lines: Arc<[String]>,
    truncated: bool,
}

#[derive(Default, Deserialize)]
pub(super) struct LogQuery {
    pub page: Option<usize>,
    pub until: Option<String>,
    pub since: Option<String>,
}

#[derive(Serialize)]
pub(super) struct LogPage {
    pub name: String,
    pub lines: Vec<String>,
    pub page: usize,
    pub until: String,
    pub since: Option<String>,
    pub has_older: bool,
    pub history_limit_reached: bool,
}

/// Accepts RFC 3339 timestamps or durations before now, such as `15m` or `2h 30m`.
fn parse_since(since: &str) -> Result<DateTime<Utc>, ApiError> {
    DateTime::parse_from_rfc3339(since)
        .map(|date| date.with_timezone(&Utc))
        .or_else(|_| {
            let ago = humantime::parse_duration(since).map_err(|_| ())?;
            Ok(Utc::now() - chrono::Duration::from_std(ago).map_err(|_| ())?)
        })
        .map_err(|()| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "Invalid since: use an RFC 3339 time or a duration such as 15m".into(),
            )
        })
}

/// Splits Docker's multiplexed log stream into redacted lines, keeping only
/// the newest lines within the line and byte limits.
struct Collector {
    tty: bool,
    secrets: Vec<String>,
    header: [u8; 8],
    header_len: usize,
    stream: usize,
    remaining: usize,
    partial: [Vec<u8>; 2],
    overlong: [bool; 2],
    lines: VecDeque<String>,
    bytes: usize,
    truncated: bool,
}
impl Collector {
    fn new(tty: bool, secrets: Vec<String>) -> Self {
        Self {
            tty,
            secrets,
            header: [0; 8],
            header_len: 0,
            stream: 0,
            remaining: 0,
            partial: Default::default(),
            overlong: [false; 2],
            lines: VecDeque::new(),
            bytes: 0,
            truncated: false,
        }
    }
    fn push(&mut self, mut chunk: &[u8]) -> Result<(), ApiError> {
        if self.tty {
            self.data(0, chunk);
            return Ok(());
        }
        while !chunk.is_empty() {
            if self.remaining == 0 {
                let take = (8 - self.header_len).min(chunk.len());
                self.header[self.header_len..self.header_len + take]
                    .copy_from_slice(&chunk[..take]);
                self.header_len += take;
                chunk = &chunk[take..];
                if self.header_len < 8 {
                    break;
                }
                self.header_len = 0;
                if !matches!(self.header[0], 1 | 2) || self.header[1..4] != [0, 0, 0] {
                    return Err(unavailable("Malformed Docker log frame"));
                }
                self.stream = self.header[0] as usize - 1;
                self.remaining = u32::from_be_bytes(self.header[4..8].try_into().unwrap()) as usize;
                continue;
            }
            let take = self.remaining.min(chunk.len());
            self.data(self.stream, &chunk[..take]);
            self.remaining -= take;
            chunk = &chunk[take..];
        }
        Ok(())
    }
    fn data(&mut self, stream: usize, mut bytes: &[u8]) {
        while let Some(newline) = bytes.iter().position(|&b| b == b'\n') {
            self.append(stream, &bytes[..newline]);
            self.complete(stream);
            bytes = &bytes[newline + 1..];
        }
        self.append(stream, bytes);
    }
    fn append(&mut self, stream: usize, bytes: &[u8]) {
        let partial = &mut self.partial[stream];
        let room = MAX_LINE_BYTES.saturating_sub(partial.len());
        partial.extend_from_slice(&bytes[..bytes.len().min(room)]);
        self.overlong[stream] |= bytes.len() > room;
    }
    fn complete(&mut self, stream: usize) {
        let raw = std::mem::take(&mut self.partial[stream]);
        let text = String::from_utf8_lossy(&raw);
        let mut line =
            diagnostics::redact_log_line(text.strip_suffix('\r').unwrap_or(&text), &self.secrets);
        if std::mem::take(&mut self.overlong[stream]) {
            line.push_str(" [line truncated]");
        }
        self.bytes += line.len();
        self.lines.push_back(line);
        while self.lines.len() > MAX_LINES || self.bytes > MAX_LOG_BYTES {
            let dropped = self.lines.pop_front().unwrap();
            self.bytes -= dropped.len();
            self.truncated = true;
        }
    }
    fn finish(mut self) -> Result<(Vec<String>, bool), ApiError> {
        if self.remaining > 0 || self.header_len > 0 {
            return Err(unavailable("Truncated Docker log frame"));
        }
        for stream in 0..2 {
            if !self.partial[stream].is_empty() {
                self.complete(stream);
            }
        }
        let mut lines = Vec::from(self.lines);
        // Docker pads timestamps to nanoseconds, so lexical order is
        // chronological. The sort is stable, so equal timestamps keep their order.
        lines.sort_by(|a, b| {
            a.split_once(' ')
                .map(|v| v.0)
                .cmp(&b.split_once(' ').map(|v| v.0))
        });
        Ok((lines, self.truncated))
    }
}

fn paginate(lines: &[String], page: usize) -> (Vec<String>, bool) {
    let end = lines.len().saturating_sub(page * PAGE_SIZE);
    let start = end.saturating_sub(PAGE_SIZE);
    (lines[start..end].to_vec(), start > 0)
}

async fn capture(
    state: &AppState,
    id: &str,
    since: Option<DateTime<Utc>>,
) -> Result<Capture, ApiError> {
    let inspected: docker::ContainerInspect =
        engine::json(state, &format!("/containers/{id}/json")).await?;
    let mut collector =
        Collector::new(inspected.config.tty, diagnostics::secret_values(&inspected));
    // One extra line shows whether older output was left out.
    let mut path = format!(
        "/containers/{id}/logs?stdout=1&stderr=1&timestamps=1&tail={}",
        MAX_LINES + 1
    );
    if let Some(since) = since {
        path.push_str(&format!(
            "&since={}.{:09}",
            since.timestamp(),
            since.timestamp_subsec_nanos()
        ));
    }
    engine::stream(state, &path, |chunk| collector.push(&chunk)).await?;
    let (lines, truncated) = collector.finish()?;
    Ok(Capture {
        name: inspected
            .name
            .as_deref()
            .unwrap_or("unnamed")
            .trim_start_matches('/')
            .into(),
        since: since.map(|date| date.to_rfc3339_opts(SecondsFormat::Nanos, true)),
        lines: lines.into(),
        truncated,
    })
}

pub(super) async fn logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LogQuery>,
) -> ApiResult<LogPage> {
    let page = query.page.unwrap_or(0);
    if !valid_id(&id) || page > MAX_PAGE {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("Invalid container ID or page (allowed: 0–{MAX_PAGE})"),
        ));
    }
    let (until, capture) = if let Some(until) = query.until {
        let mut snapshots = state.snapshots.lock().unwrap();
        snapshots.retain(|s| s.created.elapsed() < SNAPSHOT_TTL);
        let snapshot = snapshots
            .iter()
            .find(|s| s.until == until && s.id == id)
            .ok_or_else(|| {
                ApiError(
                    StatusCode::GONE,
                    "Log snapshot expired. Select Latest to start again.".into(),
                )
            })?;
        (until, snapshot.capture.clone())
    } else {
        if page != 0 {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "History pages require a snapshot timestamp".into(),
            ));
        }
        let since = query
            .since
            .as_deref()
            .filter(|since| !since.trim().is_empty())
            .map(parse_since)
            .transpose()?;
        // Capture once: Docker may apply --tail before --until. Re-reading for
        // each page could otherwise skip entries as new logs arrive.
        let until = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
        let capture = capture(&state, &id, since).await?;
        let mut snapshots = state.snapshots.lock().unwrap();
        snapshots.retain(|s| s.created.elapsed() < SNAPSHOT_TTL);
        while snapshots.len() >= MAX_SNAPSHOTS {
            snapshots.pop_front();
        }
        snapshots.push_back(Snapshot {
            id,
            until: until.clone(),
            capture: capture.clone(),
            created: Instant::now(),
        });
        (until, capture)
    };
    let (lines, has_older) = paginate(&capture.lines, page);
    Ok(Json(LogPage {
        name: capture.name,
        lines,
        page,
        until,
        since: capture.since,
        has_older,
        history_limit_reached: !has_older && capture.truncated,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pages_have_no_overlap_even_with_identical_timestamps() {
        let lines: Vec<_> = (0..251)
            .map(|i| format!("2026-01-01T00:00:00.000000000Z line {i}"))
            .collect();
        let (latest, older) = paginate(&lines, 0);
        assert_eq!(latest, lines[151..]);
        assert!(older);
        let (second, older) = paginate(&lines, 1);
        assert_eq!(second, lines[51..151]);
        assert!(older);
        let (last, older) = paginate(&lines, 2);
        assert_eq!(last, lines[..51]);
        assert!(!older);
        assert!(paginate(&lines, 3).0.is_empty());
    }
    #[test]
    fn frames_split_across_chunks_are_reassembled() {
        let frame = |stream: u8, bytes: &[u8]| {
            let mut output = vec![stream, 0, 0, 0];
            output.extend((bytes.len() as u32).to_be_bytes());
            output.extend(bytes);
            output
        };
        let mut frames = frame(1, b"a\nb");
        frames.extend(frame(2, b"c\n"));
        frames.extend(frame(1, b"\n"));
        let mut collector = Collector::new(false, Vec::new());
        for chunk in frames.chunks(3) {
            collector.push(chunk).unwrap();
        }
        assert_eq!(collector.finish().unwrap().0, ["a", "c", "b"]);
        let mut collector = Collector::new(false, Vec::new());
        collector.push(&[1, 0, 0, 0, 0, 0, 0, 9, b'x']).unwrap();
        assert!(collector.finish().is_err());
    }
    #[test]
    fn since_accepts_timestamps_and_durations() {
        assert_eq!(
            parse_since("2026-01-01T01:00:00+01:00").unwrap(),
            "2026-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
        let ago = Utc::now() - parse_since("15m").unwrap();
        assert!((899..=901).contains(&ago.num_seconds()));
        assert!(parse_since("yesterday").is_err());
    }
}
