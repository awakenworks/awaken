use std::sync::Arc;

use awaken_file_store::FileStore;
use base64::Engine as _;
use tempfile::TempDir;

use crate::docker::DockerMountMaterializer;
use crate::k8s::{K8sMount, K8sMountMaterializer, K8sMountSource};
use crate::output::OutputCollector;
use crate::{
    LocalSandboxProvider, Mount, MountAccess, NamespaceSandboxProvider, SandboxProvider, Source,
};

fn store(dir: &TempDir) -> Arc<FileStore> {
    Arc::new(FileStore::new(dir.path().join("store")))
}

// ──────────────────────────────────────────────────────────────────────────────
// Unit: LocalSandboxProvider
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn local_resolve_source_bytes() {
    let dir = TempDir::new().unwrap();
    let provider = LocalSandboxProvider::new(store(&dir));
    let id = provider
        .resolve_source(&Source::Bytes(bytes::Bytes::from("hello")))
        .await
        .unwrap();
    assert_eq!(id.len(), 64);
}

#[tokio::test]
async fn local_resolve_source_file() {
    let dir = TempDir::new().unwrap();
    let src_file = dir.path().join("input.txt");
    std::fs::write(&src_file, b"from file").unwrap();
    let provider = LocalSandboxProvider::new(store(&dir));
    let id = provider
        .resolve_source(&Source::File(src_file))
        .await
        .unwrap();
    assert_eq!(id.len(), 64);
}

#[tokio::test]
async fn local_resolve_source_missing_file_returns_error() {
    let dir = TempDir::new().unwrap();
    let provider = LocalSandboxProvider::new(store(&dir));
    let err = provider
        .resolve_source(&Source::File(dir.path().join("missing.txt")))
        .await
        .unwrap_err();
    assert!(
        matches!(err, crate::SandboxError::SourceNotFound { .. }),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn local_create_sandbox_materializes_mounts() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let provider = LocalSandboxProvider::new(Arc::clone(&s));
    let id = s.put(b"content A").await.unwrap();
    let mount = Mount::new(id, "sub/a.txt");
    let sandbox = provider.create_sandbox(&[mount]).await.unwrap();
    let target = sandbox.join("sub/a.txt");
    assert!(target.exists(), "mounted file should exist at sub/a.txt");
    assert_eq!(std::fs::read(target).unwrap(), b"content A");
}

#[tokio::test]
async fn local_create_sandbox_empty_mounts() {
    let dir = TempDir::new().unwrap();
    let provider = LocalSandboxProvider::new(store(&dir));
    let sandbox = provider.create_sandbox(&[]).await.unwrap();
    assert!(sandbox.path().is_dir());
}

#[tokio::test]
async fn local_realize_mount_after_create() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let provider = LocalSandboxProvider::new(Arc::clone(&s));
    let sandbox = provider.create_sandbox(&[]).await.unwrap();
    let id = s.put(b"late mount").await.unwrap();
    provider
        .realize_mount(&sandbox, &Mount::new(id, "late.txt"))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(sandbox.join("late.txt")).unwrap(),
        b"late mount"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Unit: NamespaceSandboxProvider
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn namespace_resolve_source_bytes() {
    let dir = TempDir::new().unwrap();
    let provider = NamespaceSandboxProvider::new(store(&dir), "tenant-a", dir.path().join("ns"));
    let id = provider
        .resolve_source(&Source::Bytes(bytes::Bytes::from("ns content")))
        .await
        .unwrap();
    assert_eq!(id.len(), 64);
}

#[tokio::test]
async fn namespace_create_sandbox_is_scoped_under_namespace_dir() {
    let dir = TempDir::new().unwrap();
    let ns_root = dir.path().join("ns");
    let provider = NamespaceSandboxProvider::new(store(&dir), "tenant-b", ns_root.clone());
    let sandbox = provider.create_sandbox(&[]).await.unwrap();
    assert!(
        sandbox.path().starts_with(ns_root.join("tenant-b")),
        "sandbox should be under ns_root/tenant-b, got: {}",
        sandbox.path().display()
    );
}

#[tokio::test]
async fn namespace_create_sandbox_with_mounts() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let provider = NamespaceSandboxProvider::new(Arc::clone(&s), "tenant-c", dir.path().join("ns"));
    let id = s.put(b"namespaced content").await.unwrap();
    let sandbox = provider
        .create_sandbox(&[Mount::new(id, "out.txt")])
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(sandbox.join("out.txt")).unwrap(),
        b"namespaced content"
    );
}

