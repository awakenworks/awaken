// @formal surface requirement=runtime.executable_snapshots.immutable_snapshot
// @formal reason: One codec owns the frozen Session prompt seed; dynamic dates are derived only from a caller-sampled clock value.
//! Trusted Session invocation facts encoded in the existing baseline prompts.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

const RESERVED: &str = "AWAKEN_SESSION_CONTEXT_";
const PREFIX: &str = "AWAKEN_SESSION_CONTEXT_V1:";

/// Origin supplied by the owning application, never inferred from metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionTrigger {
    Interactive {},
    DeploymentManual {
        deployment_id: String,
        run_id: String,
    },
    DeploymentSchedule {
        deployment_id: String,
        run_id: String,
        scheduled_at: String,
    },
    LegacyUnknown {},
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TimezoneSource {
    Explicit,
    Schedule,
    DefaultUtc,
}

/// Frozen source/timezone fact. The date is deliberately absent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SeedDocument")]
pub struct SessionContextSeed {
    trigger: SessionTrigger,
    timezone: String,
    timezone_source: TimezoneSource,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SeedDocument {
    trigger: SessionTrigger,
    timezone: String,
    timezone_source: TimezoneSource,
}

impl TryFrom<SeedDocument> for SessionContextSeed {
    type Error = SessionContextError;
    fn try_from(document: SeedDocument) -> Result<Self, Self::Error> {
        let seed = Self {
            trigger: document.trigger,
            timezone: document.timezone,
            timezone_source: document.timezone_source,
        };
        seed.validate()?;
        Ok(seed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SessionContextError {
    #[error("invalid IANA timezone")]
    InvalidTimezone,
    #[error("invalid or unsupported frozen Session context seed")]
    InvalidSeed,
    #[error("duplicate frozen Session context seed")]
    DuplicateSeed,
    #[error("Session context prompt prefix is reserved for application-owned facts")]
    ReservedPrompt,
    #[error("attempt clock value is outside the supported date range")]
    TimeOutOfRange,
}

impl SessionContextSeed {
    pub fn from_metadata(
        trigger: SessionTrigger,
        metadata: &BTreeMap<String, String>,
        schedule_timezone: Option<&str>,
    ) -> Result<Self, SessionContextError> {
        let (timezone, timezone_source) = match (schedule_timezone, metadata.get("awaken.timezone"))
        {
            (Some(timezone), _) => (timezone, TimezoneSource::Schedule),
            (None, Some(timezone)) => (timezone.as_str(), TimezoneSource::Explicit),
            (None, None) => ("UTC", TimezoneSource::DefaultUtc),
        };
        let seed = Self {
            trigger,
            timezone: timezone.into(),
            timezone_source,
        };
        seed.validate()?;
        Ok(seed)
    }

    fn validate(&self) -> Result<(), SessionContextError> {
        self.timezone
            .parse::<chrono_tz::Tz>()
            .map_err(|_| SessionContextError::InvalidTimezone)?;
        match &self.trigger {
            SessionTrigger::DeploymentManual {
                deployment_id,
                run_id,
            }
            | SessionTrigger::DeploymentSchedule {
                deployment_id,
                run_id,
                ..
            } if deployment_id.trim().is_empty() || run_id.trim().is_empty() => {
                return Err(SessionContextError::InvalidSeed);
            }
            SessionTrigger::DeploymentSchedule { scheduled_at, .. }
                if chrono::DateTime::parse_from_rfc3339(scheduled_at).is_err() =>
            {
                return Err(SessionContextError::InvalidSeed);
            }
            _ => {}
        }
        Ok(())
    }

    #[must_use]
    pub fn timezone(&self) -> &str {
        &self.timezone
    }

    #[must_use]
    pub fn is_seed_prompt(prompt: &str) -> bool {
        prompt.trim_start().starts_with(RESERVED)
    }

    /// Public prompt authoring may not mint a trusted source marker.
    pub fn admit_authored_prompts(prompts: &[String]) -> Result<(), SessionContextError> {
        if prompts.iter().any(|prompt| Self::is_seed_prompt(prompt)) {
            return Err(SessionContextError::ReservedPrompt);
        }
        Ok(())
    }

    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "{PREFIX}{}",
            serde_json::to_string(self).expect("Session seed is string/enum data")
        )
    }

    /// Missing markers retain an explicit legacy UTC interpretation. Decode
    /// only the immutable baseline, never Resource text or user messages.
    pub fn from_prompts(prompts: &[String]) -> Result<Self, SessionContextError> {
        let mut markers = prompts.iter().filter(|prompt| Self::is_seed_prompt(prompt));
        let Some(marker) = markers.next() else {
            return Ok(Self {
                trigger: SessionTrigger::LegacyUnknown {},
                timezone: "UTC".into(),
                timezone_source: TimezoneSource::DefaultUtc,
            });
        };
        if markers.next().is_some() {
            return Err(SessionContextError::DuplicateSeed);
        }
        let body = marker
            .strip_prefix(PREFIX)
            .ok_or(SessionContextError::InvalidSeed)?;
        serde_json::from_str(body).map_err(|_| SessionContextError::InvalidSeed)
    }

    /// Render one attempt's authoritative task-time facts. Adapter-provided
    /// environment information continues to describe its own process.
    pub fn render_at(&self, sampled_unix_ms: u64) -> Result<String, SessionContextError> {
        let epoch =
            i64::try_from(sampled_unix_ms).map_err(|_| SessionContextError::TimeOutOfRange)?;
        let instant = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(epoch)
            .ok_or(SessionContextError::TimeOutOfRange)?;
        let timezone = self
            .timezone
            .parse::<chrono_tz::Tz>()
            .map_err(|_| SessionContextError::InvalidTimezone)?;
        let date = instant.with_timezone(&timezone).format("%Y-%m-%d");
        let source = serde_json::to_string(&self.trigger).expect("trigger is string/enum data");
        let timezone_source =
            serde_json::to_string(&self.timezone_source).expect("timezone source is an enum");
        let unattended = matches!(self.trigger, SessionTrigger::DeploymentSchedule { .. });
        Ok(format!(
            "Awaken environment context (authoritative for this attempt's task date, timezone and trigger; adapter environment describes its own process):\ncurrent_date={date}\ntimezone={}\ntimezone_source={timezone_source}\ntrigger={source}\nunattended={unattended}",
            self.timezone
        ))
    }
}

#[cfg(test)]
mod tests;
