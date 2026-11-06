// @formal surface requirement=credential.lifecycle.credential_lifecycle
// @formal reason: Control realizes admitted token profiles and claims the existing material WAL before external dispatch.
//! Credential-owned request realization and sealed publication. It supplies IO
//! to the shared coordinator; neither request nor response logic names a vendor.
use super::{
    CredentialRefreshAccess, CredentialSourceId, CredentialStatus, OAUTH_CLIENT_SECRET_SLOT,
    OAUTH_REFRESH_TIMEOUT, OAUTH_REFRESH_TOKEN_SLOT, ProbeHttpResponse, TokenEndpointAuth,
    VaultRefreshFactory, basic_client_auth, http_client_for,
};
use awaken_agent_contract::RedactedString;
use awaken_credential_contract::{
    AcquiredCredentialToken, CredentialTokenExchangeProfile, CredentialTokenIssuer,
    HttpEffectMaterialReference, IssuedCredentialTokens, RefreshTokenPolicy, TokenAcquisitionError,
    TokenClientBasicAuth, TokenEndpointResponse, TokenRequestEncoding, TokenResponseCondition,
    TokenResponseSpec, TokenSourceField,
};
use awaken_credential_vault::repo::{
    CredentialAcquisitionPhase, CredentialMutationIntent, abandon_token_publication,
    claim_token_publication, publish_acquired_tokens, recover_token_publication,
    token_publication_state,
};
use awaken_credential_vault::{CredentialSource, decode_structured_material};
use awaken_sync::LeafLock;
use awaken_time::WallClockExt as _;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(super) fn standard_profile(access: &CredentialRefreshAccess) -> CredentialTokenExchangeProfile {
    let mut parameters = serde_json::Map::from_iter([
        ("grant_type".into(), json!("refresh_token")),
        (
            "refresh_token".into(),
            json!({"credential":"oauth","field":"refresh"}),
        ),
    ]);
    let mut fields = BTreeMap::from([("refresh".into(), TokenSourceField::RefreshToken {})]);
    if access.token_endpoint_auth == TokenEndpointAuth::ClientSecretPost {
        fields.insert("secret".into(), TokenSourceField::ClientSecret {});
        parameters.insert(
            "client_secret".into(),
            json!({"credential":"oauth","field":"secret"}),
        );
    }
    if access.token_endpoint_auth != TokenEndpointAuth::ClientSecretBasic {
        parameters.insert("client_id".into(), json!(access.client_id));
    }
    if let Some(scope) = access.scope.as_ref() {
        parameters.insert("scope".into(), json!(scope));
    }
    if let Some(resource) = access.resource.as_ref() {
        parameters.insert("resource".into(), json!(resource));
    }
    CredentialTokenExchangeProfile {
        method: awaken_credential_contract::TokenRequestMethod::Post,
        version: 1,
        token_endpoint: access.token_endpoint.clone(),
        encoding: TokenRequestEncoding::Form,
        parameters: Value::Object(parameters),
        headers: BTreeMap::new(),
        bindings: BTreeMap::from([("oauth".into(), fields)]),
        basic_auth: (access.token_endpoint_auth == TokenEndpointAuth::ClientSecretBasic).then(
            || TokenClientBasicAuth {
                client_id: access.client_id.clone(),
                secret_slot: OAUTH_CLIENT_SECRET_SLOT.into(),
            },
        ),
        access_field: None,
        response: TokenResponseSpec {
            access_ttl_unit: awaken_credential_contract::TokenExpiryUnit::Seconds,
            additional_fields: Default::default(),
            access_token_pointer: "/access_token".into(),
            access_ttl_pointer: Some("/expires_in".into()),
            require_access_ttl: false,
            refresh_token_pointer: Some("/refresh_token".into()),
            refresh_ttl_pointer: Some("/refresh_token_expires_in".into()),
            require_refresh_ttl: false,
            refresh_policy: RefreshTokenPolicy::PreserveIfAbsent,
            success: vec![],
            authorization_required: vec![TokenResponseCondition {
                pointer: "/error".into(),
                equals: json!("invalid_grant"),
            }],
            max_access_ttl_seconds: 31_622_400,
            max_refresh_ttl_seconds: 31_622_400,
            refresh_margin_ms: 0,
        },
    }
}

