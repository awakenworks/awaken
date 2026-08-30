use super::*;

#[async_trait]
impl pc::Sandbox for NamespaceSandbox {
    fn id(&self) -> &str {
        &self.id
    }

    fn handle(&self) -> pc::SandboxHandle {
        if let Some(handle) = &self.adopted_handle {
            return handle.clone();
        }
        let previous = pc::NamespaceSandboxHandleV1 {
            outputs_path: self.outputs_path.clone(),
            base_env: self.base_env.clone(),
            network: self.network.clone(),
            control_services: self.control_services.clone(),
        };
        match self.realization.current() {
            Some(realization) => pc::SandboxHandle::namespace_v2(
                NamespaceProvider::provider_kind(),
                &self.id,
                pc::NamespaceSandboxHandleV2 {
                    previous,
                    realization_fingerprint: realization.fingerprint().clone(),
                    effect_fence: realization.effect_fence().clone(),
                    physical_incarnation: realization.physical_incarnation().to_owned(),
                    owned_paths: self
                        .owned_paths
                        .lock()
                        .expect("owned paths lock poisoned")
                        .clone(),
                },
            )
            .with_memory_materializations(
                self.memory_materializations
                    .lock()
                    .expect("Memory materializations lock poisoned")
                    .clone(),
            )
            .expect("Namespace sandbox cached canonical Memory materialization evidence"),
            // Preserve decode-only V1 compatibility without inventing current
            // provider realization or Repository ownership evidence.
            None => {
                pc::SandboxHandle::namespace(NamespaceProvider::provider_kind(), &self.id, previous)
            }
        }
    }

    async fn spawn(
        &self,
        command: pc::Command,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        self.require_root_identity()?;
        let stdio = command.stdio;
        let command = self.materialize_command(command).await?;
        let argv = self.render_argv(&command)?;
        let mut cmd = TokioCommand::new(&argv[0]);
        awaken_local_process::configure_process_group(&mut cmd);
        cmd.args(&argv[1..]);
        self.configure_command(&mut cmd, &command)?;
        let (out, e) = match stdio {
            pc::Stdio::Inherit => (ProcStdio::inherit(), ProcStdio::inherit()),
            pc::Stdio::Piped => (ProcStdio::piped(), ProcStdio::piped()),
            pc::Stdio::Null => (ProcStdio::null(), ProcStdio::null()),
        };
        cmd.stdout(out).stderr(e);
        let child = cmd.spawn().map_err(err)?;
        Ok(Box::new(LocalProcess::spawned(child)))
    }

    async fn attach(
        &self,
        req: pc::MountRequirement,
    ) -> Result<pc::RealizedMount, pc::SandboxError> {
        let root_identity = self.require_root_identity()?;
        // Dynamic mount decision table:
        // Inline bytes + RO/RW -> materialize and add one bind;
        // any source requiring an external resolver -> reject without layout change.
        pc::validate_mount_requirements(
            std::slice::from_ref(&req),
            &NamespaceProvider::capabilities(),
        )
        .map_err(err)?;
        let (destination_exists, live_memory_exists) = {
            let layout = self.layout.read().expect("namespace layout lock poisoned");
            (
                layout.iter().any(|mount| mount.dest == req.mount_path),
                layout.iter().any(|mount| {
                    mount.dest == req.mount_path
                        && mount.boundary == RenderMountBoundary::ManagedMemoryStore
                }),
            )
        };
        let recovered_copy_exists = self
            .memory_materializations
            .lock()
            .expect("Memory materializations lock poisoned")
            .iter()
            .any(|evidence| evidence.mount_path == req.mount_path);
        if live_memory_exists
            || recovered_copy_exists
            || (destination_exists && matches!(req.source, pc::MountSource::MemoryStore { .. }))
        {
            return Err(err(
                "runtime Memory attachment cannot replace an existing mount participant",
            ));
        }
        let host = host_projection_path(&self.root, &self.host_workspace, &req.mount_path)?;
        let mut acquired_mounts = Vec::new();
        let memory =
            realize_memory_mount(&self.memory_mounter, &req, &host, &mut acquired_mounts).await;
        let memory = match memory {
            Ok(memory) => memory,
            Err(cause) => {
                self.memory_mounts.lock().await.extend(acquired_mounts);
                return Err(cause);
            }
        };
        if let Some((rendered, realized, materialization)) = memory {
            // Publish the guard and its evidence under the same lock order used
            // by reconciliation acknowledgement, so no snapshot can drain an
            // unrepresented Copy participant.
            let mut memory_mounts = self.memory_mounts.lock().await;
            let mut memory_materializations = self
                .memory_materializations
                .lock()
                .map_err(|_| err("Memory materializations lock poisoned"))?;
            memory_mounts.extend(acquired_mounts);
            let mut layout = self.layout.write().expect("namespace layout lock poisoned");
            layout.retain(|mount| mount.dest != req.mount_path);
            layout.push(rendered);
            if let Some(materialization) = materialization {
                memory_materializations.push(materialization);
            }
            return Ok(realized);
        }
        let (contents, content_hash) = match &req.source {
            pc::MountSource::Inline { contents } => (contents.as_bytes(), None),
            pc::MountSource::InlineBytes {
                contents,
                content_hash,
            } => (contents.as_slice(), content_hash.clone()),
            _ => {
                return Err(err(format!(
                    "runtime attach for mount {:?} requires a provider-owned resolver",
                    req.mount_id
                )));
            }
        };
        verify(&req.source, contents)?;
        let relative = host
            .strip_prefix(self.root.root())
            .map_err(|_| err("namespace attachment path escaped its sandbox root"))?;
        awaken_sandbox_fs::write_relative_file_atomic(
            self.root.root(),
            root_identity,
            relative,
            contents,
            0o600,
        )
        .map_err(err)?;
        let mut layout = self.layout.write().expect("namespace layout lock poisoned");
        layout.retain(|mount| mount.dest != req.mount_path);
        layout.push(RenderMount {
            host,
            dest: req.mount_path.clone(),
            read_only: req.access == pc::MountAccess::ReadOnly,
            boundary: RenderMountBoundary::General,
        });
        Ok(pc::RealizedMount {
            mount_id: req.mount_id,
            mount_path: req.mount_path,
            access: req.access,
            realization: pc::Realization::Bind,
            content_hash,
        })
    }

