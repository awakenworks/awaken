/// The product process role. Execution-plane `hand` remains the separate
/// `awaken-sandbox hand` binary and is deliberately not a Control role.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Role {
    #[default]
    AllInOne,
    Control,
    Coordinator,
    Worker,
}

impl Role {
    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "all-in-one" => Ok(Self::AllInOne),
            "control" => Ok(Self::Control),
            "coordinator" => Ok(Self::Coordinator),
            "worker" => Ok(Self::Worker),
            other => Err(format!(
                "invalid role={other:?}: expected all-in-one, control, coordinator, or worker"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllInOne => "all-in-one",
            Self::Control => "control",
            Self::Coordinator => "coordinator",
            Self::Worker => "worker",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_names_are_canonical_and_do_not_preserve_overlapping_aliases() {
        // Cause/effect decision table:
        // R1 each canonical service name -> its exact Role; R2 a retired
        // overlapping name -> configuration error; R3 rendering a Role -> the
        // same canonical spelling accepted by R1. This prevents command and file
        // configuration from maintaining different role vocabularies.
        for (name, role) in [
            ("all-in-one", Role::AllInOne),
            ("control", Role::Control),
            ("coordinator", Role::Coordinator),
            ("worker", Role::Worker),
        ] {
            assert_eq!(Role::parse(name).unwrap(), role, "R1 {name}");
            assert_eq!(role.as_str(), name, "R3 {name}");
        }
        // A standalone Resources role is intentionally absent until independent
        // scaling or credential isolation justifies its transport boundary.
        for retired in ["serve", "server", "management", "resources"] {
            assert!(Role::parse(retired).is_err(), "R2 {retired}");
        }
    }
}