pub(super) struct VaultAcquisitionPort<'a> {
    owner: &'a VaultRefreshFactory,
    pub(super) source: tokio::sync::Mutex<CredentialSource>,
    original: CredentialSource,
    replacement: Option<CredentialSourceId>,
    profile: CredentialTokenExchangeProfile,
    fingerprint: String,
    intent: tokio::sync::Mutex<Option<CredentialMutationIntent>>,
    probe: LeafLock<Option<ProbeHttpResponse>>,
    http: reqwest::Client,
    network_started: std::sync::atomic::AtomicBool,
}
impl<'a> VaultAcquisitionPort<'a> {
    pub(super) fn new(
        owner: &'a VaultRefreshFactory,
        source: CredentialSource,
        profile: CredentialTokenExchangeProfile,
        replacement: Option<CredentialSourceId>,
    ) -> Result<Self, TokenAcquisitionError> {
        let fingerprint = profile.fingerprint()?;
        if profile
            .auxiliary_slots()
            .iter()
            .any(|slot| source.auxiliary_material_ref(slot).is_none())
        {
            return Err(TokenAcquisitionError::AuthorityDenied);
        }
        let http = http_client_for(&profile.token_endpoint);
        Ok(Self {
            owner,
            source: tokio::sync::Mutex::new(source.clone()),
            original: source,
            replacement,
            profile,
            fingerprint,
            intent: tokio::sync::Mutex::new(None),
            probe: LeafLock::new(None),
            http,
            network_started: std::sync::atomic::AtomicBool::new(false),
        })
    }
    pub(super) fn probe(&self) -> Option<ProbeHttpResponse> {
        self.probe.lock().clone()
    }
    pub(super) async fn access_token(
        &self,
        source: &CredentialSource,
    ) -> Result<RedactedString, TokenAcquisitionError> {
        let value = self
            .owner
            .secrets
            .get(
                source
                    .material_ref
                    .as_ref()
                    .ok_or(TokenAcquisitionError::AuthorityDenied)?,
            )
            .await
            .map_err(|_| TokenAcquisitionError::AuthorityDenied)?;
        match decode_structured_material(value)
            .map_err(|_| TokenAcquisitionError::AuthorityDenied)?
        {
            Err(scalar) => Ok(scalar),
            Ok(mut fields) => fields
                .fields
                .remove(
                    self.profile
                        .access_field
                        .as_deref()
                        .ok_or(TokenAcquisitionError::InvalidContract)?,
                )
                .ok_or(TokenAcquisitionError::AuthorityDenied),
        }
    }
    async fn material(
        &self,
        source: &CredentialSource,
        slot: &TokenSourceField,
    ) -> Result<RedactedString, TokenAcquisitionError> {
        match slot {
            TokenSourceField::AccessToken {} => self.access_token(source).await,
            TokenSourceField::RefreshToken {} => self
                .owner
                .secrets
                .get(
                    source
                        .auxiliary_material_ref(OAUTH_REFRESH_TOKEN_SLOT)
                        .ok_or(TokenAcquisitionError::AuthorityDenied)?,
                )
                .await
                .map_err(|_| TokenAcquisitionError::AuthorityDenied),
            TokenSourceField::ClientSecret {} => self
                .owner
                .secrets
                .get(
                    source
                        .auxiliary_material_ref(OAUTH_CLIENT_SECRET_SLOT)
                        .ok_or(TokenAcquisitionError::AuthorityDenied)?,
                )
                .await
                .map_err(|_| TokenAcquisitionError::AuthorityDenied),
            TokenSourceField::Auxiliary { slot } => self
                .owner
                .secrets
                .get(
                    source
                        .auxiliary_material_ref(slot)
                        .ok_or(TokenAcquisitionError::AuthorityDenied)?,
                )
                .await
                .map_err(|_| TokenAcquisitionError::AuthorityDenied),
        }
    }
    async fn recovered(&self) -> Result<Option<AcquiredCredentialToken>, TokenAcquisitionError> {
        let id = self.replacement.as_ref().unwrap_or(&self.original.id);
        let current = match self.owner.credentials.get(id).await {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        let expected = awaken_agent_contract::credential::CredentialRef {
            id: self.original.id.0.clone(),
            revision: u64::try_from(self.original.version)
                .map_err(|_| TokenAcquisitionError::AuthorityDenied)?,
        };
        let state = token_publication_state(&current, self.owner.secrets.as_ref())
            .await
            .map_err(|_| TokenAcquisitionError::AuthorityDenied)?;
        let Some(state) = state else {
            return Ok(None);
        };
        if current.status != CredentialStatus::Active
            || current.workspace_id != self.original.workspace_id
            || current.descriptor != self.original.descriptor
            || state.predecessor.as_ref() != Some(&expected)
            || state.profile_fingerprint != self.fingerprint
        {
            return Ok(None);
        }
        let token = self.access_token(&current).await?;
        *self.source.lock().await = current;
        Ok(Some(AcquiredCredentialToken {
            additional_fields: Default::default(),
            access_token: token,
            expires_at_unix_ms: state.access_expires_at_unix_ms,
            reference: Some(state.successor),
            cache_generation: None,
            cache_key: None,
            cache_partition: None,
        }))
    }
}

#[async_trait::async_trait]
impl CredentialTokenIssuer for VaultAcquisitionPort<'_> {
    async fn authorize(&self) -> Result<(), TokenAcquisitionError> {
        let expected = self.source.lock().await.clone();
        let actual = self
            .owner
            .credentials
            .get(&expected.id)
            .await
            .map_err(|_| TokenAcquisitionError::AuthorityDenied)?;
        if actual != expected {
            if self.recovered().await?.is_some() {
                return Ok(());
            }
            return Err(TokenAcquisitionError::AuthorityDenied);
        }
        if actual.status != CredentialStatus::Active {
            return Err(TokenAcquisitionError::AuthorityDenied);
        }
        if self.profile.response.refresh_policy != RefreshTokenPolicy::NotExpected
            && let Some(state) = token_publication_state(&actual, self.owner.secrets.as_ref())
                .await
                .map_err(|_| TokenAcquisitionError::AuthorityDenied)?
            && state
                .refresh_expires_at_unix_ms
                .is_some_and(|at| at <= self.owner.clock.now_ms())
        {
            return Err(TokenAcquisitionError::NeedsAuthorization);
        }
        Ok(())
    }
    async fn begin(&self) -> Result<Option<AcquiredCredentialToken>, TokenAcquisitionError> {
        if let Some(token) = self.recovered().await? {
            return Ok(Some(token));
        }
        let attempt=tokio::time::timeout(OAUTH_REFRESH_TIMEOUT,async {
            loop {
                match claim_token_publication(&self.original,self.replacement.clone(),self.fingerprint.clone(),self.profile.response.refresh_policy!=RefreshTokenPolicy::NotExpected,self.owner.credentials.as_ref(),self.owner.clock.as_ref()).await {
                    Ok(intent)=>{*self.intent.lock().await=Some(intent);return Ok(None);},
                    Err(_)=>{
                        if let Some(token)=self.recovered().await? {return Ok(Some(token));}
                        let id=self.replacement.as_ref().unwrap_or(&self.original.id);
                        let pending=self.owner.credentials.pending_mutation(id).await.map_err(|_|TokenAcquisitionError::InProgress)?;
                        let Some(pending)=pending else {return Err(TokenAcquisitionError::AuthorityDenied);};
                        if pending.acquisition.as_ref().is_none_or(|v|v.profile_fingerprint!=self.fingerprint) {return Err(TokenAcquisitionError::AuthorityDenied);}
                        let phase=pending.acquisition.as_ref().expect("checked acquisition").phase;
                        if matches!(phase,CredentialAcquisitionPhase::OutcomeUnknown|CredentialAcquisitionPhase::Rejected)
                            &&pending.material_fence.phase==awaken_credential_vault::repo::CredentialMaterialMutationPhase::Writing {
                            if phase==CredentialAcquisitionPhase::Rejected {return Err(TokenAcquisitionError::NeedsAuthorization);}
                            return Err(TokenAcquisitionError::OutcomeUnknown);
                        }
                        let _recovery=recover_token_publication(&pending,self.owner.secrets.as_ref(),self.owner.credentials.as_ref(),self.owner.clock.as_ref()).await;
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                }
            }
        }).await;
        attempt.unwrap_or(Err(TokenAcquisitionError::InProgress))
    }
    async fn exchange(&self) -> Result<TokenEndpointResponse, TokenAcquisitionError> {
        self.authorize().await?;
        let source = self.source.lock().await.clone();
        let mut fields = BTreeMap::new();
        let mut scrub = Vec::new();
        for (role, bindings) in &self.profile.bindings {
            for (name, slot) in bindings {
                let value = self.material(&source, slot).await?;
                scrub.push(value.expose_secret().to_owned());
                fields.insert((role.clone(), name.clone()), value);
            }
        }
        let parameters = render(&self.profile.parameters, &fields)?;
        let mut request = self.http.request(
            if self.profile.method == awaken_credential_contract::TokenRequestMethod::Get {
                reqwest::Method::GET
            } else {
                reqwest::Method::POST
            },
            &self.profile.token_endpoint,
        );
        for (name, value) in &self.profile.headers {
            let value = render(value, &fields)?;
            request = request.header(
                name,
                value
                    .as_str()
                    .ok_or(TokenAcquisitionError::InvalidContract)?,
            );
        }
        if let Some(auth) = self.profile.basic_auth.as_ref() {
            let secret = self
                .owner
                .secrets
                .get(
                    source
                        .auxiliary_material_ref(&auth.secret_slot)
                        .ok_or(TokenAcquisitionError::AuthorityDenied)?,
                )
                .await
                .map_err(|_| TokenAcquisitionError::AuthorityDenied)?;
            scrub.push(secret.expose_secret().to_owned());
            request = request.header(
                reqwest::header::AUTHORIZATION,
                basic_client_auth(&auth.client_id, secret.expose_secret()),
            );
        }
        request = match self.profile.encoding {
            TokenRequestEncoding::Json => request.json(&parameters),
            TokenRequestEncoding::Form => request.form(&scalar_parameters(&parameters)?),
            TokenRequestEncoding::Query => request.query(&scalar_parameters(&parameters)?),
        };
        let request = request
            .build()
            .map_err(|_| TokenAcquisitionError::InvalidContract)?;
        self.authorize().await?;
        let mut intent = self.intent.lock().await;
        let dispatched = self
            .owner
            .credentials
            .transition_acquisition(
                intent
                    .as_ref()
                    .ok_or(TokenAcquisitionError::PublicationFailed)?,
                CredentialAcquisitionPhase::Dispatched,
                self.owner.clock.now_ms(),
            )
            .await
            .map_err(|_| TokenAcquisitionError::AuthorityDenied)?;
        *intent = Some(dispatched);
        drop(intent);
        self.network_started
            .store(true, std::sync::atomic::Ordering::Release);
        let mut response = self.http.execute(request).await.map_err(|e| {
            if e.is_connect() {
                TokenAcquisitionError::IssuerUnavailable
            } else {
                TokenAcquisitionError::OutcomeUnknown
            }
        })?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| TokenAcquisitionError::OutcomeUnknown)?
        {
            if bytes.len().saturating_add(chunk.len()) > 1_048_576 {
                return Err(TokenAcquisitionError::OutcomeUnknown);
            }
            bytes.extend_from_slice(&chunk);
        }
        let body: Value =
            serde_json::from_slice(&bytes).map_err(|_| TokenAcquisitionError::OutcomeUnknown)?;
        for pointer in std::iter::once(&self.profile.response.access_token_pointer)
            .chain(self.profile.response.refresh_token_pointer.iter())
        {
            if let Some(token) = body.pointer(pointer).and_then(Value::as_str) {
                scrub.push(token.to_owned());
            }
        }
        let refs = scrub.iter().map(String::as_str).collect::<Vec<_>>();
        *self.probe.lock() = Some(ProbeHttpResponse::scrubbed(
            status,
            content_type.as_deref(),
            &String::from_utf8_lossy(&bytes),
            false,
            &refs,
        ));
        Ok(TokenEndpointResponse { status, body })
    }
    async fn publish(
        &self,
        tokens: IssuedCredentialTokens,
    ) -> Result<AcquiredCredentialToken, TokenAcquisitionError> {
        let intent = self
            .intent
            .lock()
            .await
            .clone()
            .ok_or(TokenAcquisitionError::PublicationFailed)?;
        match publish_acquired_tokens(
            intent,
            tokens,
            self.profile.access_field.as_deref(),
            self.owner.secrets.as_ref(),
            self.owner.credentials.as_ref(),
            self.owner.clock.as_ref(),
        )
        .await
        {
            Ok((source, token)) => {
                *self.source.lock().await = source;
                Ok(token)
            }
            Err(_) => self
                .recovered()
                .await?
                .ok_or(TokenAcquisitionError::PublicationFailed),
        }
    }
    async fn failed(&self, error: TokenAcquisitionError) -> TokenAcquisitionError {
        let mut intent = self.intent.lock().await;
        let Some(old) = intent.as_ref() else {
            return error;
        };
        let phase = if !self
            .network_started
            .load(std::sync::atomic::Ordering::Acquire)
        {
            CredentialAcquisitionPhase::ConfirmedUnsent
        } else {
            match error {
                TokenAcquisitionError::IssuerUnavailable => {
                    CredentialAcquisitionPhase::ConfirmedUnsent
                }
                TokenAcquisitionError::NeedsAuthorization
                | TokenAcquisitionError::IssuerRejected => CredentialAcquisitionPhase::Rejected,
                _ => CredentialAcquisitionPhase::OutcomeUnknown,
            }
        };
        if let Ok(next) = self
            .owner
            .credentials
            .transition_acquisition(old, phase, self.owner.clock.now_ms())
            .await
        {
            if phase == CredentialAcquisitionPhase::ConfirmedUnsent {
                let _abort = abandon_token_publication(
                    &next,
                    self.owner.secrets.as_ref(),
                    self.owner.credentials.as_ref(),
                )
                .await;
            }
            *intent = Some(next);
        }
        error
    }
    fn durable_publication(&self) -> bool {
        true
    }
}

