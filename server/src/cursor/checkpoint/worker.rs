//! Serializes checkpoint jobs and completes commit barriers.
use tokio::sync::{mpsc, oneshot};

use crate::{
    cursor::{
        checkpoint::PendingSteps, protocol::proto::agent::v1 as pb, transport::TransportHandle,
    },
    model::{CheckpointId, ToolRoundId},
    store::Store,
    Error, Result,
};

use super::{BuiltCheckpoint, CheckpointBuilder};

pub(crate) struct CheckpointJob {
    pub kind: CheckpointKind,
    pub presentation: PendingSteps,
    pub context_tokens: Option<u64>,
    pub ready: Option<oneshot::Sender<std::result::Result<(), String>>>,
}

pub(crate) enum CheckpointKind {
    Settled(CheckpointId),
    ToolStarted {
        round_id: ToolRoundId,
        stable_checkpoint_id: CheckpointId,
    },
    ToolSettled(CheckpointId),
    TodoUpdated {
        round_id: ToolRoundId,
        checkpoint_id: CheckpointId,
        todos: Vec<pb::TodoItem>,
    },
    Final {
        checkpoint_id: CheckpointId,
        result: oneshot::Sender<Result<FinalCheckpoints>>,
    },
    Compaction {
        checkpoint_id: CheckpointId,
        summary: String,
        result: oneshot::Sender<Result<BuiltCheckpoint>>,
    },
}

pub(crate) struct FinalCheckpoints {
    pub staged: BuiltCheckpoint,
    pub settled: BuiltCheckpoint,
}

pub(crate) struct CheckpointWorker {
    pub jobs: mpsc::Sender<CheckpointJob>,
    pub failures: mpsc::Receiver<Error>,
    task: tokio::task::JoinHandle<()>,
}

impl CheckpointWorker {
    pub fn spawn(
        store: Store,
        mut builder: CheckpointBuilder,
        handle: TransportHandle,
        mode: i32,
    ) -> Self {
        let (jobs, mut receiver) = mpsc::channel::<CheckpointJob>(32);
        let (failures, failure_receiver) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            let mut latest_state = None;
            while let Some(job) = receiver.recv().await {
                builder.record_context_tokens(job.context_tokens);
                let presentation = job.presentation;
                let ready = job.ready;
                let result = match job.kind {
                    CheckpointKind::Settled(checkpoint_id)
                    | CheckpointKind::ToolSettled(checkpoint_id) => publish_settled(
                        &store,
                        &mut builder,
                        &handle,
                        mode,
                        checkpoint_id,
                        &presentation,
                    )
                    .await
                    .map(|state| latest_state = Some(state)),
                    CheckpointKind::ToolStarted {
                        round_id,
                        stable_checkpoint_id,
                    } => publish_started(
                        &store,
                        &mut builder,
                        &handle,
                        mode,
                        round_id,
                        stable_checkpoint_id,
                        &presentation,
                    )
                    .await
                    .map(|state| latest_state = Some(state)),
                    CheckpointKind::TodoUpdated {
                        round_id,
                        checkpoint_id,
                        todos,
                    } => publish_todo_updated(
                        &store,
                        &mut builder,
                        &handle,
                        mode,
                        round_id,
                        checkpoint_id,
                        latest_state.as_ref(),
                        &todos,
                        &presentation,
                    )
                    .await
                    .map(|state| latest_state = Some(state)),
                    CheckpointKind::Final {
                        checkpoint_id,
                        result,
                    } => {
                        let checkpoints =
                            build_final(&store, &mut builder, mode, checkpoint_id, &presentation)
                                .await;
                        let _ = result.send(checkpoints);
                        Ok(())
                    }
                    CheckpointKind::Compaction {
                        checkpoint_id,
                        summary,
                        result,
                    } => {
                        let messages = store.load_checkpoint_messages(checkpoint_id).await;
                        let checkpoint = match messages {
                            Ok(messages) => {
                                builder
                                    .compacted(&messages, mode, &summary, &presentation)
                                    .await
                            }
                            Err(error) => Err(error),
                        };
                        let _ = result.send(checkpoint);
                        Ok(())
                    }
                };

                if let Err(error) = result {
                    if let Some(ready) = ready {
                        let _ = ready.send(Err(error.to_string()));
                    }
                    tracing::error!(%error, "failed to build or publish Cursor checkpoint");
                    let _ = failures.send(error).await;
                    break;
                }
                if let Some(ready) = ready {
                    let _ = ready.send(Ok(()));
                }
            }
        });
        Self {
            jobs,
            failures: failure_receiver,
            task,
        }
    }

    pub fn abort(&self) {
        self.task.abort();
    }
}

