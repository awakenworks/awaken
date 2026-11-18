// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Aggregates Dispatch candidates from scoped Session partitions and preserves selection order.
//! Portable scan state binds the engine identity, query and per-Session positions.
use super::{
    DispatchCompletion, DispatchControlReads, DispatchCursor, DispatchError,
    DispatchOperationalEvent, DispatchOperationalFeed, DispatchScanCursor, DispatchScanPage,
    ObjectDispatchPartition, ObjectDispatchStore, RunId,
};
use std::collections::{BTreeMap, VecDeque};

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Scan {
    format: u32,
    namespace: String,
    scope: String,
    kind: String,
    run: Option<String>,
    positions: BTreeMap<String, u64>,
    last: Option<String>,
}
trait LocalPosition {
    fn position(&self) -> u64;
}
impl LocalPosition for DispatchCompletion {
    fn position(&self) -> u64 {
        self.sequence
    }
}
impl LocalPosition for DispatchOperationalEvent {
    fn position(&self) -> u64 {
        self.cursor.0
    }
}
impl ObjectDispatchStore {
    async fn scan_partitions<'a, T, F>(
        &'a self,
        kind: &str,
        cursor: Option<&DispatchScanCursor>,
        limit: usize,
        run: Option<&RunId>,
        fetch: F,
    ) -> Result<DispatchScanPage<T>, DispatchError>
    where
        T: LocalPosition + Send + 'a,
        F: Fn(
                ObjectDispatchPartition,
                u64,
                usize,
            )
                -> futures_util::future::BoxFuture<'a, Result<(Vec<T>, u64), DispatchError>>
            + Send
            + Sync,
    {
        let namespace = self
            .objects
            .scan_namespace()
            .await
            .map_err(DispatchError::unavailable)?;
        let selected = run.map(|run| run.0.clone());
        let mut scan: Scan = match cursor {
            Some(cursor) => serde_json::from_str(cursor.token())
                .map_err(|_| DispatchError::Rejected("invalid partition scan cursor".into()))?,
            None => Scan {
                format: 1,
                namespace: namespace.clone(),
                scope: self.default_scope.clone(),
                kind: kind.into(),
                run: selected.clone(),
                positions: BTreeMap::new(),
                last: None,
            },
        };
        if scan.format != 1
            || scan.namespace != namespace
            || scan.scope != self.default_scope
            || scan.kind != kind
            || scan.run != selected
        {
            return Err(DispatchError::Rejected(
                "scan cursor belongs to another engine or query".into(),
            ));
        }
        let limit = limit.min(4096);
        if limit == 0 {
            return Ok(DispatchScanPage {
                events: Vec::new(),
                next_cursor: DispatchScanCursor::new(
                    serde_json::to_string(&scan).map_err(DispatchError::unavailable)?,
                )?,
            });
        }
        let mut objects = match run {
            Some(run) => self
                .objects
                .locate_record(&format!("run:{}", run.0))
                .await
                .map_err(DispatchError::unavailable)?
                .into_iter()
                .collect::<Vec<_>>(),
            None => self
                .objects
                .list()
                .await
                .map_err(DispatchError::unavailable)?,
        };
        objects
            .sort_by(|a, b| (&a.owner_scope, &a.session_id).cmp(&(&b.owner_scope, &b.session_id)));
        let mut pending = Vec::new();
        for object in objects {
            let id = serde_json::to_string(&(&object.owner_scope, &object.session_id))
                .map_err(DispatchError::unavailable)?;
            let after = scan.positions.get(&id).copied().unwrap_or(0);
            let (events, frontier) = fetch(self.partition(&object), after, limit).await?;
            if events.is_empty() {
                scan.positions.insert(id, frontier);
                continue;
            }
            pending.push((id, VecDeque::from(events), frontier));
        }
        // Resume after the last emitted partition, so one busy Session cannot
        // indefinitely hide other Sessions when callers request one event.
        if let Some(last) = &scan.last {
            let start = pending.iter().position(|(id, _, _)| id > last).unwrap_or(0);
            pending.rotate_left(start);
        }
        let mut events = Vec::new();
        while events.len() < limit {
            let mut advanced = false;
            for (id, queue, frontier) in &mut pending {
                let Some(event) = queue.pop_front() else {
                    continue;
                };
                scan.positions.insert(id.clone(), event.position());
                if queue.is_empty() {
                    scan.positions.insert(id.clone(), *frontier);
                }
                scan.last = Some(id.clone());
                events.push(event);
                advanced = true;
                if events.len() == limit {
                    break;
                }
            }
            if !advanced {
                break;
            }
        }
        Ok(DispatchScanPage {
            events,
            next_cursor: DispatchScanCursor::new(
                serde_json::to_string(&scan).map_err(DispatchError::unavailable)?,
            )?,
        })
    }
    pub(super) async fn scan_completion_partitions(
        &self,
        cursor: Option<&DispatchScanCursor>,
        limit: usize,
    ) -> Result<DispatchScanPage<DispatchCompletion>, DispatchError> {
        self.scan_partitions(
            "completions",
            cursor,
            limit,
            None,
            |partition, after, limit| {
                Box::pin(async move {
                    let events = partition.completion_events_after(after, limit).await?;
                    let frontier = events.last().map_or(after, |event| event.sequence);
                    Ok((events, frontier))
                })
            },
        )
        .await
    }
    pub(super) async fn scan_operation_partitions(
        &self,
        cursor: Option<&DispatchScanCursor>,
        limit: usize,
        run: Option<&RunId>,
    ) -> Result<DispatchScanPage<DispatchOperationalEvent>, DispatchError> {
        let selected = run.cloned();
        self.scan_partitions(
            "operations",
            cursor,
            limit,
            run,
            move |partition, after, limit| {
                let selected = selected.clone();
                Box::pin(async move {
                    let page = partition
                        .events_after(DispatchCursor(after), limit, selected.as_ref())
                        .await?;
                    Ok((page.events, page.next_cursor.0))
                })
            },
        )
        .await
    }
}
