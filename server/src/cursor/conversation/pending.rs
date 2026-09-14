//! Stores messages waiting across Run lifecycle boundaries.

use std::collections::{HashSet, VecDeque};

use super::CompiledMessages;
use crate::model::TerminalCompletion;

#[derive(Default)]
pub struct PendingMessages {
    queued: VecDeque<CompiledMessages>,
    event_ids: HashSet<String>,
}

impl PendingMessages {
    pub fn push(&mut self, messages: CompiledMessages) -> bool {
        if !self.event_ids.insert(messages.event_id.clone()) {
            return false;
        }
        self.queued.push_back(messages);
        true
    }

    pub fn remove_completions(&mut self, completions: &[TerminalCompletion]) {
        let processed = completions
            .iter()
            .map(|completion| completion.event_id.as_str())
            .collect::<HashSet<_>>();
        for batch in &mut self.queued {
            // A batch can also contain a genuinely new execution of the same child.
            // Its delivery ID alone is not sufficient evidence to delete the batch.
            batch.messages.retain(|message| {
                !message
                    .terminal_completion
                    .as_ref()
                    .is_some_and(|completion| processed.contains(completion.event_id.as_str()))
            });
        }
        self.queued.retain(|batch| !batch.messages.is_empty());
        self.event_ids = self
            .queued
            .iter()
            .map(|batch| batch.event_id.clone())
            .collect();
    }

    pub fn is_empty(&self) -> bool {
        self.queued.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn queued(&self) -> &VecDeque<CompiledMessages> {
        &self.queued
    }

    pub fn drain(&mut self) -> impl Iterator<Item = CompiledMessages> + '_ {
        self.event_ids.clear();
        self.queued.drain(..)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cursor::conversation::MessageDelivery, model::RuntimeEvent};

    #[test]
    fn processed_cleanup_preserves_new_execution_and_unrelated_messages() {
        let completion = |execution: &str| TerminalCompletion {
            task_id: "same-child".into(),
            tool_call_id: execution.into(),
            kind: "subagent".into(),
            status: "success".into(),
            payload_digest: Some("digest".into()),
            event_id: format!("completion:{execution}"),
        };
        let old = completion("old-call");
        let new = completion("new-call");
        let notification = |completion: &TerminalCompletion| {
            let mut message = RuntimeEvent {
                event_id: completion.event_id.clone(),
                text: "result".into(),
            }
            .into_message();
            message.terminal_completion = Some(completion.clone());
            message
        };
        let old_message = notification(&old);
        let new_message = notification(&new);
        let unrelated = RuntimeEvent {
            event_id: "other-input".into(),
            text: "keep this input".into(),
        }
        .into_message();
        let batch = |event_id: String, messages| CompiledMessages {
            event_id,
            target_run_id: None,
            messages,
            delivery: MessageDelivery::InsertMessages,
        };
        let mut pending = PendingMessages::default();
        pending.push(batch(
            old.event_id.clone(),
            vec![old_message.clone(), new_message.clone()],
        ));
        pending.push(batch(
            "another-batch".into(),
            vec![old_message.clone(), unrelated.clone()],
        ));
        pending.push(batch("old-only".into(), vec![old_message]));
        pending.remove_completions(std::slice::from_ref(&old));
        assert_eq!(pending.queued.len(), 2);
        assert_eq!(
            pending.queued[0].messages,
            std::slice::from_ref(&new_message)
        );
        assert_eq!(pending.queued[1].messages, [unrelated]);
        assert!(!pending.event_ids.contains("old-only"));
        assert!(!pending.push(batch(old.event_id.clone(), vec![new_message])));
        pending.remove_completions(&[new]);
        assert_eq!(pending.queued.len(), 1);
        assert!(!pending.event_ids.contains(&old.event_id));
        assert_eq!(pending.drain().count(), 1);
        assert!(pending.is_empty());
        assert!(pending.event_ids.is_empty());
    }
}
