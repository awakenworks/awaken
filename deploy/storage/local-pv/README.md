# Preallocated local volumes

This is a storage-layer deployment for ordinary Kubernetes PVCs, including
Session continuation, application data and cache claims. SIG Storage's unchanged
local-static-provisioner owns PV discovery/recycling; Kubernetes owns matching
and exclusive PVC binding. Each application owns initialization, retention and
data recovery. Awaken Runtime contains no second volume pool or storage database.

## Static structure

| Owner | Responsibility |
|---|---|
| Infrastructure/operator | Prepare independent empty filesystems mounted under `/mnt/awaken-local/volume-*` on explicitly labelled nodes |
| `chart/` | Exact upstream Helm templates from tag v2.9.0; LICENSE and file identities in UPSTREAM.json |
| `values.yaml` | One reviewed class, pinned Linux amd64 image, fixed discovery directory and resource policy |
| `install.sh` | Offline rendering, existing-class admission and controller deployment |
| Kubernetes | PV/PVC matching, node affinity, binding and mount protection |
| Application | Ordinary PVC requests, data initialization and lifecycle |

## Dynamic flow

```text
infrastructure mounts empty filesystems
  -> standard controller publishes Available PVs
  -> any namespace submits an ordinary awaken-local PVC
  -> consumer scheduling selects compatible node/PV and binds it
  -> Pod mounts that exclusively bound volume
  -> Pod replacement reuses the same PVC/PV/data
  -> explicit PVC deletion releases PV
  -> standard controller cleans its contents before recreating an Available PV
```

There is one node controller (10m CPU/32Mi memory requested, 128Mi memory limit),
not one supply helper Pod per claim. Filesystem cleaning runs inside that existing
controller. Application-specific initialization, including Awaken's existing
CacheVolume marker initializer, is a separate responsibility and may use a Pod.

## Install and request

Prepare real durable filesystems before installation. Their mounted capacity must
cover the expected claim sizes; a 1Gi volume cannot satisfy an 8Gi data claim.
The controller only discovers mountpoints matching `volume-*`; it does not
partition or format operator disks. Explicitly opt eligible nodes in:

```bash
kubectl --context CONTEXT label node NODE awaken.dev/local-storage=true
bash deploy/storage/local-pv/install.sh --context CONTEXT /tmp/awaken-local-install
```

The class is not made default and does not redirect existing classes. For a new
Awaken Worker deployment, use its existing configuration seam:

```toml
k8s_continuation_volume = { storage_class_name = "awaken-local", size = "1Gi" }
```

Other applications request it normally:

```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: application-data
spec:
  storageClassName: awaken-local
  accessModes: [ReadWriteOnce]
  volumeMode: Filesystem
  resources:
    requests:
      storage: 8Gi
```

`WaitForFirstConsumer` preserves placement correctness. An early PVC stays Pending
until its consumer selects a compatible node; preallocation removes synchronous
disk supply, not the scheduler's binding transition. Pool exhaustion stays Pending,
never borrows another claim's volume. Local data requires that node's storage;
this is not cross-node replication or a durability guarantee for node loss.

Reclaim policy is Delete: explicit PVC release authorizes cleaning. Databases must
retain their claims for their intended lifetime and use their own backup/recovery.
Existing bound Sessions continue with their original Worker storage configuration;
changing that configuration changes the immutable provider adoption fingerprint.
Introduce this class on new deployments rather than rewriting old bindings.

## Tests and evidence

Cause/effect rules live beside `test_contract.py` and the generic live assertions.
The canonical k3d harness prepares real loop-backed ext4 filesystems only inside a
new uniquely owned Node container; it refuses pre-existing directories and releases
mounts during exact fixture cleanup. These thin loop-backed volumes test mechanics,
not production disk provisioning, device failure or guaranteed physical capacity.

```bash
python3 deploy/storage/local-pv/test_contract.py
npm --prefix e2e run test:local-storage:k3d
# Reuse the production real-LLM Session fixture for an optional paired sample.
AWAKEN_LAZY_STORAGE_PROFILE=local-static AWAKEN_LAZY_IMAGE_PREWARM=1 \
  AWAKEN_LAZY_STARTUP_SAMPLES=3 npm --prefix e2e run test:real:skill-lazy:k3d
```

The live test covers ordinary claims in different namespaces/capacities, node
affinity, exclusive binding, Pod replacement, controller restart, class isolation,
pool exhaustion and data cleaning before reallocation. Kubelet creates declared
writable directory subPaths with the Pod's fsGroup; Awaken adds no directory-only
init container. Consumers verify three roots as UID/GID 10001 and retain data
after Pod replacement. Historical directory init projections require their
complete original spec and digest. Empty PV preparation has no Session execution
Pod. The real-LLM fixture preserves `on_tool_use`, uses one immutable Sandbox
image and records PVC/Pod/Hand/tool clocks; results are samples, not P95.