#[tokio::test]
async fn namespace_isolates_separate_namespaces() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let ns_root = dir.path().join("ns");
    let provider_a = NamespaceSandboxProvider::new(Arc::clone(&s), "ns-a", ns_root.clone());
    let provider_b = NamespaceSandboxProvider::new(Arc::clone(&s), "ns-b", ns_root.clone());
    let sb_a = provider_a.create_sandbox(&[]).await.unwrap();
    let sb_b = provider_b.create_sandbox(&[]).await.unwrap();
    assert_ne!(
        sb_a.path().parent().unwrap(),
        sb_b.path().parent().unwrap(),
        "sandboxes from different namespaces must not share a parent directory"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Unit: MountAccess
// ──────────────────────────────────────────────────────────────────────────────

#[test]
fn mount_access_default_is_read_only() {
    let id = "a".repeat(64);
    let mount = Mount::new(id.clone(), "file.txt");
    assert_eq!(mount.access, MountAccess::ReadOnly);

    let mount_rw = Mount::read_write(id.clone(), "file.txt");
    assert_eq!(mount_rw.access, MountAccess::ReadWrite);

    let mount_with = Mount::new(id, "file.txt").with_access(MountAccess::ReadWrite);
    assert_eq!(mount_with.access, MountAccess::ReadWrite);
}

// ──────────────────────────────────────────────────────────────────────────────
// Unit: DockerMountMaterializer
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn docker_materialize_produces_staged_file() {
    let dir = TempDir::new().unwrap();
    let s = Arc::new(FileStore::new(dir.path().join("store")));
    let id = s.put(b"docker content").await.unwrap();

    let staging = dir.path().join("staging");
    let materializer = DockerMountMaterializer::new(Arc::clone(&s), &staging);
    let mount = Mount::new(id.clone(), "/container/file.txt");
    let bind = materializer.materialize_one(&mount).await.unwrap();

    assert!(bind.host_path.exists(), "staged file must exist on host");
    assert_eq!(std::fs::read(&bind.host_path).unwrap(), b"docker content");
    assert_eq!(bind.container_path.to_str().unwrap(), "/container/file.txt");
    assert!(bind.read_only, "default access must be read-only");
}

#[tokio::test]
async fn docker_bind_spec_read_only() {
    let dir = TempDir::new().unwrap();
    let s = Arc::new(FileStore::new(dir.path().join("store")));
    let id = s.put(b"ro").await.unwrap();
    let staging = dir.path().join("staging");
    let materializer = DockerMountMaterializer::new(Arc::clone(&s), &staging);
    let mount = Mount::new(id, "/app/config");
    let bind = materializer.materialize_one(&mount).await.unwrap();
    let spec = bind.bind_spec();
    assert!(
        spec.ends_with(":ro"),
        "read-only bind must end with ':ro': {spec}"
    );
    assert!(
        spec.contains(":/app/config"),
        "spec must contain container path: {spec}"
    );
}

#[tokio::test]
async fn docker_bind_spec_read_write() {
    let dir = TempDir::new().unwrap();
    let s = Arc::new(FileStore::new(dir.path().join("store")));
    let id = s.put(b"rw").await.unwrap();
    let staging = dir.path().join("staging");
    let materializer = DockerMountMaterializer::new(Arc::clone(&s), &staging);
    let mount = Mount::read_write(id, "/app/data");
    let bind = materializer.materialize_one(&mount).await.unwrap();
    let spec = bind.bind_spec();
    assert!(
        !spec.ends_with(":ro"),
        "read-write bind must not end with ':ro': {spec}"
    );
    assert!(
        spec.contains(":/app/data"),
        "spec must contain container path: {spec}"
    );
}

#[tokio::test]
async fn docker_materialize_batch() {
    let dir = TempDir::new().unwrap();
    let s = Arc::new(FileStore::new(dir.path().join("store")));
    let id_a = s.put(b"file-a").await.unwrap();
    let id_b = s.put(b"file-b").await.unwrap();
    let staging = dir.path().join("staging");
    let materializer = DockerMountMaterializer::new(Arc::clone(&s), &staging);
    let mounts = vec![
        Mount::new(id_a, "/a/file.txt"),
        Mount::read_write(id_b, "/b/file.txt"),
    ];
    let binds = materializer.materialize(&mounts).await.unwrap();
    assert_eq!(binds.len(), 2);
    assert_eq!(std::fs::read(&binds[0].host_path).unwrap(), b"file-a");
    assert_eq!(std::fs::read(&binds[1].host_path).unwrap(), b"file-b");
    assert!(binds[0].read_only);
    assert!(!binds[1].read_only);
}

#[tokio::test]
async fn docker_materialize_idempotent() {
    let dir = TempDir::new().unwrap();
    let s = Arc::new(FileStore::new(dir.path().join("store")));
    let id = s.put(b"same content").await.unwrap();
    let staging = dir.path().join("staging");
    let materializer = DockerMountMaterializer::new(Arc::clone(&s), &staging);
    let mount = Mount::new(id.clone(), "/x");
    let bind1 = materializer.materialize_one(&mount).await.unwrap();
    let bind2 = materializer.materialize_one(&mount).await.unwrap();
    assert_eq!(
        bind1.host_path, bind2.host_path,
        "same content must map to same staged path"
    );
}

// ──────────────────────────────────────────────────────────────────────────────
// Unit: K8sMountMaterializer
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn k8s_blob_mount_produces_configmap_and_empty_dir() {
    let dir = TempDir::new().unwrap();
    let s = Arc::new(FileStore::new(dir.path().join("store")));
    let id = s.put(b"k8s blob content").await.unwrap();

    let materializer = K8sMountMaterializer::new(Arc::clone(&s));
    let mounts = vec![K8sMount {
        name: "my-file".to_owned(),
        source: K8sMountSource::Blob { content_id: id },
        mount_path: "/app/config.json".to_owned(),
        read_only: true,
    }];
    let spec = materializer.prepare(&mounts).await.unwrap();

    assert_eq!(spec.config_maps.len(), 1, "one ConfigMap per blob mount");
    let cm = &spec.config_maps[0];
    assert_eq!(cm.api_version, "v1");
    assert_eq!(cm.kind, "ConfigMap");
    assert!(
        cm.metadata.name.contains("my-file"),
        "ConfigMap name must reference mount name"
    );
    assert!(
        cm.data.contains_key("content"),
        "ConfigMap must have 'content' key"
    );

    // emptyDir + cfgvol = 2 volumes for blob mounts
    assert_eq!(
        spec.volumes.len(),
        2,
        "blob mount needs emptyDir + configmap-vol volumes"
    );
    let empty_vol = spec
        .volumes
        .iter()
        .find(|v| v.empty_dir.is_some())
        .expect("must have an emptyDir volume");
    assert!(empty_vol.name.contains("my-file"));

    assert_eq!(
        spec.init_containers.len(),
        1,
        "one initContainer per blob mount"
    );
    let ic = &spec.init_containers[0];
    assert!(ic.name.contains("my-file"));
    assert_eq!(ic.image, "busybox:stable");
    assert!(!ic.command.is_empty());

    assert_eq!(spec.volume_mounts.len(), 1);
    let vm = &spec.volume_mounts[0];
    assert_eq!(vm.mount_path, "/app/config.json");
    assert_eq!(vm.read_only, Some(true));
    assert_eq!(vm.sub_path.as_deref(), Some("content"));
}

#[tokio::test]
async fn k8s_secret_mount_produces_no_configmap() {
    let dir = TempDir::new().unwrap();
    let s = Arc::new(FileStore::new(dir.path().join("store")));
    let materializer = K8sMountMaterializer::new(Arc::clone(&s));
    let mounts = vec![K8sMount {
        name: "db-creds".to_owned(),
        source: K8sMountSource::Secret {
            secret_name: "my-secret".to_owned(),
        },
        mount_path: "/run/secrets".to_owned(),
        read_only: true,
    }];
    let spec = materializer.prepare(&mounts).await.unwrap();

    assert!(
        spec.config_maps.is_empty(),
        "Secret mount must not create ConfigMaps"
    );
    assert!(
        spec.init_containers.is_empty(),
        "Secret mount must not add initContainers"
    );
    assert_eq!(spec.volumes.len(), 1);
    let vol = &spec.volumes[0];
    assert!(vol.secret.is_some(), "volume must use secretVolume source");
    assert_eq!(vol.secret.as_ref().unwrap().secret_name, "my-secret");
    assert_eq!(spec.volume_mounts.len(), 1);
    assert_eq!(spec.volume_mounts[0].mount_path, "/run/secrets");
    assert_eq!(spec.volume_mounts[0].read_only, Some(true));
}

#[tokio::test]
async fn k8s_configmap_mount_produces_no_extra_resources() {
    let dir = TempDir::new().unwrap();
    let s = Arc::new(FileStore::new(dir.path().join("store")));
    let materializer = K8sMountMaterializer::new(Arc::clone(&s));
    let mounts = vec![K8sMount {
        name: "app-config".to_owned(),
        source: K8sMountSource::ConfigMap {
            config_map_name: "app-settings".to_owned(),
        },
        mount_path: "/etc/app".to_owned(),
        read_only: false,
    }];
    let spec = materializer.prepare(&mounts).await.unwrap();

    assert!(spec.config_maps.is_empty());
    assert!(spec.init_containers.is_empty());
    assert_eq!(spec.volumes.len(), 1);
    let vol = &spec.volumes[0];
    assert!(vol.config_map.is_some());
    assert_eq!(vol.config_map.as_ref().unwrap().name, "app-settings");
    assert_eq!(spec.volume_mounts.len(), 1);
    assert_eq!(spec.volume_mounts[0].mount_path, "/etc/app");
    assert!(
        spec.volume_mounts[0].read_only.is_none(),
        "rw mounts omit readOnly"
    );
}

#[tokio::test]
async fn k8s_blob_configmap_content_decodes_to_original() {
    let dir = TempDir::new().unwrap();
    let s = Arc::new(FileStore::new(dir.path().join("store")));
    let original = b"\x00\x01\xfe\xff binary content";
    let id = s.put(original).await.unwrap();
    let materializer = K8sMountMaterializer::new(Arc::clone(&s));
    let mounts = vec![K8sMount {
        name: "bin".to_owned(),
        source: K8sMountSource::Blob { content_id: id },
        mount_path: "/bin/tool".to_owned(),
        read_only: true,
    }];
    let spec = materializer.prepare(&mounts).await.unwrap();
    let encoded = &spec.config_maps[0].data["content"];
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap();
    assert_eq!(
        decoded, original,
        "ConfigMap content must round-trip through base64"
    );
}

#[tokio::test]
async fn k8s_mixed_mounts() {
    let dir = TempDir::new().unwrap();
    let s = Arc::new(FileStore::new(dir.path().join("store")));
    let id = s.put(b"mixed blob").await.unwrap();
    let materializer = K8sMountMaterializer::new(Arc::clone(&s));
    let mounts = vec![
        K8sMount {
            name: "blob".to_owned(),
            source: K8sMountSource::Blob { content_id: id },
            mount_path: "/data/blob".to_owned(),
            read_only: true,
        },
        K8sMount {
            name: "secret".to_owned(),
            source: K8sMountSource::Secret {
                secret_name: "tls-cert".to_owned(),
            },
            mount_path: "/tls".to_owned(),
            read_only: true,
        },
        K8sMount {
            name: "cm".to_owned(),
            source: K8sMountSource::ConfigMap {
                config_map_name: "nginx-cfg".to_owned(),
            },
            mount_path: "/etc/nginx".to_owned(),
            read_only: false,
        },
    ];
    let spec = materializer.prepare(&mounts).await.unwrap();
    assert_eq!(
        spec.config_maps.len(),
        1,
        "only the blob mount produces a ConfigMap"
    );
    assert_eq!(
        spec.init_containers.len(),
        1,
        "only the blob mount needs an initContainer"
    );
    // blob: emptyDir + cfgvol = 2 vols; secret: 1 vol; cm: 1 vol → total 4
    assert_eq!(spec.volumes.len(), 4);
    assert_eq!(spec.volume_mounts.len(), 3, "one VolumeMount per K8sMount");
}

// ──────────────────────────────────────────────────────────────────────────────
// Unit: OutputCollector — collect_dir
// ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn output_collect_dir_single_file() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let out_dir = dir.path().join("output");
    std::fs::create_dir(&out_dir).unwrap();
    std::fs::write(out_dir.join("result.txt"), b"hello output").unwrap();

    let collector = OutputCollector::new(Arc::clone(&s));
    let artifacts = collector.collect_dir(&out_dir).await.unwrap();

    assert_eq!(artifacts.len(), 1);
    let a = &artifacts[0];
    assert_eq!(a.name, "result.txt");
    assert_eq!(a.id.len(), 64);

    let content = collector.read_artifact(&a.id).await.unwrap();
    assert_eq!(content.as_ref(), b"hello output");
}

