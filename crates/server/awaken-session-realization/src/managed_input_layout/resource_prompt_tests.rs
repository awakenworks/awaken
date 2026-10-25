use super::*;
use awaken_resource_contract::{BindingId, FileId, ResourceAccess};
use awaken_session_configuration_contract::resource::{ResolvedInput, ResolvedInputSource};

fn file(instructions: Option<&str>) -> ResolvedInput {
    ResolvedInput {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        binding_id: BindingId::from("input-file"),
        source: ResolvedInputSource::File {
            file_id: FileId::from("file-1"),
        },
        mount_path: "brief.md".into(),
        access: ResourceAccess::ReadOnly,
        instructions: instructions.map(str::to_owned),
    }
}

fn expected_file_prompt(path: &str) -> String {
    // Fixed fixture identity; keep JSON key ordering outside the contract.
    // The separate matrix decodes JSON and checks every identity/path field.
    format!(
        "Read-only file reference (JSON data): {}",
        serde_json::json!({"file_id":"file-1", "binding_id":"input-file", "path":path})
    )
}

#[test]
fn resource_prompt_variable_decision_table_is_closed_and_provider_relative() {
    // Cause/effect graph:
    // C1 path view={sandbox absolute, Workdir relative};
    // C2 separator={POSIX, Windows}; C3 template={known, unknown, malformed}.
    // Effects: E1 known variables render from the same logical mount;
    // E2 Workdir output contains no provider-private absolute prefix;
    // E3 unknown/malformed Awaken variables fail before realization.
    // Constraints: Windows separator affects only Workdir-relative paths;
    // sandbox paths are provider-neutral POSIX paths. Decision rules:
    // R1 absolute+known=>E1; R2 relative+POSIX+known=>E1+E2;
    // R3 relative+Windows+known=>E1+E2; R4/R5 invalid=>E3.
    let projection = ResourcePromptProjection::try_from_input(&file(Some(
        "binding={{awaken.resource.binding}} path={{awaken.resource.path}} dir={{awaken.resource.directory}} workspace={{awaken.workspace}} outputs={{awaken.outputs}} logical={{awaken.resource.logical_path}} access={{awaken.resource.access}}",
    )))
    .expect("known template");
    let absolute = projection.render(
        crate::sandbox_layout::ManagedContentDelivery::ManagedFilesystem,
        AgentPathView::sandbox_absolute(),
    );
    assert_eq!(
        absolute,
        format!(
            "{}\nbinding=input-file path=/mnt/session/uploads/brief.md dir=/mnt/session/uploads workspace=/workspace outputs=/mnt/session/outputs logical=/mnt/session/uploads/brief.md access=read-only",
            expected_file_prompt("/mnt/session/uploads/brief.md")
        ),
        "R1 every variable has an exact oracle"
    );

    let posix_path = AgentPathView::workdir_relative("/workspace")
        .render_with_separator("/mnt/session/uploads/brief.md", '/');
    let windows_path = AgentPathView::workdir_relative("/workspace")
        .render_with_separator("/mnt/session/uploads/brief.md", '\\');
    assert_eq!(posix_path, "../mnt/session/uploads/brief.md", "R2");
    assert_eq!(windows_path, "..\\mnt\\session\\uploads\\brief.md", "R3");
    assert!(!posix_path.starts_with('/'), "R2 E2");
    assert!(!windows_path.contains(":"), "R3 E2");
    let relative = projection.render(
        crate::sandbox_layout::ManagedContentDelivery::ManagedFilesystem,
        AgentPathView::workdir_relative("/workspace"),
    );
    let parent = ["..", "mnt", "session", "uploads"].join(std::path::MAIN_SEPARATOR_STR);
    let outputs = ["..", "mnt", "session", "outputs"].join(std::path::MAIN_SEPARATOR_STR);
    let path = format!("{parent}{}brief.md", std::path::MAIN_SEPARATOR);
    assert_eq!(
        relative,
        format!(
            "{}\nbinding=input-file path={path} dir={parent} workspace=. outputs={outputs} logical=/mnt/session/uploads/brief.md access=read-only",
            expected_file_prompt(&path)
        ),
        "R2 every variable, not only a substring"
    );

    for (rule, instructions) in [
        ("R4", "{{awaken.resource.missing}}"),
        ("R5", "{{awaken.resource.path"),
    ] {
        assert!(
            ResourcePromptProjection::try_from_input(&file(Some(instructions))).is_err(),
            "{rule}"
        );
    }
}

