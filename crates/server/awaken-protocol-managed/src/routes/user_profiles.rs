//! Managed User Profiles HTTP adapter over the Control-owned Data Subject application.

use std::collections::BTreeMap;
use std::sync::Arc;

use awaken_data_subject_application::{
    CreateUserProfileCommand, DataSubjectApplication, DataSubjectApplicationError,
    EnrollmentTicket, ExternalAccountStatus, ExternalEntityType, ExternalUserDetails,
    ExternalUserDetailsUpdate, UpdateUserProfileCommand, UserProfileFieldUpdate, UserProfileRecord,
    UserProfileRelationship, UserProfileTrustGrant, UserProfileTrustGrantStatus,
};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::common::headers::{ManagedCapability, has_capability};
use crate::routes::{ManagedJson, ManagedQuery};
use crate::types::user_profile::{
    AccessType, AccountStatus, CurrentUserProfile, EnrollmentUrl, EntityType,
    ExternalUserDetails as WireExternalUserDetails, ExternalUserDetailsParams, LegacyUserProfile,
    Relationship, TrustGrant, TrustGrantStatus, UserProfile, UserProfileCore,
    UserProfileCreateParams, UserProfileUpdateParams,
};
use crate::types::{ErrorResponse, PageCursor, PageQuery, paginate};

struct UserProfileHttpState {
    application: Arc<DataSubjectApplication>,
    org: String,
}

/// Mount the User Profiles wire adapter over the one Data Subject application.
pub fn user_profiles_router(application: Arc<DataSubjectApplication>, org: String) -> Router {
    Router::new()
        .route("/v1/user_profiles", post(create_profile).get(list_profiles))
        .route(
            "/v1/user_profiles/{id}",
            get(retrieve_profile).post(update_profile),
        )
        .route(
            "/v1/user_profiles/{id}/enrollment_url",
            post(enrollment_url),
        )
        .with_state(Arc::new(UserProfileHttpState { application, org }))
}

type WireError = (StatusCode, Json<ErrorResponse>);

fn error_response(error: DataSubjectApplicationError) -> WireError {
    let (status, kind, message) = match error {
        DataSubjectApplicationError::NotFound => (
            StatusCode::NOT_FOUND,
            "not_found_error",
            "user_profile not found".to_string(),
        ),
        DataSubjectApplicationError::InvalidEnrollment => (
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            error.to_string(),
        ),
        DataSubjectApplicationError::Conflict => {
            (StatusCode::CONFLICT, "conflict_error", error.to_string())
        }
        DataSubjectApplicationError::WeakEnrollmentKey
        | DataSubjectApplicationError::Erasure(_)
        | DataSubjectApplicationError::Repository(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
            error.to_string(),
        ),
    };
    (status, Json(ErrorResponse::new(kind, message)))
}

fn bad_request(message: impl Into<String>) -> WireError {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse::new("invalid_request_error", message)),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UserProfileProjection {
    Legacy,
    Current,
    /// `user-profiles-2026-09-04`: the current vocabulary plus the
    /// platform-stated `external_user_details`.
    External,
}

fn user_profile_projection(
    legacy: bool,
    current: bool,
    external: bool,
) -> Option<UserProfileProjection> {
    // The legacy vocabulary (03-24) is exclusive: it names a different access
    // model. The current family is cumulative: an official client sends its
    // default 08-18 selector beside the 09-04 opt-in on one header, and the
    // newer selector decides the projection.
    match (legacy, current, external) {
        (true, false, false) => Some(UserProfileProjection::Legacy),
        (false, true, false) => Some(UserProfileProjection::Current),
        (false, _, true) => Some(UserProfileProjection::External),
        _ => None,
    }
}

fn selected_projection(headers: &HeaderMap) -> Result<UserProfileProjection, WireError> {
    let legacy = has_capability(headers, ManagedCapability::UserProfilesLegacy);
    let current = has_capability(headers, ManagedCapability::UserProfilesCurrent);
    let external = has_capability(headers, ManagedCapability::UserProfilesExternal);
    match user_profile_projection(legacy, current, external) {
        Some(projection) => Ok(projection),
        None if legacy && (current || external) => Err(bad_request(
            "legacy and current User Profiles beta capabilities cannot be combined",
        )),
        None => Err(bad_request("a User Profiles beta capability is required")),
    }
}

