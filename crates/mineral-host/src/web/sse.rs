//! Server-sent events: what an operation is doing, as it does it.
//!
//! The shape follows the EventSource protocol exactly, because a browser is the
//! client:
//!
//! ```text
//! id: 17
//! event: progress
//! data: {"sequence":17,"kind":"stage","message":"…"}
//!
//! event: completed
//! data: {"state":"succeeded"}
//!
//! ```
//!
//! Two properties matter more than the format:
//!
//! * `id` is the operation's own sequence number, so a client that reconnects
//!   sends `Last-Event-ID` and is replayed only what it has not seen.
//! * The stream **ends** after the terminal event. It is a notification of
//!   change, never the record: the authoritative state is
//!   `GET /api/v1/operations/:id`, and a client that treats the last progress
//!   line as the result would be reading tea leaves.
//!
//! The supervisor is synchronous by design, so one dedicated thread per stream
//! forwards its events into an async channel. A local admin UI has a handful of
//! tabs, not thousands of subscribers.

use std::{convert::Infallible, pin::Pin, sync::Arc, task::Poll, time::Duration};

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::sse::{Event, KeepAlive, Sse},
};
use futures_core::Stream;
use serde::Serialize;

use crate::operations::{OperationEvent, OperationState};

use super::{WebState, dto::WebProgressEvent, error::WebApiError, operations::parse_id};

/// How often a stream that has nothing to say says so anyway.
///
/// A comment line keeps intermediaries and browsers from deciding a quiet
/// connection is a dead one.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// `GET /api/v1/operations/:id/events`
pub async fn events(
    State(state): State<Arc<WebState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Sse<EventStream>, WebApiError> {
    let id = parse_id(&id)?;
    let after = last_event_id(&headers);
    let subscription = state
        .supervisor()
        .subscribe_after(id, after)
        .ok_or_else(|| WebApiError::operation_not_found(id))?;

    let (sender, receiver) = tokio::sync::mpsc::channel(64);
    std::thread::Builder::new()
        .name(format!("mineral-sse-{id}"))
        .spawn(move || {
            for event in subscription {
                let outgoing = match event {
                    OperationEvent::Progress(progress) => {
                        Outgoing::Progress(WebProgressEvent::from_event(&progress))
                    }
                    OperationEvent::Finished { state } => {
                        // The terminal event, then the loop ends and the sender
                        // drops, which is what closes the stream.
                        let _ = sender.blocking_send(Outgoing::Terminal { state });
                        break;
                    }
                };
                if sender.blocking_send(outgoing).is_err() {
                    // The client went away. Nothing is lost: the record stays
                    // readable, and a reconnecting client is replayed.
                    break;
                }
            }
        })
        .map_err(|error| {
            WebApiError::internal(format!("could not start the event stream: {error}"))
        })?;

    Ok(Sse::new(EventStream { receiver }).keep_alive(
        KeepAlive::new()
            .interval(KEEP_ALIVE)
            .text("mineral keep-alive"),
    ))
}

/// One thing to send.
enum Outgoing {
    Progress(WebProgressEvent),
    Terminal { state: OperationState },
}

#[derive(Serialize)]
struct Terminal<'a> {
    state: &'a str,
}

impl Outgoing {
    fn into_event(self) -> Event {
        match self {
            Self::Progress(progress) => {
                let sequence = progress.sequence;
                // `data` is written by hand rather than through `json_data` so a
                // serialisation failure cannot silently drop a field.
                let data = serde_json::to_string(&progress).unwrap_or_else(|_| "{}".to_owned());
                Event::default()
                    .id(sequence.to_string())
                    .event("progress")
                    .data(data)
            }
            Self::Terminal { state } => {
                let name = if state == OperationState::Succeeded {
                    "completed"
                } else {
                    "failed"
                };
                let data = serde_json::to_string(&Terminal {
                    state: &state.to_string(),
                })
                .unwrap_or_else(|_| "{}".to_owned());
                Event::default().event(name).data(data)
            }
        }
    }
}

/// The client's last seen sequence, if it reconnected.
///
/// An unreadable value is treated as "start from the beginning" rather than as
/// an error: the worst case is a client rendering its history twice, which is
/// better than a stream that cannot be resumed.
fn last_event_id(headers: &HeaderMap) -> u64 {
    headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// A stream of events from the forwarding thread.
pub struct EventStream {
    receiver: tokio::sync::mpsc::Receiver<Outgoing>,
}

impl Stream for EventStream {
    type Item = Result<Event, Infallible>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        match self.receiver.poll_recv(context) {
            Poll::Ready(Some(outgoing)) => Poll::Ready(Some(Ok(outgoing.into_event()))),
            // The sender dropped: the operation finished, or the client left.
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}