#[test]
fn resource_prompt_templates_preserve_literals_and_never_expand_resource_data() {
    // Cause/effect rules: L1 absent/empty => default only; L2 ordinary braces,
    // Unicode and newlines => unchanged prose; L3 repeated known tokens =>
    // every occurrence; L4 replacement contains a token => literal data.
    // L5 malformed token after valid prose => reject the entire template.
    // No rule can interpret mounted bytes or a replacement as authored code.
    for authored in [None, Some("")] {
        let projection = ResourcePromptProjection::try_from_input(&file(authored)).unwrap();
        assert_eq!(
            projection.render(
                crate::sandbox_layout::ManagedContentDelivery::ManagedFilesystem,
                AgentPathView::sandbox_absolute(),
            ),
            expected_file_prompt("/mnt/session/uploads/brief.md"),
            "L1"
        );
    }
    for (authored, expected) in [
        (
            "中文 {json} {{other.path}}\nend",
            "中文 {json} {{other.path}}\nend",
        ),
        (
            "{{awaken.resource.access}}/{{awaken.resource.access}}",
            "read-only/read-only",
        ),
        ("{{awaken.resource.binding}}", "{{awaken.resource.path}}"),
    ] {
        let mut input = file(Some(authored));
        input.binding_id = BindingId::from("{{awaken.resource.path}}");
        let rendered = ResourcePromptProjection::try_from_input(&input)
            .unwrap()
            .render(
                crate::sandbox_layout::ManagedContentDelivery::ManagedFilesystem,
                AgentPathView::sandbox_absolute(),
            );
        assert_eq!(rendered.split_once('\n').unwrap().1, expected, "L2-L4");
    }
    for invalid in [
        "{{awaken.workspace}} then {{awaken.unknown}}",
        "{{awaken.workspace}} then {{awaken.resource.path",
        "{{awaken.workspace {{awaken.outputs}}",
    ] {
        assert!(
            ResourcePromptProjection::try_from_input(&file(Some(invalid))).is_err(),
            "L5"
        );
    }
}

#[test]
fn resource_directory_means_parent_and_unicode_paths_roundtrip() {
    // D1 resource.directory is the lexical parent for every resource kind,
    // including a directory mount; resource.path names the directory itself.
    // D2 spaces/Unicode stay lossless; D3 no parent => "."; D4 root stays root.
    // Both separators are exercised without assuming the executing OS.
    for (path, parent) in [
        ("/mnt/memory/team", "/mnt/memory"),
        ("/workspace/项目 repo", "/workspace"),
        (
            "../mnt/session/uploads/资料 brief.md",
            "../mnt/session/uploads",
        ),
        ("..\\mnt\\memory\\团队", "..\\mnt\\memory"),
        ("brief.md", "."),
        ("/brief.md", "/"),
    ] {
        assert_eq!(resource_parent(path), parent, "D1-D4");
    }
    let mut input = file(Some("{{awaken.resource.path}}"));
    input.mount_path = "资料 brief.md".into();
    let rendered = ResourcePromptProjection::try_from_input(&input)
        .unwrap()
        .render(
            crate::sandbox_layout::ManagedContentDelivery::ManagedFilesystem,
            AgentPathView::workdir_relative("/workspace/项目 repo/src"),
        );
    let expected = [
        "..",
        "..",
        "..",
        "mnt",
        "session",
        "uploads",
        "资料 brief.md",
    ]
    .join(std::path::MAIN_SEPARATOR_STR);
    assert_eq!(rendered.split_once('\n').unwrap().1, expected, "D2");
}

#[test]
fn workdir_prompt_paths_follow_the_effective_nested_cwd_cross_platform() {
    // Cause/effect graph: C1 cwd={workspace, workspace/repo/src};
    // C2 target={workspace, repository, File, Memory, outputs};
    // C3 separator={POSIX, Windows}. E1 every rendered path resolves from
    // the same logical cwd; E2 common prefixes are removed; E3 no host
    // drive/root enters model-visible text. Constraints: the authored cwd
    // is validated upstream and all targets are WorkspaceLayout-owned.
    // Decision rules: P1 workspace=>workspace-relative; P2 nested+ancestor=>`..`;
    // P3 nested+sibling-root=>common-prefix traversal; P4 Windows=>the same
    // components with `\\`; all rules require E3.
    let nested = AgentPathView::workdir_relative("/workspace/repo/src");
    for (target, posix, windows) in [
        ("/workspace", "../..", "..\\.."),
        ("/workspace/repo", "..", ".."),
        ("/workspace/repo/README.md", "../README.md", "..\\README.md"),
        (
            "/mnt/session/uploads/brief.md",
            "../../../mnt/session/uploads/brief.md",
            "..\\..\\..\\mnt\\session\\uploads\\brief.md",
        ),
        (
            "/mnt/memory/team",
            "../../../mnt/memory/team",
            "..\\..\\..\\mnt\\memory\\team",
        ),
        (
            "/mnt/session/outputs",
            "../../../mnt/session/outputs",
            "..\\..\\..\\mnt\\session\\outputs",
        ),
    ] {
        assert_eq!(nested.render_with_separator(target, '/'), posix, "P2/P3");
        let rendered = nested.render_with_separator(target, '\\');
        assert_eq!(rendered, windows, "P4");
        assert!(!rendered.contains(':') && !rendered.starts_with('\\'), "E3");
    }
    assert_eq!(
        nested.workspace(),
        ["..", ".."].join(std::path::MAIN_SEPARATOR_STR),
        "P2 workspace variable"
    );
    let workspace = AgentPathView::workdir_relative("/workspace");
    assert_eq!(workspace.workspace(), ".", "P1 workspace variable");
    assert_eq!(
        workspace.render_with_separator("/mnt/memory/team", '/'),
        "../mnt/memory/team",
        "P1 sibling mount"
    );
}