async fn publish_settled(
    store: &Store,
    builder: &mut CheckpointBuilder,
    handle: &TransportHandle,
    mode: i32,
    checkpoint_id: CheckpointId,
    presentation: &PendingSteps,
) -> Result<pb::ConversationStateStructure> {
    let messages = store.load_checkpoint_messages(checkpoint_id).await?;
    let checkpoint = builder.settled(&messages, mode, presentation).await?;
    builder.publish(handle, &checkpoint).await?;
    Ok(checkpoint.state)
}

async fn publish_started(
    store: &Store,
    builder: &mut CheckpointBuilder,
    handle: &TransportHandle,
    mode: i32,
    round_id: ToolRoundId,
    stable_checkpoint_id: CheckpointId,
    presentation: &PendingSteps,
) -> Result<pb::ConversationStateStructure> {
    let round = store
        .tool_round(&round_id)
        .await?
        .ok_or_else(|| Error::Store(format!("checkpoint tool round not found: {round_id}")))?;
    let messages = store.load_checkpoint_messages(stable_checkpoint_id).await?;
    let checkpoint = builder
        .staged_tool_round(
            &messages,
            mode,
            &round.assistant,
            &round.calls,
            round.created_at_ms,
            presentation,
        )
        .await?;
    builder.publish(handle, &checkpoint).await?;
    Ok(checkpoint.state)
}

async fn publish_todo_updated(
    store: &Store,
    builder: &mut CheckpointBuilder,
    handle: &TransportHandle,
    mode: i32,
    round_id: ToolRoundId,
    checkpoint_id: CheckpointId,
    current: Option<&pb::ConversationStateStructure>,
    todos: &[pb::TodoItem],
    presentation: &PendingSteps,
) -> Result<pb::ConversationStateStructure> {
    let current = current.ok_or_else(|| {
        Error::Protocol("TodoWrite completed before the active checkpoint was published".into())
    })?;
    let messages = store.load_checkpoint_messages(checkpoint_id).await?;
    let start = messages
        .iter()
        .position(|message| {
            matches!(
                &message.content,
                crate::model::MessageContent::Assistant {
                    tool_round_id: Some(candidate),
                    ..
                } if candidate == &round_id
            )
        })
        .ok_or_else(|| {
            Error::Protocol("TodoWrite commit is absent from canonical history".into())
        })?;
    let checkpoint = builder
        .todo_updated(current, todos, &messages[start..], mode, presentation)
        .await?;
    builder.publish(handle, &checkpoint).await?;
    Ok(checkpoint.state)
}

async fn build_final(
    store: &Store,
    builder: &mut CheckpointBuilder,
    mode: i32,
    checkpoint_id: CheckpointId,
    presentation: &PendingSteps,
) -> Result<FinalCheckpoints> {
    let messages = store.load_checkpoint_messages(checkpoint_id).await?;
    let (assistant, stable) = messages
        .split_last()
        .ok_or_else(|| Error::Store("final checkpoint contains no assistant".into()))?;
    let started_at_ms = crate::cursor::tools::runtime::now_ms();
    let staged = builder
        .staged_final(stable, mode, assistant, started_at_ms, presentation)
        .await?;
    let settled = builder
        .settled(&messages, mode, &PendingSteps::default())
        .await?;
    Ok(FinalCheckpoints { staged, settled })
}
