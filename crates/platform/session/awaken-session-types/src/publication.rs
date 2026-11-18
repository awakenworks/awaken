//! Opaque engine-issued condition for publication in one logical Session.
/// A conditional authority observation, not a lock or a routing address.
/// Consumers forward it to the selected store; only its issuing engine may
/// interpret it. Possession does not bypass normal scope/claim authorization.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionPublicationCondition(String);
impl SessionPublicationCondition {
    pub fn new(token: String) -> Result<Self, String> {
        if token.is_empty() || token.len() > 16384 {
            return Err("invalid Session publication condition".into());
        }
        Ok(Self(token))
    }
    #[must_use]
    pub fn token(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for SessionPublicationCondition {
    type Error = String;
    fn try_from(token: String) -> Result<Self, String> {
        Self::new(token)
    }
}
impl From<SessionPublicationCondition> for String {
    fn from(condition: SessionPublicationCondition) -> Self {
        condition.0
    }
}