#[test]
fn memory_path_variables_require_filesystem_delivery() {
    // Cause/effect graph: C1 Memory instructions use a physical-path
    // variable; C2 uses binding/logical facts only. E1 C1 marks the prompt
    // as requiring filesystem delivery; E2 C2 remains valid for semantic
    // tools. The Session delivery decision owns the eventual rejection.
    // Decision table: M1 C1=>E1; M2 C2=>E2.
    let memory = |instructions: &str| ResolvedInput {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        binding_id: BindingId::from("memory"),
        source: ResolvedInputSource::MemoryStore {
            memory_store_id: awaken_resource_contract::MemoryStoreId::from("store"),
            config: awaken_resource_contract::MemoryStoreConfigVersion {
                memory_store_id: awaken_resource_contract::MemoryStoreId::from("store"),
                version: awaken_resource_contract::ConfigVersion(1),
                retention_policy: Default::default(),
            },
        },
        mount_path: "/mnt/memory".into(),
        access: ResourceAccess::ReadWrite,
        instructions: Some(instructions.into()),
    };
    assert!(
        ResourcePromptProjection::try_from_input(&memory("read {{awaken.resource.path}}"))
            .unwrap()
            .requires_filesystem_delivery(),
        "M1"
    );
    assert!(
        !ResourcePromptProjection::try_from_input(&memory(
            "use {{awaken.resource.binding}} at logical {{awaken.resource.logical_path}}"
        ))
        .unwrap()
        .requires_filesystem_delivery(),
        "M2"
    );
    for variable in [
        "{{awaken.workspace}}",
        "{{awaken.outputs}}",
        "{{awaken.resource.directory}}",
    ] {
        assert!(
            ResourcePromptProjection::try_from_input(&memory(variable))
                .unwrap()
                .requires_filesystem_delivery(),
            "M1 physical variable {variable}"
        );
    }
}

