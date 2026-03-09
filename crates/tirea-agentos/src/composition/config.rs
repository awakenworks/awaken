#[cfg(feature = "skills")]
#[derive(Debug, Clone)]
pub struct SkillsConfig {
    pub enabled: bool,
    pub advertise_catalog: bool,
    pub discovery_max_entries: usize,
    pub discovery_max_chars: usize,
}

#[cfg(feature = "skills")]
impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            advertise_catalog: true,
            discovery_max_entries: 32,
            discovery_max_chars: 16 * 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentToolsConfig {
    pub discovery_max_entries: usize,
    pub discovery_max_chars: usize,
}

impl Default for AgentToolsConfig {
    fn default() -> Self {
        Self {
            discovery_max_entries: 64,
            discovery_max_chars: 16 * 1024,
        }
    }
}
