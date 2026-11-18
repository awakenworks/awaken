// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: An immutable request route is reclaimed only below a durable monotone cutoff.
use super::{
    Bytes, Error, ObjectStoreExt as _, Path, PutMode, PutOptions, Result, S3ComponentStore,
    SessionKey, SessionObject, digest, storage_error,
};
use awaken_time::WallClockExt as _;
use futures::{StreamExt as _, TryStreamExt as _};
use object_store::UpdateVersion;
use std::time::Duration;
use tokio::{sync::Mutex, time::Instant};

const BATCH: usize = 1_000;
const INTERVAL: Duration = Duration::from_secs(15);

#[derive(Default)]
pub(super) struct Maintenance(Mutex<Option<Instant>>);

impl S3ComponentStore {
    fn claim_routes_prefix(&self) -> Path {
        self.storage
            .inner
            .path(&format!("v1/routing/claim-requests/{}", self.component))
    }

    fn cutoff_path(&self) -> Path {
        self.storage
            .inner
            .path(&format!("v1/routing/claim-retention/{}", self.component))
    }

    pub(super) async fn claim_cutoff(&self) -> Result<(u64, Option<UpdateVersion>)> {
        let result = match self.storage.inner.store.get(&self.cutoff_path()).await {
            Ok(result) => result,
            Err(object_store::Error::NotFound { .. }) => return Ok((0, None)),
            Err(error) => return Err(storage_error(error)),
        };
        let version = UpdateVersion {
            e_tag: result.meta.e_tag.clone(),
            version: result.meta.version.clone(),
        };
        let cutoff = serde_json::from_slice(&result.bytes().await.map_err(storage_error)?)
            .map_err(crate::objects::corrupt)?;
        Ok((cutoff, Some(version)))
    }

    async fn claim_request_live(&self, expiry: u64) -> Result<bool> {
        if expiry <= self.storage.inner.clock.now_ms() {
            return Ok(false);
        }
        Ok(expiry > self.claim_cutoff().await?.0)
    }

    pub(super) async fn resolve_claim_request(
        &self,
        identity: &str,
        expiry: u64,
        proposed: Option<&SessionObject>,
    ) -> Result<Option<SessionObject>> {
        if !self.claim_request_live(expiry).await? {
            return Ok(None);
        }
        let path = self
            .claim_routes_prefix()
            .join(format!("{expiry:020}-{}", digest(identity.as_bytes())));
        let mut bound = self.read_route(&path).await?;
        // The previous engine published bindings in the immutable record
        // namespace. Keep recognizing them during the rollout.
        if bound.is_none() {
            bound = self.read_record_route(identity).await?;
        }
        if let (None, Some(proposed)) = (&bound, proposed) {
            let key = SessionKey::new(&proposed.owner_scope, &proposed.session_id)?;
            let body = Bytes::from(serde_json::to_vec(&key).map_err(crate::objects::corrupt)?);
            match self
                .storage
                .inner
                .store
                .put_opts(
                    &path,
                    body.into(),
                    PutOptions {
                        mode: PutMode::Create,
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(_) => bound = Some(key),
                Err(
                    object_store::Error::AlreadyExists { .. }
                    | object_store::Error::Precondition { .. },
                ) => {
                    bound = self.read_route(&path).await?;
                }
                Err(error) => return Err(Error::OutcomeUnknown(error.to_string())),
            }
        }
        // A publisher starting before retirement may still be opening the
        // route while the sweeper removes it. Recheck the persisted fence
        // after resolving/creating the binding before offering authority.
        if !self.claim_request_live(expiry).await? {
            return Ok(None);
        }
        match bound {
            Some(key) => self.snapshot(key).await,
            None => Ok(None),
        }
    }

    async fn advance_claim_cutoff(&self, now: u64) -> Result<u64> {
        loop {
            let (cutoff, version) = self.claim_cutoff().await?;
            if cutoff >= now {
                return Ok(cutoff);
            }
            let bytes = Bytes::from(serde_json::to_vec(&now).map_err(crate::objects::corrupt)?);
            match self
                .storage
                .inner
                .store
                .put_opts(
                    &self.cutoff_path(),
                    bytes.into(),
                    PutOptions {
                        mode: version.map_or(PutMode::Create, PutMode::Update),
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(_) => return Ok(now),
                Err(
                    object_store::Error::AlreadyExists { .. }
                    | object_store::Error::Precondition { .. },
                ) => {}
                Err(error) => return Err(Error::OutcomeUnknown(error.to_string())),
            }
        }
    }

    async fn sweep_claim_requests(&self) -> Result<bool> {
        let inner = &self.storage.inner;
        let cutoff = self.advance_claim_cutoff(inner.clock.now_ms()).await?;
        let prefix = self.claim_routes_prefix();
        let mut routes = inner.store.list(Some(&prefix));
        let mut expired = Vec::new();
        // S3 general-purpose and MinIO lists order by the full key. Stop as
        // soon as the next expiry is live, keeping the deletion batch bounded.
        for _ in 0..BATCH {
            let Some(route) = routes.try_next().await.map_err(storage_error)? else {
                break;
            };
            if !route.location.prefix_matches(&prefix) {
                return Err(Error::Corrupt("claim route escaped its namespace".into()));
            }
            let expiry = route
                .location
                .filename()
                .and_then(|file| file.split_once('-'))
                .and_then(|(expiry, _)| expiry.parse::<u64>().ok())
                .ok_or_else(|| Error::Corrupt("invalid claim route expiry".into()))?;
            if expiry > cutoff {
                break;
            }
            expired.push(route.location);
        }
        let saturated = expired.len() == BATCH;
        if !expired.is_empty() {
            inner
                .store
                .delete_stream(futures::stream::iter(expired.into_iter().map(Ok)).boxed())
                .try_collect::<Vec<_>>()
                .await
                .map_err(storage_error)?;
        }
        Ok(saturated)
    }

    pub(super) async fn maintain_claim_requests(&self) {
        let Ok(mut scheduled) = self.claim_routes.0.try_lock() else {
            return;
        };
        let now = awaken_time::mono::now();
        if scheduled.is_some_and(|next| next > now) {
            return;
        }
        *scheduled = Some(now + INTERVAL);
        match tokio::time::timeout(Duration::from_millis(250), self.sweep_claim_requests()).await {
            Ok(Ok(true)) => *scheduled = Some(now + Duration::from_millis(250)),
            Ok(Ok(false)) => (),
            Ok(Err(error)) => tracing::warn!(%error, "S3 claim route sweep will retry"),
            Err(_) => tracing::debug!("S3 claim route sweep reached its budget"),
        }
    }
}
