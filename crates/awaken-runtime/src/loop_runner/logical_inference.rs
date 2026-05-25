use std::sync::atomic::{AtomicU64, Ordering};

use awaken_contract::contract::executor::InferenceRequest;

static LOGICAL_INFERENCE_SEQ: AtomicU64 = AtomicU64::new(1);

pub(super) fn ensure_logical_inference_id(request: &mut InferenceRequest) {
    let routing_key = request.routing_key.get_or_insert_with(Default::default);
    if routing_key.logical_inference_id.is_none() {
        let id = LOGICAL_INFERENCE_SEQ.fetch_add(1, Ordering::Relaxed);
        routing_key.logical_inference_id = Some(format!("stream-{id}"));
    }
}