/// The fields a request may carry under each projection. `access_type` is
/// current-or-newer vocabulary; `external_user_details` exists only under the
/// external projection. Legacy carries neither.
fn user_profile_field_set_is_valid(
    projection: UserProfileProjection,
    access_type_is_present: bool,
    external_user_details_is_present: bool,
) -> bool {
    match projection {
        UserProfileProjection::Legacy => {
            !access_type_is_present && !external_user_details_is_present
        }
        UserProfileProjection::Current => !external_user_details_is_present,
        UserProfileProjection::External => true,
    }
}

fn reject_current_only_field(
    projection: UserProfileProjection,
    access_type_is_present: bool,
    external_user_details_is_present: bool,
) -> Result<(), WireError> {
    if user_profile_field_set_is_valid(
        projection,
        access_type_is_present,
        external_user_details_is_present,
    ) {
        Ok(())
    } else if external_user_details_is_present && projection != UserProfileProjection::External {
        Err(bad_request(
            "external_user_details requires the user-profiles-2026-09-04 beta capability",
        ))
    } else {
        Err(bad_request(
            "access_type requires the user-profiles-2026-08-18 beta capability",
        ))
    }
}

#[cfg(kani)]
#[kani::proof]
fn user_profile_capability_and_field_projection_is_total_exclusive_and_exact() {
    // Exactly one selector chooses a projection; none or several choose
    // nothing. The field set admitted by each projection is monotone in the
    // selector's age: legacy admits neither newer field, current admits
    // access_type, external admits both.
    let legacy: bool = kani::any();
    let current: bool = kani::any();
    let external: bool = kani::any();
    let access_type_is_present: bool = kani::any();
    let external_user_details_is_present: bool = kani::any();
    let projection = user_profile_projection(legacy, current, external);
    // Legacy is exclusive; the current family (08-18, 09-04) is cumulative
    // with the newest selector deciding; nothing selected chooses nothing.
    assert_eq!(
        projection.is_some(),
        (legacy && !current && !external) || (!legacy && (current || external)),
    );
    match projection {
        None => {}
        Some(UserProfileProjection::Legacy) => {
            assert!(legacy && !current && !external);
            assert_eq!(
                user_profile_field_set_is_valid(
                    UserProfileProjection::Legacy,
                    access_type_is_present,
                    external_user_details_is_present,
                ),
                !access_type_is_present && !external_user_details_is_present,
            );
        }
        Some(UserProfileProjection::Current) => {
            assert!(current && !legacy && !external);
            assert_eq!(
                user_profile_field_set_is_valid(
                    UserProfileProjection::Current,
                    access_type_is_present,
                    external_user_details_is_present,
                ),
                !external_user_details_is_present,
            );
        }
        Some(UserProfileProjection::External) => {
            assert!(external && !legacy);
            assert!(user_profile_field_set_is_valid(
                UserProfileProjection::External,
                access_type_is_present,
                external_user_details_is_present,
            ));
        }
    }
}

fn check_len(field: &str, value: Option<&str>) -> Result<(), WireError> {
    if let Some(value) = value
        && value.len() > 255
    {
        return Err(bad_request(format!(
            "{field} must be at most 255 characters"
        )));
    }
    Ok(())
}

fn parse_external_user_onboarded_at(value: Option<&str>) -> Result<Option<i64>, WireError> {
    value
        .map(|value| {
            chrono::DateTime::parse_from_rfc3339(value)
                .map(|timestamp| timestamp.timestamp_millis())
                .map_err(|_| {
                    bad_request("external_user_onboarded_at must be an RFC 3339 timestamp")
                })
        })
        .transpose()
}

