//! Records Cursor request traces without blocking request or runtime paths.

mod event;
mod worker;

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc,
    },
};

use bytes::Bytes;
use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::store::{BlobId, Store};

use event::{TraceEvent, TRACE_DISABLED, TRACE_UNKNOWN};

/// 上限只用于回收已没有 recorder 持有的异常终止 request。
const MAX_TRACKED_TRACES: usize = 256;

type Activations = Arc<Mutex<HashMap<Arc<str>, Arc<AtomicU8>>>>;

/// A decoded view of one captured client message, persisted next to its raw payload.
#[derive(Clone)]
pub struct DecodedMessage {
    pub data: Bytes,
    pub metadata: serde_json::Value,
}

#[derive(Clone)]
pub struct CursorTraceService {
    sender: mpsc::UnboundedSender<TraceEvent>,
    activations: Activations,
}

impl CursorTraceService {
    pub fn new(store: Store) -> Self {
        // 详细记录必须完整且不能阻塞通信路径。单一后台 worker 保持落库顺序;
        // 显式开启详细记录时,由其负责吸收 SQLite 的短暂写入延迟。
        let (sender, receiver) = mpsc::unbounded_channel();
        let activations: Activations = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(worker::run(store, receiver, activations.clone()));
        Self {
            sender,
            activations,
        }
    }

    pub fn recorder(&self, request_id: &str) -> CursorTraceRecorder {
        CursorTraceRecorder {
            request_id: Arc::from(request_id),
            sender: self.sender.clone(),
            finished: Arc::new(AtomicBool::new(false)),
            activation: self.activation(request_id),
        }
    }

    /// 同一 request 的所有 recorder 共享一个激活单元。后台确认当前请求
    /// 是否记录前保持 UNKNOWN，避免启动或设置变更时丢弃首批事件。
    fn activation(&self, request_id: &str) -> Arc<AtomicU8> {
        let mut activations = self.activations.lock();
        if let Some(activation) = activations.get(request_id) {
            return activation.clone();
        }
        if activations.len() >= MAX_TRACKED_TRACES {
            activations.retain(|_, activation| Arc::strong_count(activation) > 1);
        }
        let request_id: Arc<str> = Arc::from(request_id);
        let activation = Arc::new(AtomicU8::new(TRACE_UNKNOWN));
        activations.insert(request_id, activation.clone());
        activation
    }
}

#[derive(Clone)]
pub struct CursorTraceRecorder {
    request_id: Arc<str>,
    sender: mpsc::UnboundedSender<TraceEvent>,
    finished: Arc<AtomicBool>,
    activation: Arc<AtomicU8>,
}

impl CursorTraceRecorder {
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn begin(&self, conversation_id: Option<&str>, route: &str, model_id: Option<&str>) {
        self.send_control(TraceEvent::Begin {
            request_id: self.request_id.to_string(),
            activation: self.activation.clone(),
            conversation_id: conversation_id.map(str::to_owned),
            route: route.to_owned(),
            model_id: model_id.map(str::to_owned),
        });
    }

    pub fn resume(&self) {
        self.send_control(TraceEvent::Resume {
            request_id: self.request_id.to_string(),
            activation: self.activation.clone(),
        });
    }

    /// Sends one Bidi append together with its optional decoded view. Both are
    /// persisted adjacently so a trace reads as raw payload + decoded message.
    pub fn bidi_append(
        &self,
        data: Bytes,
        metadata: serde_json::Value,
        decoded: Option<DecodedMessage>,
    ) {
        self.send(TraceEvent::Request {
            request_id: self.request_id.to_string(),
            artifact_type: "bidi_request".into(),
            data,
            decoded,
            metadata,
        });
    }

    pub fn is_enabled(&self) -> bool {
        self.activation.load(Ordering::Acquire) != TRACE_DISABLED
    }

    pub fn artifact(
        &self,
        artifact_type: &str,
        source: &str,
        data: &[u8],
        metadata: serde_json::Value,
    ) {
        self.send(TraceEvent::Artifact {
            request_id: self.request_id.to_string(),
            artifact_type: artifact_type.to_owned(),
            source: source.to_owned(),
            data: Bytes::copy_from_slice(data),
            metadata,
        });
    }

    pub fn linked_blob(
        &self,
        artifact_type: &str,
        source: &str,
        blob_id: &BlobId,
        metadata: serde_json::Value,
    ) {
        self.send(TraceEvent::LinkedBlob {
            request_id: self.request_id.to_string(),
            artifact_type: artifact_type.to_owned(),
            source: source.to_owned(),
            blob_id: blob_id.clone(),
            metadata,
        });
    }

    pub fn response_started(&self, status: u16) {
        self.send(TraceEvent::ResponseStarted {
            request_id: self.request_id.to_string(),
            status,
        });
    }

    pub fn response_chunk(&self, source: &str, data: Bytes) {
        if self.finished.load(Ordering::Acquire) {
            return;
        }
        self.send(TraceEvent::ResponseChunk {
            request_id: self.request_id.to_string(),
            source: source.to_owned(),
            data,
        });
    }

    pub fn finish(&self, error: Option<&str>) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        self.send_control(TraceEvent::Finish {
            request_id: self.request_id.to_string(),
            error: error.map(str::to_owned),
        });
    }

    fn send(&self, event: TraceEvent) {
        if self.activation.load(Ordering::Acquire) == TRACE_DISABLED {
            return;
        }
        self.send_control(event);
    }

    fn send_control(&self, event: TraceEvent) {
        if self.sender.send(event).is_err() {
            tracing::warn!(
                request_id = %self.request_id,
                "Cursor trace worker stopped before accepting an event"
            );
        }
    }
}
