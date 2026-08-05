//! Server-to-client MCP notifications.
//!
//! Two frames leave the server unsolicited:
//!
//! * `notifications/bobby/event` — the principal's runtime events, carrying the
//!   exact `Event` body `GET /v1/events` returns in its `events` array.
//! * `notifications/tools/list_changed` — the principal's capability set
//!   changed, so the tool list it last downloaded is stale.
//!
//! Both are JSON-RPC *notifications*: they carry no `id`. A frame with an `id`
//! is a request, and a client will try to answer it.
//!
//! Events are never re-broadcast through a lossy ring: the event half of a
//! subscription is a per-subscriber cursor read off `EventStore`, the same call
//! `GET /v1/events` and the `events_read` tool make, with the same gap
//! semantics. The broadcast carries only the stateless control frames.
//!
//! Principal scoping: a [`NotificationSink`] is built from the owning `Server`'s
//! own `CapabilityHandle`, and the only read path it can reach is
//! `EventStore::read_after_for`, which filters to that principal's audience.
//! Keep it that way: no constructor taking an arbitrary principal, no unscoped
//! read anywhere in this module.

use std::collections::VecDeque;

use interface_core::{Event, EventGap, EventStore};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use types::{EventCursor, PrincipalId};

/// Runtime event frames. `params` is one element of the `events` array
/// `GET /v1/events` returns, verbatim.
pub const EVENT_METHOD: &str = "notifications/bobby/event";

/// The principal's capability set changed; the client must re-run `tools/list`.
pub const TOOLS_LIST_CHANGED_METHOD: &str = "notifications/tools/list_changed";

/// `kind` of the frame that reports lost events — the same marker
/// `GET /v1/events?stream=1` uses for its terminal SSE gap event, so a client
/// that already understands one stream understands the other.
pub const GAP_KIND: &str = "event.gap";

/// Control frames are stateless and rare (one per capability rotation). The
/// ring only has to absorb a burst while a subscriber is busy writing a frame.
const CONTROL_CAPACITY: usize = 32;

/// Events pulled per `EventStore` read. Bounded so one read cannot pin an
/// arbitrary slice of retention in memory; the subscriber drains the batch one
/// frame at a time before reading again.
const EVENT_READ_LIMIT: usize = 64;

/// The outbound notification fan-out for one principal's `Server`.
pub struct NotificationSink {
    control: broadcast::Sender<Value>,
    events: EventStore,
    principal: PrincipalId,
}

impl NotificationSink {
    pub(crate) fn new(events: EventStore, principal: PrincipalId) -> Self {
        let (control, _) = broadcast::channel(CONTROL_CAPACITY);
        Self {
            control,
            events,
            principal,
        }
    }

    /// Opens a subscription positioned at the store's tail: every event appended
    /// for this principal from this moment on, and no history.
    ///
    /// Must not start at [`EventCursor::ZERO`]. `HistoryLost` is judged against
    /// the *store-wide* front of retention and the log is shared by every
    /// principal, so after `max_event_retention` appends (16,384 by default) a
    /// cursor-`ZERO` subscription gaps on its first read, unrecoverably: MCP
    /// gives the client no way to name a resume cursor.
    ///
    /// Seed here, not on the first `recv`, so an event appended in between is
    /// still delivered.
    pub async fn subscribe(&self) -> NotificationStream {
        NotificationStream {
            control: self.control.subscribe(),
            cursor: self.events.latest_cursor().await,
            events: self.events.clone(),
            principal: self.principal.clone(),
            queued: VecDeque::new(),
            events_open: true,
            reseek: false,
            control_open: true,
        }
    }

    pub(crate) fn publish(&self, frame: Value) {
        // `send` fails only when nobody is subscribed, which is not an error:
        // a notification exists for the clients that are listening.
        let _ = self.control.send(frame);
    }
}

/// One client's view of a [`NotificationSink`].
///
/// Must be held by the transport, never by the `Server`: that is what makes a
/// capability rotation observable. `McpServers` replaces the cached `Server` on
/// a rotated handle, the old sink's sender drops, and this stream delivers the
/// buffered `tools/list_changed` and then ends.
pub struct NotificationStream {
    control: broadcast::Receiver<Value>,
    events: EventStore,
    principal: PrincipalId,
    cursor: EventCursor,
    queued: VecDeque<Value>,
    events_open: bool,
    /// Set when a gap has been reported and the cursor must be re-seeded from
    /// the store's tail before the next read.
    reseek: bool,
    control_open: bool,
}

impl NotificationStream {
    /// Drops the event half of this subscription, leaving control frames only.
    ///
    /// For a principal not authorized for `SubscribeEvents`: the MCP channel
    /// still has to exist (clients open it before they will POST) but must not
    /// carry event data `events_read` and `GET /v1/events` would both refuse.
    pub fn control_only(mut self) -> Self {
        self.events_open = false;
        self
    }