fn external_user_details_to_application(
    params: Option<ExternalUserDetailsParams>,
) -> Result<Option<ExternalUserDetailsUpdate>, WireError> {
    let Some(params) = params else {
        return Ok(None);
    };
    for (field, value) in [
        ("external_user_details.country", &params.country),
        ("external_user_details.email_hash", &params.email_hash),
        ("external_user_details.name_hash", &params.name_hash),
        ("external_user_details.reference_id", &params.reference_id),
    ] {
        check_len(field, value.as_ref().and_then(|value| value.as_deref()))?;
    }
    if let Some(country) = params.country.as_ref().and_then(|value| value.as_deref())
        && !(country.len() == 2 && country.bytes().all(|byte| byte.is_ascii_uppercase()))
    {
        return Err(bad_request(
            "external_user_details.country must be an ISO 3166-1 alpha-2 code",
        ));
    }
    let onboarded_at = params
        .onboarded_at
        .as_deref()
        .map(|value| {
            chrono::DateTime::parse_from_rfc3339(value)
                .map(|timestamp| timestamp.timestamp_millis())
                .map_err(|_| {
                    bad_request("external_user_details.onboarded_at must be an RFC 3339 timestamp")
                })
        })
        .transpose()?;
    Ok(Some(ExternalUserDetailsUpdate {
        account_status: field_update(params.account_status.map(|value| {
            value.map(|status| match status {
                AccountStatus::Active => ExternalAccountStatus::Active,
                AccountStatus::Suspended => ExternalAccountStatus::Suspended,
                AccountStatus::Blocked => ExternalAccountStatus::Blocked,
            })
        })),
        country: field_update(params.country),
        email_hash: field_update(params.email_hash),
        entity_type: field_update(params.entity_type.map(|value| {
            value.map(|kind| match kind {
                EntityType::Individual => ExternalEntityType::Individual,
                EntityType::Business => ExternalEntityType::Business,
                EntityType::NonProfit => ExternalEntityType::NonProfit,
                EntityType::Government => ExternalEntityType::Government,
            })
        })),
        name_hash: field_update(params.name_hash),
        onboarded_at,
        reference_id: field_update(params.reference_id),
    }))
}

fn external_user_details_to_wire(details: ExternalUserDetails) -> WireExternalUserDetails {
    WireExternalUserDetails {
        account_status: details.account_status.map(|status| match status {
            ExternalAccountStatus::Active => AccountStatus::Active,
            ExternalAccountStatus::Suspended => AccountStatus::Suspended,
            ExternalAccountStatus::Blocked => AccountStatus::Blocked,
        }),
        country: details.country,
        email_hash: details.email_hash,
        entity_type: details.entity_type.map(|kind| match kind {
            ExternalEntityType::Individual => EntityType::Individual,
            ExternalEntityType::Business => EntityType::Business,
            ExternalEntityType::NonProfit => EntityType::NonProfit,
            ExternalEntityType::Government => EntityType::Government,
        }),
        name_hash: details.name_hash,
        onboarded_at: details.onboarded_at.map(|value| {
            awaken_session_application_contract::epoch_millis_to_rfc3339(value.max(0) as u64)
        }),
        reference_id: details.reference_id,
    }
}

fn relationship_to_application(value: Relationship) -> UserProfileRelationship {
    match value {
        Relationship::External => UserProfileRelationship::External,
        Relationship::Resold => UserProfileRelationship::Resold,
        Relationship::Internal => UserProfileRelationship::Internal,
    }
}

fn relationship_to_wire(value: UserProfileRelationship) -> Relationship {
    match value {
        UserProfileRelationship::External => Relationship::External,
        UserProfileRelationship::Resold => Relationship::Resold,
        UserProfileRelationship::Internal => Relationship::Internal,
    }
}

fn access_type_to_application(
    value: AccessType,
) -> awaken_data_subject_application::UserProfileAccessType {
    match value {
        AccessType::Application => {
            awaken_data_subject_application::UserProfileAccessType::Application
        }
        AccessType::Passthrough => {
            awaken_data_subject_application::UserProfileAccessType::Passthrough
        }
    }
}

fn access_type_to_wire(
    value: awaken_data_subject_application::UserProfileAccessType,
) -> AccessType {
    match value {
        awaken_data_subject_application::UserProfileAccessType::Application => {
            AccessType::Application
        }
        awaken_data_subject_application::UserProfileAccessType::Passthrough => {
            AccessType::Passthrough
        }
    }
}

fn access_type_relationship(value: AccessType) -> Relationship {
    match value {
        AccessType::Application => Relationship::External,
        AccessType::Passthrough => Relationship::Resold,
    }
}

fn validate_access_relationship(
    access_type: AccessType,
    relationship: Relationship,
) -> Result<(), WireError> {
    if access_type_relationship(access_type) == relationship {
        Ok(())
    } else {
        Err(bad_request(
            "access_type and relationship describe different profile access models",
        ))
    }
}

fn grant_to_application(value: TrustGrant) -> UserProfileTrustGrant {
    UserProfileTrustGrant {
        status: match value.status {
            TrustGrantStatus::Active => UserProfileTrustGrantStatus::Active,
            TrustGrantStatus::Pending => UserProfileTrustGrantStatus::Pending,
            TrustGrantStatus::Rejected => UserProfileTrustGrantStatus::Rejected,
        },
    }
}