#[tokio::test]
async fn output_collect_dir_nested() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let out_dir = dir.path().join("output");
    std::fs::create_dir_all(out_dir.join("sub")).unwrap();
    std::fs::write(out_dir.join("a.txt"), b"a").unwrap();
    std::fs::write(out_dir.join("sub/b.txt"), b"b").unwrap();

    let collector = OutputCollector::new(Arc::clone(&s));
    let mut artifacts = collector.collect_dir(&out_dir).await.unwrap();
    artifacts.sort_by(|x, y| x.name.cmp(&y.name));

    assert_eq!(artifacts.len(), 2);
    assert_eq!(artifacts[0].name, "a.txt");
    assert_eq!(artifacts[1].name, "sub/b.txt");

    let ca = collector.read_artifact(&artifacts[0].id).await.unwrap();
    assert_eq!(ca.as_ref(), b"a");
    let cb = collector.read_artifact(&artifacts[1].id).await.unwrap();
    assert_eq!(cb.as_ref(), b"b");
}

#[tokio::test]
async fn output_collect_dir_empty() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let out_dir = dir.path().join("output");
    std::fs::create_dir(&out_dir).unwrap();

    let collector = OutputCollector::new(s);
    let artifacts = collector.collect_dir(&out_dir).await.unwrap();
    assert!(
        artifacts.is_empty(),
        "empty directory should yield no artifacts"
    );
}

