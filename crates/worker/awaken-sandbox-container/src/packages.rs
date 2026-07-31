//! Immutable container-image package provisioning.
//!
//! This module is the single owner of translating neutral package requirements
//! into provider build input. Runtime adapters may execute the returned recipe;
//! they must not implement a second manager-command mapping.

use awaken_provisioning_contract as pc;

use crate::RuntimeError;

#[cfg(any(feature = "docker", feature = "podman", test))]
const PACKAGE_RECIPE_VERSION: &str = "2";

#[cfg(any(feature = "docker", feature = "podman", test))]
fn requirement_is_pinned(manager: &str, package: &str) -> bool {
    match manager {
        "apt" => package.contains('='),
        "cargo" | "go" => package
            .rsplit_once('@')
            .is_some_and(|(name, version)| !name.is_empty() && !version.is_empty()),
        "gem" => package
            .rsplit_once(':')
            .is_some_and(|(name, version)| !name.is_empty() && !version.is_empty()),
        "npm" => package
            .rfind('@')
            .is_some_and(|index| index > 0 && index + 1 < package.len()),
        "pip" => package.contains("=="),
        _ => false,
    }
}

#[cfg(any(feature = "docker", feature = "podman", test))]
fn has_unpinned_requirement(requirements: &pc::PackageRequirements) -> bool {
    requirements.managers.iter().any(|(manager, packages)| {
        packages
            .iter()
            .any(|package| !requirement_is_pinned(manager, package))
    })
}

/// Render the immutable derived-image build consumed by package-capable
/// container runtimes. Every package is an argv element in Dockerfile JSON form;
/// no protocol string is interpolated into a shell command.
pub fn package_containerfile(
    base_image: &str,
    requirements: &pc::PackageRequirements,
) -> Result<String, RuntimeError> {
    package_containerfile_for_user(base_image, "", requirements)
}

fn package_containerfile_for_user(
    base_image: &str,
    base_user: &str,
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
    if !base_user.is_empty()
        && !base_user
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
    {
        return Err(RuntimeError::Backend(
            "package base image has an unsupported OCI user".into(),
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
    let mut dockerfile = format!("FROM {base_image}\nUSER 0\n");
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
    if !base_user.is_empty() && base_user != "0" && base_user != "root" {
        dockerfile.push_str(&format!("USER {base_user}\n"));
    }
    Ok(dockerfile)
}

/// Return the immutable build recipe and its content address. The exact local
/// base-image identity is part of the recipe, so moving a mutable tag invalidates
/// the cache even when the Environment package lists stay unchanged.
#[cfg(any(feature = "docker", feature = "podman", test))]
pub(crate) fn package_image_recipe(
    base_identity: &str,
    base_user: &str,
    requirements: &pc::PackageRequirements,
) -> Result<(String, String), RuntimeError> {
    let mut containerfile = package_containerfile_for_user(base_identity, base_user, requirements)?;
    let resolution = if has_unpinned_requirement(requirements) {
        Some(
            requirements
                .resolution_id
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    RuntimeError::Backend(
                        "unpinned package requirements need a frozen Environment resolution id"
                            .into(),
                    )
                })?,
        )
    } else {
        None
    };
    let identity = format!(
        "recipe={PACKAGE_RECIPE_VERSION}\nresolution={}\n{containerfile}",
        resolution.unwrap_or("pinned")
    );
    let fingerprint = blake3::hash(identity.as_bytes()).to_hex().to_string();
    containerfile.push_str(&format!(
        "LABEL org.awaken.package-recipe={PACKAGE_RECIPE_VERSION} \\\n      org.awaken.package-key={fingerprint}\n"
    ));
    Ok((containerfile, fingerprint))
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
            ..Default::default()
        };
        let file = package_containerfile("registry.test/base@sha256:abc", &requirements).unwrap();
        assert!(
            file.starts_with("FROM registry.test/base@sha256:abc\nUSER 0\n"),
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
                ..Default::default()
            };
            assert!(
                package_containerfile("base:1", &requirements).is_err(),
                "{rule}"
            );
        }
    }

    #[test]
    fn image_fingerprint_keys_on_exact_base_identity_and_package_contents() {
        let requirements = pc::PackageRequirements {
            managers: [("pip".into(), vec!["httpx==0.28.0".into()])]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let (recipe, first) =
            package_image_recipe("sha256:base-a", "10001", &requirements).unwrap();
        assert!(recipe.contains("USER 10001\n"));
        assert!(recipe.contains("org.awaken.package-recipe=2"));
        let (_, identical) = package_image_recipe("sha256:base-a", "10001", &requirements).unwrap();
        assert_eq!(identical, first, "identical inputs reuse one image key");

        let (_, moved_base) =
            package_image_recipe("sha256:base-b", "10001", &requirements).unwrap();
        assert_ne!(
            moved_base, first,
            "a mutable base tag moving invalidates the key"
        );

        let changed = pc::PackageRequirements {
            managers: [("pip".into(), vec!["httpx==0.29.0".into()])]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let (_, changed_packages) =
            package_image_recipe("sha256:base-a", "10001", &changed).unwrap();
        assert_ne!(
            changed_packages, first,
            "a package or version change invalidates the key"
        );

        let (_, changed_user) =
            package_image_recipe("sha256:base-a", "agent", &requirements).unwrap();
        assert_ne!(
            changed_user, first,
            "the restored runtime identity is part of the image key"
        );
        assert!(
            package_image_recipe("sha256:base-a", "root\nRUN false", &requirements).is_err(),
            "an untrusted image user cannot inject another build instruction"
        );
    }

    #[test]
    fn unpinned_requirements_are_scoped_to_one_frozen_environment_resolution() {
        let mut requirements = pc::PackageRequirements {
            managers: [("pip".into(), vec!["httpx".into()])].into_iter().collect(),
            resolution_id: Some("environment-revision-1".into()),
        };
        let (_, first) = package_image_recipe("sha256:base-a", "10001", &requirements).unwrap();
        let (_, same) = package_image_recipe("sha256:base-a", "10001", &requirements).unwrap();
        assert_eq!(
            same, first,
            "one Environment snapshot reuses its resolution"
        );

        requirements.resolution_id = Some("environment-revision-2".into());
        let (_, updated) = package_image_recipe("sha256:base-a", "10001", &requirements).unwrap();
        assert_ne!(
            updated, first,
            "a new Environment snapshot resolves latest again"
        );

        requirements.resolution_id = None;
        assert!(
            package_image_recipe("sha256:base-a", "10001", &requirements).is_err(),
            "an unpinned requirement without a frozen lifecycle must fail closed"
        );
    }
}
