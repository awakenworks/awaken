//! Host filesystem component rules, shared by projection and sandbox naming.

/// Whether a component is a Windows device name, including an extension.
#[must_use]
pub fn is_reserved_component(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|suffix| {
                suffix.chars().count() == 1 && "123456789¹²³".contains(suffix)
            })
        })
}

/// A component that cannot introduce host path syntax on any supported OS.
/// Unicode and interior spaces remain valid; filesystem alias detection is
/// performed by exclusive creation, not an approximate Unicode case fold.
#[must_use]
pub fn is_portable_component(name: &str) -> bool {
    !name.is_empty()
        && !matches!(name, "." | "..")
        && !name.ends_with([' ', '.'])
        && !name
            .chars()
            .any(|c| c.is_control() || "\\/:*?\"<>|".contains(c))
        && !is_reserved_component(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_rules_are_platform_independent() {
        // N1 traversal/drive/ADS/device/trailing aliases => reject on EVERY OS,
        // before host join. N2 ordinary Unicode/spaces/dots => exact preservation.
        for name in [
            "", ".", "..", "..\\out", "C:out", "a:b", "\\\\host", "NUL.txt", "con", "COM1",
            "LPT9.log", "COM¹", "a.", "a ", "a\n", "a/b", "a?b",
        ] {
            assert!(!is_portable_component(name), "N1 {name:?}");
        }
        for name in [
            "资料 brief.md",
            "a.b",
            ".gitkeep",
            "COM10",
            "company",
            "a b",
        ] {
            assert!(is_portable_component(name), "N2 {name:?}");
        }
    }
}
