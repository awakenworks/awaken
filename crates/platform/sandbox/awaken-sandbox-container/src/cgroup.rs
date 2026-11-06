use awaken_provisioning_contract as pc;

/// Neutral cgroup caps derived from [`pc::ResourceLimits`] — what a container
/// runtime must apply. The adapter-specific Docker/Kubernetes translation remains
/// outside this pure value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CgroupCaps {
    pub memory_bytes: Option<i64>,
    /// Pinned equal to `memory_bytes`, disabling escape beyond the memory cap via swap.
    pub memory_swap_bytes: Option<i64>,
    /// CPU quota expressed in nano-CPUs (`cpu_millis * 1e6`).
    pub nano_cpus: Option<i64>,
    pub pids: Option<i64>,
    /// Writable-layer size in the string form container runtimes accept.
    pub disk_size: Option<String>,
}

impl CgroupCaps {
    #[must_use]
    pub fn from_limits(limits: &pc::ResourceLimits) -> Self {
        let memory = limits.memory_bytes.map(|value| value as i64);
        Self {
            memory_bytes: memory,
            memory_swap_bytes: memory,
            nano_cpus: limits.cpu_millis.map(|value| i64::from(value) * 1_000_000),
            pids: limits.pids.map(i64::from),
            disk_size: limits.disk_bytes.map(|value| value.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_swap_to_the_memory_cap_and_maps_every_field() {
        let limits = pc::ResourceLimits {
            cpu_millis: Some(1500),
            memory_bytes: Some(512 * 1024 * 1024),
            pids: Some(256),
            disk_bytes: Some(2 * 1024 * 1024 * 1024),
        };
        let caps = CgroupCaps::from_limits(&limits);
        assert_eq!(caps.memory_bytes, Some(512 * 1024 * 1024));
        assert_eq!(caps.memory_swap_bytes, caps.memory_bytes);
        assert_eq!(caps.nano_cpus, Some(1_500_000_000));
        assert_eq!(caps.pids, Some(256));
        assert_eq!(caps.disk_size.as_deref(), Some("2147483648"));
    }

    #[test]
    fn unset_limits_map_to_no_caps() {
        let caps = CgroupCaps::from_limits(&pc::ResourceLimits::default());
        assert_eq!(caps, CgroupCaps::default());
        assert!(caps.memory_swap_bytes.is_none());
    }
}
