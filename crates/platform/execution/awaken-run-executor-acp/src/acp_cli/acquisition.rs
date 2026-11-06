//! Canonical local acquisition and argv projection for ACP executables.

/// How the local host obtains the ACP-serving executable. This is the sole local
/// argv authority; discovery and launch both project it instead of inferring an
/// install strategy from a command name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpAcquisition {
    Direct {
        executable: &'static str,
        args: &'static [&'static str],
    },
    PinnedNpmWrapper {
        installer: &'static str,
        package: &'static str,
        bin: &'static str,
    },
}

impl AcpAcquisition {
    #[must_use]
    pub fn executable(self) -> &'static str {
        match self {
            Self::Direct { executable, .. } => executable,
            Self::PinnedNpmWrapper { bin, .. } => bin,
        }
    }

    #[must_use]
    pub fn local_argv(self) -> Vec<String> {
        match self {
            Self::Direct { executable, args } => std::iter::once(executable)
                .chain(args.iter().copied())
                .map(str::to_string)
                .collect(),
            Self::PinnedNpmWrapper { bin, .. } => vec![bin.to_string()],
        }
    }

    #[must_use]
    pub fn requires_installation(self) -> bool {
        matches!(self, Self::PinnedNpmWrapper { .. })
    }
}
