//! Owns context-injection state: id dedup, run identity, and pending delivery.
//!
//! Protocol-input → domain-content compilation stays in `compile/`; this module
//! owns only the admission rules and state transitions of accepted injections.

use std::collections::{HashMap, HashSet};

use crate::cursor::protocol::proto::agent::v1 as pb;

/// An accepted injection waiting for its runtime-event commit.
pub(crate) struct PendingInjection {
    pub(crate) user_message: Option<pb::UserMessage>,
}

/// Outcome of admitting an `InjectContextAction` against the active Run.
#[derive(Debug)]
pub(crate) enum Admission {
    /// Fresh id addressed to this Run: queue it for delivery.
    Accepted,
    /// Id already seen (queued, delivered, or rejected): drop it silently.
    Duplicate,
    /// Id addressed to a different Run: reject it with this reason.
    Rejected(String),
}

/// Tracks context injections for one Run output session.
///
/// Owns the rules that were previously scattered across `output.rs`:
/// - injection ids are deduplicated for the whole Run, so client retries of a
///   queued, delivered, or rejected id are silent no-ops;
/// - `expected_run_id` is checked against the client's stable run identity,
///   never against the per-attempt transport request id: a stream-drop retry
///   reconnects with a fresh request id while the run id is unchanged;
/// - an accepted injection stays pending until the engine commits its runtime
///   event; tool rounds observed meanwhile are detached, not started.
pub(crate) struct InjectionTracker {
    run_identity: String,
    seen: HashSet<String>,
    pending: HashMap<String, PendingInjection>,
}

impl InjectionTracker {
    /// Builds the tracker for one Run: the client's stable run id when the
    /// request carries one, otherwise the transport request id.
    pub(crate) fn for_run(client_run_id: Option<&str>, request_id: &str) -> Self {
        Self::new(
            client_run_id
                .filter(|id| !id.is_empty())
                .unwrap_or(request_id),
        )
    }

    fn new(run_identity: impl Into<String>) -> Self {
        Self {
            run_identity: run_identity.into(),
            seen: HashSet::new(),
            pending: HashMap::new(),
        }
    }

    /// Admits an injection id: deduplication first, then the run identity
    /// check. A rejected id is remembered, so its retries stay no-ops.
    pub(crate) fn admit(&mut self, injection_id: &str, expected_run_id: &str) -> Admission {
        if !self.seen.insert(injection_id.to_owned()) {
            return Admission::Duplicate;
        }
        if expected_run_id != self.run_identity {
            return Admission::Rejected(format!(
                "InjectContextAction expected run {expected_run_id}, active run is {}",
                self.run_identity
            ));
        }
        Admission::Accepted
    }

    /// Queues an accepted injection (or a runtime user message, which shares
    /// the pending machinery without an identity check) under its id. The id
    /// joins the dedup set so later actions reusing it are silent no-ops.
    pub(crate) fn enqueue(&mut self, injection_id: String, user_message: Option<pb::UserMessage>) {
        self.seen.insert(injection_id.clone());
        self.pending
            .insert(injection_id, PendingInjection { user_message });
    }

    /// Takes the injection committed under `event_id`, if one is pending.
    /// Injections commit under `inject-context:{id}` while runtime user
    /// messages commit under their full `user-message:{id}` event id.
    pub(crate) fn take_committed(&mut self, event_id: &str) -> Option<(String, PendingInjection)> {
        let injection_id = event_id.strip_prefix("inject-context:").unwrap_or(event_id);
        self.pending
            .remove(injection_id)
            .map(|pending| (injection_id.to_owned(), pending))
    }

    /// Whether an accepted injection is still awaiting its commit. A tool
    /// round observed while one is pending must be detached, not started.
    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_message(text: &str) -> pb::UserMessage {
        pb::UserMessage {
            text: text.into(),
            ..Default::default()
        }
    }

    fn rejected_reason(admission: Admission) -> String {
        match admission {
            Admission::Rejected(reason) => reason,
            admission => panic!("expected a rejection, got {admission:?}"),
        }
    }

    #[test]
    fn seen_ids_are_deduplicated_whatever_their_target() {
        let mut tracker = InjectionTracker::new("run");
        assert!(matches!(tracker.admit("inj", "run"), Admission::Accepted));
        assert!(matches!(tracker.admit("inj", "run"), Admission::Duplicate));
        assert!(matches!(
            tracker.admit("inj", "other"),
            Admission::Duplicate
        ));
        // A rejected id is remembered: its retry must not re-run the check.
        assert!(matches!(
            tracker.admit("rejected", "other"),
            Admission::Rejected(_)
        ));
        assert!(matches!(
            tracker.admit("rejected", "run"),
            Admission::Duplicate
        ));
    }

    #[test]
    fn identity_uses_the_stable_run_id_not_the_attempt_request_id() {
        // A stream-drop retry reconnects with a fresh attempt request id while
        // the client keeps addressing injections by the original run id.
        let mut tracker = InjectionTracker::for_run(Some("client-run"), "retry-attempt-request");
        assert!(matches!(
            tracker.admit("reconnect", "client-run"),
            Admission::Accepted
        ));
        // Targeting only the current attempt id is stale: the run identity rules.
        assert_eq!(
            rejected_reason(tracker.admit("attempt", "retry-attempt-request")),
            "InjectContextAction expected run retry-attempt-request, active run is client-run"
        );
        assert_eq!(
            rejected_reason(tracker.admit("stale", "replaced-run")),
            "InjectContextAction expected run replaced-run, active run is client-run"
        );
    }

    #[test]
    fn requests_without_a_run_id_fall_back_to_the_request_id() {
        let mut tracker = InjectionTracker::for_run(None, "request");
        assert!(matches!(
            tracker.admit("inj", "request"),
            Admission::Accepted
        ));
        assert!(matches!(
            tracker.admit("stale", "other"),
            Admission::Rejected(_)
        ));
        let mut empty = InjectionTracker::for_run(Some(""), "request");
        assert!(matches!(empty.admit("inj", "request"), Admission::Accepted));
    }

    #[test]
    fn committed_injections_are_delivered_exactly_once() {
        let mut tracker = InjectionTracker::new("run");
        tracker.admit("inj", "run");
        tracker.enqueue("inj".into(), Some(user_message("hello")));
        assert!(tracker.has_pending());

        let (id, pending) = tracker
            .take_committed("inject-context:inj")
            .expect("pending injection");
        assert_eq!(id, "inj");
        assert_eq!(pending.user_message.expect("user message").text, "hello");
        assert!(!tracker.has_pending());
        assert!(
            tracker.take_committed("inject-context:inj").is_none(),
            "no replay after the commit cleared the pending entry"
        );
        // The delivered id stays in the dedup set: later retries are no-ops.
        assert!(matches!(tracker.admit("inj", "run"), Admission::Duplicate));
    }

    #[test]
    fn user_messages_commit_under_their_full_event_id() {
        let mut tracker = InjectionTracker::new("run");
        tracker.enqueue("user-message:follow-up".into(), None);
        let (id, pending) = tracker
            .take_committed("user-message:follow-up")
            .expect("pending user message");
        assert_eq!(id, "user-message:follow-up");
        assert!(pending.user_message.is_none());
        // A retried user message with the same id is a silent no-op.
        assert!(matches!(
            tracker.admit("user-message:follow-up", "ignored"),
            Admission::Duplicate
        ));
    }
}