fn render(
    value: &Value,
    fields: &BTreeMap<(String, String), RedactedString>,
) -> Result<Value, TokenAcquisitionError> {
    if let Some(reference) = HttpEffectMaterialReference::from_json_value(value)
        .map_err(|_| TokenAcquisitionError::InvalidContract)?
    {
        let secret = fields
            .get(&(
                reference.credential().to_owned(),
                reference.field().to_owned(),
            ))
            .ok_or(TokenAcquisitionError::AuthorityDenied)?;
        return Ok(Value::String(reference.render(secret.expose_secret())));
    }
    match value {
        Value::Object(values) => values
            .iter()
            .map(|(k, v)| Ok((k.clone(), render(v, fields)?)))
            .collect::<Result<serde_json::Map<_, _>, _>>()
            .map(Value::Object),
        Value::Array(values) => values
            .iter()
            .map(|v| render(v, fields))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        v => Ok(v.clone()),
    }
}
fn scalar_parameters(value: &Value) -> Result<Vec<(String, String)>, TokenAcquisitionError> {
    value
        .as_object()
        .ok_or(TokenAcquisitionError::InvalidContract)?
        .iter()
        .map(|(k, v)| {
            let text = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => return Err(TokenAcquisitionError::InvalidContract),
            };
            Ok((k.clone(), text))
        })
        .collect()
}
