//! Resident Hand process shape owned by one Session container.

/// One provider-owned resident Hand process inside a Session container.
/// Authentication and topology stay on [`crate::ContainerRuntime::open_channel`];
/// this value owns only the exact process shape rendered with the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidentHandConfig {
    pub bin: String,
    pub port: u16,
    pub ledger_dir: String,
    pub ledger_max_entries: usize,
    pub max_connections: usize,
}

pub const DEFAULT_RESIDENT_HAND_LEDGER_MAX_ENTRIES: usize = 4_096;
pub const DEFAULT_RESIDENT_HAND_MAX_CONNECTIONS: usize = 16;

impl ResidentHandConfig {
    pub fn new(bin: impl Into<String>, port: u16) -> Result<Self, String> {
        let bin = bin.into();
        if bin.trim().is_empty() {
            return Err("resident Hand executable must not be blank".into());
        }
        if port == 0 {
            return Err("resident Hand port must be non-zero".into());
        }
        Ok(Self {
            bin,
            port,
            ledger_dir: "/tmp/.awaken-hand-operations".into(),
            ledger_max_entries: DEFAULT_RESIDENT_HAND_LEDGER_MAX_ENTRIES,
            max_connections: DEFAULT_RESIDENT_HAND_MAX_CONNECTIONS,
        })
    }

    /// Override bounded resident resources. Zero would disable the safety
    /// control and is therefore rejected at provider construction.
    pub fn with_resource_limits(
        mut self,
        ledger_max_entries: usize,
        max_connections: usize,
    ) -> Result<Self, String> {
        if ledger_max_entries == 0 || max_connections == 0 {
            return Err("resident Hand resource limits must be positive".into());
        }
        self.ledger_max_entries = ledger_max_entries;
        self.max_connections = max_connections;
        Ok(self)
    }

    pub(crate) fn command(&self) -> Vec<String> {
        vec![
            self.bin.clone(),
            "hand".into(),
            "--listen".into(),
            format!("127.0.0.1:{}", self.port),
        ]
    }
}
