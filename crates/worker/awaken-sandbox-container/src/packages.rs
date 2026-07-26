//! Immutable container-image package provisioning.
//!
//! This module is the single owner of translating neutral package requirements
//! into provider build input. Runtime adapters may execute the returned recipe;
//! they must not implement a second manager-command mapping.

use awaken_provisioning_contract as pc;

use crate::RuntimeError;

/// Render the immutable derived-image build consumed by package-capable
/// container runtimes. Every package is an argv element in Dockerfile JSON form;
/// no protocol string is interpolated into a shell command.
pub fn package_containerfile(
    base_image: &str,
    requirements: &pc::PackageRequirements,
) -> Result<String, RuntimeError> {
    if base_image.is_empty()
        || base_image
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(RuntimeError::Backend(
            "package base image must be a non-empty OCI reference".into(),
        ));
    }
    let mut steps = Vec::<Vec<String>>::new();
    for (manager, packages) in &requirements.managers {
        if packages.is_empty() {
            continue;
        }
        for package in packages {
            if package.trim() != package
                || package.is_empty()
                || package.starts_with('-')
                || package.chars().any(char::is_control)
            {
                return Err(RuntimeError::Backend(format!(
                    "invalid {manager} package requirement"
                )));
            }
        }
        match manager.as_str() {
            "apt" => {
                steps.push(vec!["apt-get".into(), "update".into()]);
                let mut argv = vec![
                    "apt-get".into(),
                    "install".into(),
                    "-y".into(),
                    "--no-install-recommends".into(),
                ];
                argv.extend(packages.clone());
                steps.push(argv);
            }
            "cargo" => steps.extend(
                packages
                    .iter()
                    .map(|package| vec!["cargo".into(), "install".into(), package.clone()]),
            ),
            "gem" => {
                let mut argv = vec!["gem".into(), "install".into()];
                argv.extend(packages.clone());
                steps.push(argv);
            }
            "go" => steps.extend(
                packages
                    .iter()
                    .map(|package| vec!["go".into(), "install".into(), package.clone()]),
            ),
            "npm" => {
                let mut argv = vec!["npm".into(), "install".into(), "--global".into()];
                argv.extend(packages.clone());
                steps.push(argv);
            }
            "pip" => {
                let mut argv = vec!["pip".into(), "install".into()];
                argv.extend(packages.clone());
                steps.push(argv);
            }
            _ => {
                return Err(RuntimeError::Backend(format!(
                    "unsupported package manager `{manager}`"
                )));
            }
        }
    }
    let mut dockerfile = format!("FROM {base_image}\n");
    for argv in steps {
        let argv = std::iter::once("/usr/bin/env".to_string())
            .chain(argv)
            .collect::<Vec<_>>();
        dockerfile.push_str("RUN ");
        dockerfile.push_str(
            &serde_json::to_string(&argv)
                .map_err(|error| RuntimeError::Backend(error.to_string()))?,
        );
        dockerfile.push('\n');
    }
    Ok(dockerfile)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Package-build cause graph: validated manager/package inputs become JSON
    /// exec-form build steps in official manager order; invalid manager/options
    /// fail before a runtime build. No package string becomes shell syntax.
    ///
    /// | Rule | Manager | Package | Result |
    /// |---|---|---|---|
    /// | B1 | supported | exact version | deterministic JSON RUN |
    /// | B2 | supported | shell punctuation | one argv string, never shell |
    /// | B3 | supported | leading option | reject |
    /// | B4 | unknown | any | reject |
    #[test]
    fn containerfile_follows_the_build_decision_table() {
        let requirements = pc::PackageRequirements {
            managers: [
                ("npm".into(), vec!["tsx@4.0.0".into()]),
                ("pip".into(), vec!["httpx==0.28.0;touch /tmp/pwn".into()]),
            ]
            .into_iter()
            .collect(),
        };
        let file = package_containerfile("registry.test/base@sha256:abc", &requirements).unwrap();
        assert!(
            file.starts_with("FROM registry.test/base@sha256:abc\n"),
            "B1"
        );
        assert!(
            file.contains(r#"RUN ["/usr/bin/env","npm","install","--global","tsx@4.0.0"]"#),
            "B1: {file}"
        );
        assert!(
            file.contains(r#"["/usr/bin/env","pip","install","httpx==0.28.0;touch /tmp/pwn"]"#),
            "B2 remains one JSON argv element: {file}"
        );
        assert!(!file.contains("RUN pip install"), "B2 no shell form");

        for (rule, manager, package) in [("B3", "pip", "--index-url"), ("B4", "docker", "image")] {
            let requirements = pc::PackageRequirements {
                managers: [(manager.into(), vec![package.into()])]
                    .into_iter()
                    .collect(),
            };
            assert!(
                package_containerfile("base:1", &requirements).is_err(),
                "{rule}"
            );
        }
    }
}