#[tokio::test]
async fn output_collect_dir_content_hash_consistent() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let out_dir = dir.path().join("output");
    std::fs::create_dir(&out_dir).unwrap();
    let content = b"deterministic content";
    std::fs::write(out_dir.join("file.bin"), content).unwrap();

    let collector = OutputCollector::new(Arc::clone(&s));
    let a1 = collector.collect_dir(&out_dir).await.unwrap();
    let a2 = collector.collect_dir(&out_dir).await.unwrap();

    assert_eq!(a1[0].id, a2[0].id, "same content must yield same id");
    let retrieved = collector.read_artifact(&a1[0].id).await.unwrap();
    assert_eq!(retrieved.as_ref(), content);
}

// ──────────────────────────────────────────────────────────────────────────────
// Unit: OutputCollector — collect_tar (Docker tar fallback)
// ──────────────────────────────────────────────────────────────────────────────

fn make_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut ar = tar::Builder::new(&mut buf);
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            ar.append_data(&mut header, name, std::io::Cursor::new(data))
                .unwrap();
        }
        ar.finish().unwrap();
    }
    buf
}

#[tokio::test]
async fn output_collect_tar_single_entry() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let tar_data = make_tar(&[("output/result.json", b"{\"answer\":42}")]);

    let collector = OutputCollector::new(Arc::clone(&s));
    let artifacts = collector.collect_tar(&tar_data).await.unwrap();

    assert_eq!(artifacts.len(), 1);
    let a = &artifacts[0];
    assert_eq!(a.name, "output/result.json");

    let content = collector.read_artifact(&a.id).await.unwrap();
    assert_eq!(content.as_ref(), b"{\"answer\":42}");
}