    async fn artifacts(&self) -> Result<Vec<pc::Artifact>, pc::SandboxError> {
        let Some(identity) = self.root_identity_for_access()? else {
            return Ok(Vec::new());
        };
        Ok(
            crate::artifacts::scan_outputs(&self.root, identity, &self.outputs_path)?
                .into_iter()
                .map(|(artifact, _)| artifact)
                .collect(),
        )
    }

    async fn read_artifact(&self, id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        let Some(identity) = self.root_identity_for_access()? else {
            return Err(err(format!("no artifact with id {id:?}")));
        };
        for (artifact, bytes) in
            crate::artifacts::scan_outputs(&self.root, identity, &self.outputs_path)?
        {
            if artifact.id == id {
                return Ok(bytes);
            }
        }
        Err(err(format!("no artifact with id {id:?}")))
    }

    fn realized(&self) -> &[pc::RealizedMount] {
        &self.realized
    }

    async fn process(
        &self,
        _process_id: &str,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        Err(err(
            "local namespace tier cannot reattach to a process across owners",
        ))
    }

    async fn status(&self) -> Result<pc::SandboxStatus, pc::SandboxError> {
        Ok(if self.root_identity_for_access()?.is_some() {
            pc::SandboxStatus::Ready
        } else {
            pc::SandboxStatus::Terminated
        })
    }

    async fn renew_lease(&self) -> Result<(), pc::SandboxError> {
        Ok(())
    }

    async fn dispose(&self) -> Result<(), pc::SandboxError> {
        if self.realization.current().is_some() {
            return Err(err(
                "current durable sandbox disposal requires an aggregate effect fence",
            ));
        }
        self.control_publication.close_for_dispose().await;
        self.release_memory_mounts().await?;
        self.shred_secrets()?;
        crate::realization_marker::dispose_legacy(
            self.root.root(),
            self.realization.legacy_live_identity(),
        )
    }

    async fn acknowledge_memory_reconciliation(
        &self,
        effect_fence: &pc::SandboxEffectFence,
        complete_materializations: &[pc::MemoryMaterializationEvidence],
    ) -> Result<(), pc::SandboxError> {
        crate::acknowledge_memory_reconciliation(
            &self.memory_reconciliation_ack,
            &self.memory_mounts,
            effect_fence,
            complete_materializations,
            || {
                let handle = pc::Sandbox::handle(self);
                Ok(handle
                    .memory_materializations()?
                    .unwrap_or_default()
                    .to_vec())
            },
            || {
                crate::authorize_terminal_disposal(
                    &self.realization_root,
                    &self.realization,
                    &self.terminal_removal,
                    effect_fence,
                )
                .map(|_| ())
            },
        )
        .await
    }

    async fn prepare_disposal_for_effect(
        &self,
        effect_fence: &pc::SandboxEffectFence,
    ) -> Result<pc::SandboxEffectFence, pc::SandboxError> {
        let handle = pc::Sandbox::handle(self);
        let complete_materializations = handle.memory_materializations()?.unwrap_or_default();
        crate::prepare_terminal_disposal(
            &self.memory_reconciliation_ack,
            complete_materializations,
            &self.realization_root,
            &self.realization,
            &self.terminal_removal,
            effect_fence,
        )
    }

    async fn dispose_for_effect(
        &self,
        authorization: &pc::SandboxDisposalAuthorization,
    ) -> Result<(), pc::SandboxError> {
        crate::dispose_terminal_realization(
            &self.memory_mounts,
            &self.realization_root,
            &self.secret_paths,
            &self.terminal_removal,
            authorization,
        )
        .await?;
        self.control_publication.close_for_dispose().await;
        Ok(())
    }
}
