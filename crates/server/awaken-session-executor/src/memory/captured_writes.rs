// @formal surface requirement=resource.memory.memory_consistency
// @formal reason: Static owner: the per-extraction in-memory WriteMemory tool sink and committed-message
// @formal reason: decoder split from the Host Memory orchestrator; durable Memory authority
// @formal reason: remains unchanged. Dynamic path: one auxiliary Agent run records accepted write_memory
// @formal reason: name/content pairs, the orchestrator takes that finite map, and only the existing
// @formal reason: extraction controller admits its durable mutations; malformed blocks and rejected
// @formal reason: content create no proposal. This requirement-only classification is not a proof of model
// @formal reason: behavior, concurrent tool execution, or repository durability.
use super::*;

#[derive(Default)]
pub(super) struct CapturedMemoryWrites {
    writes: Mutex<BTreeMap<String, String>>,
}

impl CapturedMemoryWrites {
    /// The writes captured from the auxiliary Run, unless the capture lock is
    /// poisoned (the map may be half-written).
    pub(super) fn take(&self) -> Result<BTreeMap<String, String>, crate::host_error::ExecutorLock> {
        Ok(std::mem::take(&mut *self.writes.lock().map_err(|_| {
            crate::host_error::ExecutorLock::CapturedMemoryWrites
        })?))
    }
}

pub(super) fn writes_from_committed_agent(messages: &[Message]) -> BTreeMap<String, String> {
    let mut writes = BTreeMap::new();
    for block in messages.iter().flat_map(|message| &message.content) {
        let ContentBlock::ToolUse { name, input, .. } = block else {
            continue;
        };
        if name != "write_memory" {
            continue;
        }
        let Some(name) = input.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(content) = input.get("content").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if accepts_memory_content(content) {
            writes.insert(format!("/{}.md", sanitize_stem(name)), content.to_string());
        }
    }
    writes
}

#[async_trait]
impl MemoryStoreHandle for CapturedMemoryWrites {
    async fn write(&self, name: &str, content: &str) -> Result<String, String> {
        let path = format!("/{}.md", sanitize_stem(name));
        self.writes
            .lock()
            .map_err(|error| error.to_string())?
            .insert(path.clone(), content.to_string());
        Ok(path)
    }

    async fn entries(&self) -> Result<Vec<awaken_ext_memory::Entry>, String> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poisoned_capture_is_refused_so_extraction_reads_its_committed_transcript() {
        // Row X3: `take` refuses; the extraction then decodes the committed
        // auxiliary transcript (rows X1/X2 live with the extraction tests).
        let capture = CapturedMemoryWrites::default();
        crate::host_error::poison_for_test(&capture.writes);
        assert_eq!(
            capture.take().unwrap_err(),
            crate::host_error::ExecutorLock::CapturedMemoryWrites
        );
    }
}
