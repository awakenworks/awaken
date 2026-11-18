// @formal surface requirement=dispatch.durable_ingress.claim_and_delivery
// @formal reason: Shared identity and timestamp validation for replayable acquisitions.
use crate::DispatchError;
use awaken_dispatch_contract::ClaimRequest;

pub(crate) const CLAIM_RECEIPT_TTL_MS: i64 = 15 * 60 * 1_000;
pub(crate) const CLAIM_REQUEST_CLOCK_SKEW_MS: i64 = 60 * 1_000;

#[derive(Clone, Copy)]
pub(crate) struct ClaimReceiptRequest<'a> {
    pub(crate) request: &'a ClaimRequest,
    pub(crate) expires_at_ms: i64,
}

impl<'a> ClaimReceiptRequest<'a> {
    pub(crate) fn new(request: &'a ClaimRequest) -> Result<Self, DispatchError> {
        let invalid =
            || DispatchError::Rejected("claim request id must be a canonical UUIDv7".into());
        let id = uuid::Uuid::parse_str(&request.request_id).map_err(|_| invalid())?;
        if id.get_version_num() != 7 || id.to_string() != request.request_id {
            return Err(invalid());
        }
        let (seconds, nanos) = id.get_timestamp().ok_or_else(invalid)?.to_unix();
        let issued_at_ms =
            i64::try_from(seconds * 1_000 + u64::from(nanos / 1_000_000)).map_err(|_| invalid())?;
        Ok(Self {
            request,
            expires_at_ms: issued_at_ms + CLAIM_RECEIPT_TTL_MS,
        })
    }

    pub(crate) fn live_at(self, now_ms: u64) -> Result<bool, DispatchError> {
        let now = crate::clock::db_millis(now_ms);
        if self.expires_at_ms
            > now.saturating_add(CLAIM_RECEIPT_TTL_MS + CLAIM_REQUEST_CLOCK_SKEW_MS)
        {
            return Err(DispatchError::Rejected(
                "claim request timestamp is ahead of the store clock".into(),
            ));
        }
        Ok(self.expires_at_ms > now)
    }
}