#[tokio::test]
async fn output_collect_tar_multiple_entries() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let tar_data = make_tar(&[("a.txt", b"aaa"), ("sub/b.bin", b"\x00\x01\x02")]);

    let collector = OutputCollector::new(Arc::clone(&s));
    let mut artifacts = collector.collect_tar(&tar_data).await.unwrap();
    artifacts.sort_by(|x, y| x.name.cmp(&y.name));

    assert_eq!(artifacts.len(), 2);
    assert_eq!(artifacts[0].name, "a.txt");
    assert_eq!(artifacts[1].name, "sub/b.bin");

    let ca = collector.read_artifact(&artifacts[0].id).await.unwrap();
    assert_eq!(ca.as_ref(), b"aaa");
    let cb = collector.read_artifact(&artifacts[1].id).await.unwrap();
    assert_eq!(cb.as_ref(), b"\x00\x01\x02");
}

#[tokio::test]
async fn output_collect_tar_leading_dot_slash_stripped() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    // Docker `docker cp` often produces tar entries prefixed with ./
    let tar_data = make_tar(&[("./result.txt", b"docker cp output")]);

    let collector = OutputCollector::new(s);
    let artifacts = collector.collect_tar(&tar_data).await.unwrap();

    assert_eq!(artifacts.len(), 1);
    assert_eq!(
        artifacts[0].name, "result.txt",
        "leading ./ must be stripped from tar entry names"
    );
}

