//! Live skill discovery cache for a Session-owned remote hand.

use awaken_provisioning_contract as pc;
use awaken_sandbox_local::DiscoveredSkillFile;

pub(crate) struct ContainerSkillCache {
    dirs: std::sync::Mutex<std::collections::BTreeSet<String>>,
    snapshot: std::sync::RwLock<ContainerSkillSnapshot>,
}

enum ContainerSkillSnapshot {
    Ready(std::collections::BTreeMap<String, Vec<DiscoveredSkillFile>>),
    Unavailable(String),
}

impl Default for ContainerSkillCache {
    fn default() -> Self {
        Self {
            dirs: std::sync::Mutex::new(std::collections::BTreeSet::new()),
            snapshot: std::sync::RwLock::new(ContainerSkillSnapshot::Ready(Default::default())),
        }
    }
}

impl ContainerSkillCache {
    pub(super) fn register(&self, subdir: &str) {
        self.dirs.lock().unwrap().insert(subdir.to_string());
    }

    pub(super) fn get(&self, subdir: &str) -> Result<Vec<DiscoveredSkillFile>, pc::SandboxError> {
        self.register(subdir);
        match &*self.snapshot.read().unwrap() {
            ContainerSkillSnapshot::Ready(files) => {
                Ok(files.get(subdir).cloned().unwrap_or_default())
            }
            ContainerSkillSnapshot::Unavailable(error) => Err(pc::SandboxError::new(format!(
                "container Skill catalog is unavailable: {error}"
            ))),
        }
    }

    pub(super) fn invalidate(&self, error: impl Into<String>) {
        *self.snapshot.write().unwrap() = ContainerSkillSnapshot::Unavailable(error.into());
    }

    pub(super) async fn refresh(
        &self,
        sandbox: &dyn awaken_sandbox_container::ContainerEnvironment,
    ) -> Result<(), pc::SandboxError> {
        if let ContainerSkillSnapshot::Unavailable(error) = &*self.snapshot.read().unwrap() {
            return Err(pc::SandboxError::new(format!(
                "container Skill catalog is unavailable: {error}"
            )));
        }
        let dirs: Vec<_> = self.dirs.lock().unwrap().iter().cloned().collect();
        let discovered = async {
            let mut next = std::collections::BTreeMap::new();
            for subdir in dirs {
                let root = super::container_files::workspace_path(&subdir)?;
                let mut files = Vec::new();
                for file in sandbox.read_files(&root).await? {
                    let Some((id, name)) = file.path.split_once('/') else {
                        continue;
                    };
                    if name != "SKILL.md" || id.is_empty() || id.contains('/') {
                        continue;
                    }
                    let content = String::from_utf8(file.bytes).map_err(|error| {
                        pc::SandboxError::new(format!("Skill `{id}` is not UTF-8: {error}"))
                    })?;
                    files.push(DiscoveredSkillFile {
                        id: id.to_string(),
                        content,
                        dir: format!("{subdir}/{id}"),
                    });
                }
                files.sort_by(|left, right| left.id.cmp(&right.id));
                next.insert(subdir, files);
            }
            Ok::<_, pc::SandboxError>(next)
        }
        .await;
        match discovered {
            Ok(files) => {
                *self.snapshot.write().unwrap() = ContainerSkillSnapshot::Ready(files);
                Ok(())
            }
            Err(error) => {
                self.invalidate(error.to_string());
                Err(error)
            }
        }
    }
}