#[test]
fn every_managed_resource_kind_uses_the_same_prompt_projection() {
    // Causes: C1 kind={file,memory,repository}; C2 Memory delivery={files,
    // semantic}; C3 access={read-only,read-write}. Effects: E1 file and repo
    // expose their exact provider-relative path; E2 filesystem Memory names
    // its mount; E3 semantic Memory names only binding-scoped tools; E4
    // read/write wording matches frozen access. The cases below cover every
    // renderer arm and both access effects; authored variable parsing is
    // owned by the preceding decision table.
    let memory = |access| ResolvedInput {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        binding_id: BindingId::from("memory"),
        source: ResolvedInputSource::MemoryStore {
            memory_store_id: awaken_resource_contract::MemoryStoreId::from("store"),
            config: awaken_resource_contract::MemoryStoreConfigVersion {
                memory_store_id: awaken_resource_contract::MemoryStoreId::from("store"),
                version: awaken_resource_contract::ConfigVersion::INITIAL,
                retention_policy: Default::default(),
            },
        },
        mount_path: "/mnt/memory".into(),
        access,
        instructions: None,
    };
    let repository = ResolvedInput {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        binding_id: BindingId::from("repo"),
        source: ResolvedInputSource::Repository {
            repository_id: awaken_resource_contract::RepositoryId::from("repository"),
            config: awaken_resource_contract::RepositoryConfigVersion {
                repository_id: awaken_resource_contract::RepositoryId::from("repository"),
                version: awaken_resource_contract::ConfigVersion::INITIAL,
                remote_url: "https://example.invalid/repo.git".into(),
                credential_binding: None,
                initial_branch: None,
                initial_commit: None,
                clone_policy: Default::default(),
            },
            credential: None,
        },
        mount_path: "/workspace/repo".into(),
        access: ResourceAccess::ReadWrite,
        instructions: None,
    };
    let file = ResourcePromptProjection::try_from_input(&file(None))
        .unwrap()
        .render(
            crate::sandbox_layout::ManagedContentDelivery::ManagedFilesystem,
            AgentPathView::workdir_relative("/workspace"),
        );
    let relative_file =
        ["..", "mnt", "session", "uploads", "brief.md"].join(std::path::MAIN_SEPARATOR_STR);
    assert_eq!(file, expected_file_prompt(&relative_file), "E1 file");
    let memory_files = ResourcePromptProjection::try_from_input(&memory(ResourceAccess::ReadOnly))
        .unwrap()
        .render(
            crate::sandbox_layout::ManagedContentDelivery::ManagedFilesystem,
            AgentPathView::sandbox_absolute(),
        );
    assert!(memory_files.contains("/mnt/memory"), "E2");
    assert!(memory_files.contains("read-only"), "E4");
    let memory_tools = ResourcePromptProjection::try_from_input(&memory(ResourceAccess::ReadWrite))
        .unwrap()
        .render(
            crate::sandbox_layout::ManagedContentDelivery::SemanticTools,
            AgentPathView::sandbox_absolute(),
        );
    assert!(!memory_tools.contains("/mnt/memory"), "E3");
    assert!(memory_tools.contains("binding: \"memory\""), "E3");
    assert!(memory_tools.contains("read/write"), "E4");
    let repository = ResourcePromptProjection::try_from_input(&repository)
        .unwrap()
        .render(
            crate::sandbox_layout::ManagedContentDelivery::ManagedFilesystem,
            AgentPathView::sandbox_absolute(),
        );
    assert!(repository.contains("/workspace/repo"), "E1 repository");
    assert!(repository.contains("commit"), "E4");
}

#[test]
fn selected_file_identity_and_environment_path_are_one_runtime_reference() {
    // Causes: C1 equal filenames with different immutable File/binding IDs;
    // C2 requested API mount is not the actual upload-root path;
    // C3 Sandbox absolute / Workdir root / Workdir nested cwd;
    // C4 identity contains quotes, newline or template-looking literal data.
    // Effects: E1 exact File and binding are paired with their own usable path;
    // E2 no client mount request masquerades as an executable path;
    // E3 identity remains JSON data, not another prompt instruction/template.
    // Rules: cross all 3 identity rows with all 3 Environment views (9 rows).
    // OS separator projection has independent POSIX/Windows literal oracles in
    // resource_prompt_variable_decision_table_is_closed_and_provider_relative.
    for (id, binding, leaf) in [
        ("file-a", "binding-a", "a"),
        ("file-b", "binding-b", "b"),
        (
            "file-\"quoted\n{{awaken.resource.path}}",
            "binding-\"quoted",
            "c",
        ),
    ] {
        let mut input = file(None);
        input.source = ResolvedInputSource::File {
            file_id: FileId::from(id),
        };
        input.binding_id = BindingId::from(binding);
        input.mount_path = format!("/mnt/files/references/{leaf}/report.md");
        let projection = ResourcePromptProjection::try_from_input(&input).unwrap();
        for (view, expected) in [
            (
                AgentPathView::sandbox_absolute(),
                format!("/mnt/session/uploads/mnt/files/references/{leaf}/report.md"),
            ),
            (
                AgentPathView::workdir_relative("/workspace"),
                format!("../mnt/session/uploads/mnt/files/references/{leaf}/report.md")
                    .replace('/', std::path::MAIN_SEPARATOR_STR),
            ),
            (
                AgentPathView::workdir_relative("/workspace/repo/src"),
                format!("../../../mnt/session/uploads/mnt/files/references/{leaf}/report.md")
                    .replace('/', std::path::MAIN_SEPARATOR_STR),
            ),
        ] {
            let rendered = projection.render(
                crate::sandbox_layout::ManagedContentDelivery::ManagedFilesystem,
                view,
            );
            assert_eq!(
                rendered.lines().count(),
                1,
                "identity newline is escaped data"
            );
            let reference: serde_json::Value = serde_json::from_str(
                rendered
                    .strip_prefix("Read-only file reference (JSON data): ")
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                reference,
                serde_json::json!({"file_id": id, "binding_id":binding, "path":expected})
            );
            assert_ne!(reference["path"], input.mount_path);
        }
    }
}