#[tokio::test]
async fn output_collect_tar_empty_archive() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let tar_data = make_tar(&[]);

    let collector = OutputCollector::new(s);
    let artifacts = collector.collect_tar(&tar_data).await.unwrap();
    assert!(artifacts.is_empty());
}

#[tokio::test]
async fn output_collect_tar_hash_matches_dir_hash() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let content = b"same bytes either way";

    // Collect from directory
    let out_dir = dir.path().join("output");
    std::fs::create_dir(&out_dir).unwrap();
    std::fs::write(out_dir.join("file.txt"), content).unwrap();
    let collector = OutputCollector::new(Arc::clone(&s));
    let dir_artifacts = collector.collect_dir(&out_dir).await.unwrap();

    // Collect from tar with same content
    let tar_data = make_tar(&[("file.txt", content)]);
    let tar_artifacts = collector.collect_tar(&tar_data).await.unwrap();

    assert_eq!(
        dir_artifacts[0].id, tar_artifacts[0].id,
        "same content ingested via dir or tar must produce the same content id"
    );
}

#[tokio::test]
async fn output_read_artifact_not_found_returns_error() {
    let dir = TempDir::new().unwrap();
    let s = store(&dir);
    let collector = OutputCollector::new(s);
    let bad_id = "a".repeat(64);
    let err = collector.read_artifact(&bad_id).await.unwrap_err();
    assert!(
        matches!(err, crate::SandboxError::FileStore(_)),
        "missing id must return FileStore error, got: {err}"
    );
}