    /// Re-evaluate event delivery mid-stream: a token rotation can drop (or
    /// grant) `SubscribeEvents` after the channel opened, so transports
    /// re-check the guard per poll and toggle delivery without tearing down
    /// the channel.
    pub fn set_events_open(&mut self, open: bool) {
        self.events_open = open;
    }

    /// The next frame to write to the client, or `None` once the owning
    /// `Server` is gone.
    ///
    /// Cancel-safe, and must stay so: both transports drop this future mid-poll.
    /// All three inner `await`s are cancel-safe primitives re-issued from the
    /// same cursor, and decoded frames are parked in `queued` rather than held
    /// across an await.
    pub async fn recv(&mut self) -> Option<Value> {
        loop {
            // Re-seek ahead of the queue drain, not after it, so the whole
            // gap-and-resume sequence stays inside one `recv` call. Otherwise
            // anything appended between the gap frame's return and the next
            // `recv` is skipped on a stale cursor.
            if self.reseek {
                self.cursor = self.events.latest_cursor().await;
                self.reseek = false;
            }
            if let Some(frame) = self.queued.pop_front() {
                return Some(frame);
            }
            if !self.control_open {
                return None;
            }
            // Destructured so the two branches below borrow disjoint fields.
            let Self {
                control,
                events,
                principal,
                cursor,
                queued,
                events_open,
                reseek,
                control_open,
            } = self;
            if *events_open {
                tokio::select! {
                    received = control.recv() => {
                        drain_control(received, queued, control_open);
                    }
                    read = events.read_after_for(principal, *cursor, EVENT_READ_LIMIT) => {
                        match read {
                            Ok(batch) => {
                                for event in batch.events {
                                    *cursor = event.cursor;
                                    queued.push_back(event_frame(&event));
                                }
                            }
                            Err(gap) => {
                                // Retention passed this subscriber: report the
                                // gap, then re-arm from the store's tail. Do
                                // not latch the stream closed instead. Over
                                // stdio `Server::serve` subscribes once and
                                // cannot reconnect, so one transient gap would
                                // silence notifications for the whole session.
                                queued.push_back(gap_frame(*cursor, gap));
                                *reseek = true;
                            }
                        }
                    }
                }
            } else {
                let received = control.recv().await;
                drain_control(received, queued, control_open);
            }
        }
    }
}

fn drain_control(
    received: Result<Value, broadcast::error::RecvError>,
    queued: &mut VecDeque<Value>,
    control_open: &mut bool,
) {
    match received {
        Ok(frame) => queued.push_back(frame),
        // Control frames are never dropped silently. `tools/list_changed` is the
        // only one and it is idempotent, so re-synthesize it on lag.
        Err(broadcast::error::RecvError::Lagged(_)) => queued.push_back(tools_list_changed_frame()),
        Err(broadcast::error::RecvError::Closed) => *control_open = false,
    }
}

pub(crate) fn tools_list_changed_frame() -> Value {
    json!({"jsonrpc":"2.0","method":TOOLS_LIST_CHANGED_METHOD})
}

/// `params` is the `Event` exactly as `GET /v1/events` serializes it, so a
/// client can decode both with one type.
fn event_frame(event: &Event) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": EVENT_METHOD,
        "params": {
            "cursor": event.cursor,
            "kind": event.kind,
            "payload": event.payload
        }
    })
}

/// Reports lost events in the same envelope as a real event, with `cursor` set
/// to the last cursor this subscriber actually delivered and `payload` carrying
/// the `EventGap` the HTTP stream would have sent.
fn gap_frame(cursor: EventCursor, gap: EventGap) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": EVENT_METHOD,
        "params": {
            "cursor": cursor,
            "kind": GAP_KIND,
            "payload": gap
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_frames_never_carry_an_id() {
        let frames = [
            tools_list_changed_frame(),
            event_frame(&Event::new("command.outcome", json!({"ok": true}))),
            gap_frame(
                EventCursor(7),
                EventGap {
                    reason: interface_core::EventGapReason::HistoryLost,
                    earliest_available: EventCursor(9),
                },
            ),
        ];
        for frame in frames {
            assert_eq!(frame["jsonrpc"], "2.0", "{frame}");
            assert!(frame.get("id").is_none(), "{frame}");
            assert!(frame["method"].is_string(), "{frame}");
        }
    }

    #[test]
    fn a_gap_frame_reports_the_reason_and_the_earliest_cursor_still_available() {
        let frame = gap_frame(
            EventCursor(3),
            EventGap {
                reason: interface_core::EventGapReason::HistoryLost,
                earliest_available: EventCursor(11),
            },
        );
        assert_eq!(frame["params"]["kind"], GAP_KIND);
        assert_eq!(frame["params"]["cursor"], 3);
        assert_eq!(frame["params"]["payload"]["reason"], "historyLost");
        assert_eq!(frame["params"]["payload"]["earliestAvailable"], 11);
    }
}