fn grant_to_wire(value: UserProfileTrustGrant) -> TrustGrant {
    TrustGrant {
        status: match value.status {
            UserProfileTrustGrantStatus::Active => TrustGrantStatus::Active,
            UserProfileTrustGrantStatus::Pending => TrustGrantStatus::Pending,
            UserProfileTrustGrantStatus::Rejected => TrustGrantStatus::Rejected,
        },
    }
}

fn project(record: UserProfileRecord, projection: UserProfileProjection) -> UserProfile {
    let relationship = relationship_to_wire(record.relationship);
    let access_type = record.access_type.map(access_type_to_wire);
    let core = UserProfileCore {
        id: record.id,
        created_at: awaken_session_application_contract::epoch_millis_to_rfc3339(
            record.created_at.max(0) as u64,
        ),
        updated_at: awaken_session_application_contract::epoch_millis_to_rfc3339(
            record.updated_at.max(0) as u64,
        ),
        metadata: record.metadata,
        trust_grants: record
            .trust_grants
            .into_iter()
            .map(|(name, grant)| (name, grant_to_wire(grant)))
            .collect(),
        object_type: "user_profile",
        external_id: record.external_id,
        name: record.name,
    };
    match projection {
        UserProfileProjection::Legacy => {
            UserProfile::Legacy(LegacyUserProfile { core, relationship })
        }
        UserProfileProjection::Current => UserProfile::Current(CurrentUserProfile {
            core,
            access_type,
            external_user_onboarded_at: record.external_user_onboarded_at.map(|value| {
                awaken_session_application_contract::epoch_millis_to_rfc3339(value.max(0) as u64)
            }),
            external_user_details: None,
        }),
        UserProfileProjection::External => UserProfile::Current(CurrentUserProfile {
            core,
            access_type,
            external_user_onboarded_at: record.external_user_onboarded_at.map(|value| {
                awaken_session_application_contract::epoch_millis_to_rfc3339(value.max(0) as u64)
            }),
            // The platform's record is always presented under this selector,
            // every field `null` until stated, so a reader sees one shape.
            external_user_details: Some(external_user_details_to_wire(
                record.external_user_details.unwrap_or_default(),
            )),
        }),
    }
}

async fn create_profile(
    State(state): State<Arc<UserProfileHttpState>>,
    headers: HeaderMap,
    ManagedJson(params): ManagedJson<UserProfileCreateParams>,
) -> Result<Json<UserProfile>, WireError> {
    let projection = selected_projection(&headers)?;
    reject_current_only_field(
        projection,
        params.access_type.is_some(),
        params.external_user_details.is_some(),
    )?;
    check_len("external_id", params.external_id.as_deref())?;
    check_len("name", params.name.as_deref())?;
    let external_user_onboarded_at =
        parse_external_user_onboarded_at(params.external_user_onboarded_at.as_deref())?;
    let external_user_details = external_user_details_to_application(params.external_user_details)?;
    if let (Some(access_type), Some(relationship)) = (params.access_type, params.relationship) {
        validate_access_relationship(access_type, relationship)?;
    }
    let relationship = params
        .relationship
        .or_else(|| params.access_type.map(access_type_relationship))
        .unwrap_or_default();
    state
        .application
        .create_user_profile(CreateUserProfileCommand {
            org: state.org.clone(),
            metadata: params.metadata,
            relationship: relationship_to_application(relationship),
            access_type: params.access_type.map(access_type_to_application),
            external_id: params.external_id,
            external_user_onboarded_at,
            external_user_details,
            name: params.name,
        })
        .await
        .map(|record| project(record, projection))
        .map(Json)
        .map_err(error_response)
}

async fn retrieve_profile(
    State(state): State<Arc<UserProfileHttpState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<UserProfile>, WireError> {
    let projection = selected_projection(&headers)?;
    state
        .application
        .get_user_profile(&state.org, &id)
        .await
        .map(|record| project(record, projection))
        .map(Json)
        .map_err(error_response)
}

#[derive(Clone, Copy, Debug, Default)]
enum UserProfileListOrder {
    #[default]
    Asc,
    Desc,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UserProfileListOrderBy {
    #[default]
    CreatedAt,
    Name,
}

impl std::str::FromStr for UserProfileListOrder {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "asc" => Ok(Self::Asc),
            "desc" => Ok(Self::Desc),
            _ => Err("order must be `asc` or `desc`"),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserProfileListParams {
    #[serde(flatten)]
    page: PageQuery,
    #[serde(default, rename = "beta")]
    _beta_selector: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::types::page::deserialize_optional_query_value"
    )]
    order: Option<UserProfileListOrder>,
    #[serde(default)]
    order_by: Option<UserProfileListOrderBy>,
}

async fn list_profiles(
    State(state): State<Arc<UserProfileHttpState>>,
    headers: HeaderMap,
    ManagedQuery(query): ManagedQuery<UserProfileListParams>,
) -> Result<Json<PageCursor<UserProfile>>, WireError> {
    let projection = selected_projection(&headers)?;
    let mut data = state
        .application
        .list_user_profiles(&state.org)
        .await
        .map_err(error_response)?;
    if matches!(query.order_by, Some(UserProfileListOrderBy::Name)) {
        data.sort_by(|left, right| {
            (
                left.name.as_deref().unwrap_or(""),
                left.created_at,
                left.id.as_str(),
            )
                .cmp(&(
                    right.name.as_deref().unwrap_or(""),
                    right.created_at,
                    right.id.as_str(),
                ))
        });
    }
    if matches!(query.order, Some(UserProfileListOrder::Desc)) {
        data.reverse();
    }
    let page = paginate(data, &query.page, |profile| profile.id.as_str());
    Ok(Json(PageCursor {
        data: page
            .data
            .into_iter()
            .map(|record| project(record, projection))
            .collect(),
        next_page: page.next_page,
    }))
}

async fn update_profile(
    State(state): State<Arc<UserProfileHttpState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ManagedJson(params): ManagedJson<UserProfileUpdateParams>,
) -> Result<Json<UserProfile>, WireError> {
    let projection = selected_projection(&headers)?;
    reject_current_only_field(
        projection,
        params.access_type.is_some(),
        params.external_user_details.is_some(),
    )?;
    let external_user_details = external_user_details_to_application(params.external_user_details)?;
    check_len(
        "external_id",
        params
            .external_id
            .as_ref()
            .and_then(|value| value.as_deref()),
    )?;
    let external_user_onboarded_at =
        parse_external_user_onboarded_at(params.external_user_onboarded_at.as_deref())?;
    check_len(
        "name",
        params.name.as_ref().and_then(|value| value.as_deref()),
    )?;
    let trust_grants = params.trust_grants.map(|grants| {
        grants
            .into_iter()
            .map(|(name, grant)| (name, grant_to_application(grant)))
            .collect::<BTreeMap<_, _>>()
    });
    if let (Some(Some(access_type)), Some(relationship)) = (params.access_type, params.relationship)
    {
        validate_access_relationship(access_type, relationship.unwrap_or_default())?;
    }
    let (access_type, relationship) = match (params.access_type, params.relationship) {
        (Some(Some(access_type)), None) => (
            Some(Some(access_type_to_application(access_type))),
            Some(relationship_to_application(access_type_relationship(
                access_type,
            ))),
        ),
        (Some(access_type), relationship) => (
            Some(access_type.map(access_type_to_application)),
            relationship.map(|value| relationship_to_application(value.unwrap_or_default())),
        ),
        (None, Some(relationship)) => (
            Some(None),
            Some(relationship_to_application(
                relationship.unwrap_or_default(),
            )),
        ),
        (None, None) => (None, None),
    };
    state
        .application
        .update_user_profile(
            &state.org,
            &id,
            UpdateUserProfileCommand {
                metadata: params.metadata,
                relationship,
                access_type,
                trust_grants,
                external_id: field_update(params.external_id),
                external_user_onboarded_at,
                external_user_details,
                name: field_update(params.name),
            },
        )
        .await
        .map(|record| project(record, projection))
        .map(Json)
        .map_err(error_response)
}

fn field_update<T>(value: Option<Option<T>>) -> Option<UserProfileFieldUpdate<T>> {
    value.map(|value| match value {
        Some(value) => UserProfileFieldUpdate::Replace(value),
        None => UserProfileFieldUpdate::Clear,
    })
}

async fn enrollment_url(
    State(state): State<Arc<UserProfileHttpState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<EnrollmentUrl>, WireError> {
    selected_projection(&headers)?;
    state
        .application
        .mint_enrollment(&state.org, &id)
        .await
        .map(|EnrollmentTicket { url, expires_at }| {
            Json(EnrollmentUrl {
                object_type: "enrollment_url",
                url,
                expires_at: awaken_session_application_contract::epoch_millis_to_rfc3339(
                    expires_at.max(0) as u64,
                ),
            })
        })
        .map_err(error_response)
}
