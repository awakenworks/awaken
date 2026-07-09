//! Tenancy scope tree — `org` ⊃ `workspace` — as plain persistent
//! entities behind a [`scope::ScopeRepo`] port (in-memory reference here; SQL
//! backends in the adapter layer). Authorization is a cross-cutting aspect at
//! the edge; tenancy is strictly Org → Workspace (no Project tier).
//!
//! These are tenancy config, not aggregates whose history is
//! load-bearing, so they are plain rows: `update` mutates in place and a delete
//! sets an `archived` flag. The domain services own the business rules (slug
//! validity, parent liveness, slug-conflict → typed rejection); the repo owns
//! only per-parent slug uniqueness among live entities.
//!
//! # Provenance & seam
//! Vendored serde-only from `awaken-flow`'s `awaken-flow-work` scope tree (both
//! repos Apache-2.0), the shape kept faithful so this crate can later be
//! promoted verbatim into the shared `awaken-foundation` and depended on by
//! both products. Only the scope tree is vendored; `awaken-flow-work`'s
//! fact-sourced `process`/`subject` families are out of scope here.
//!
//! The authorization coordinate (`ScopeRef`) lives in `awaken-iam`, not here:
//! an [`scope::Entity`] is translated to a `ScopeRef` only at the authz ACL
//! boundary, so this domain never depends on the authz engine.

pub mod org;
pub mod scope;
pub mod workspace;

#[cfg(test)]
mod tests {
    use crate::org::{self, CreateOrg, UpdateOrg};
    use crate::scope::{InMemoryScopeRepo, Rejection, ScopeRepo, Status};
    use crate::workspace::{self, CreateWorkspace};

    fn repo() -> InMemoryScopeRepo {
        InMemoryScopeRepo::new()
    }

    fn org(repo: &dyn ScopeRepo, id: &str, slug: &str) {
        org::create_org(repo, cmd_org(id, slug)).unwrap();
    }

    fn workspace(repo: &dyn ScopeRepo, id: &str, org: &str, slug: &str) {
        workspace::create_workspace(repo, cmd_ws(id, org, slug)).unwrap();
    }

    #[test]
    fn org_create_read_update_archive() {
        let r = repo();
        org(&r, "o1", "acme");
        assert_eq!(org::get_org(&r, "o1").unwrap().unwrap().slug, "acme");

        org::update_org(
            &r,
            UpdateOrg {
                id: "o1".into(),
                name: Some("Acme Inc".into()),
                description: Some("the org".into()),
            },
        )
        .unwrap();
        let got = org::get_org(&r, "o1").unwrap().unwrap();
        assert_eq!(got.name, "Acme Inc");
        assert_eq!(got.description.as_deref(), Some("the org"));

        org::archive_org(&r, "o1").unwrap();
        assert_eq!(
            org::get_org(&r, "o1").unwrap().unwrap().status,
            Status::Archived
        );
        assert!(org::list_orgs(&r).unwrap().is_empty());
    }

    #[test]
    fn org_rejections() {
        let r = repo();
        org(&r, "o1", "acme");
        assert!(matches!(
            org::create_org(&r, cmd_org("o2", "acme")).unwrap_err(),
            Rejection::SlugTaken { .. }
        ));
        assert!(matches!(
            org::create_org(&r, cmd_org("o3", "Bad Slug")).unwrap_err(),
            Rejection::InvalidSlug { .. }
        ));
        assert_eq!(
            org::create_org(
                &r,
                CreateOrg {
                    id: "o4".into(),
                    name: "  ".into(),
                    slug: "ok".into(),
                    description: None
                }
            )
            .unwrap_err(),
            Rejection::EmptyName
        );
        assert!(org::get_org(&r, "ghost").unwrap().is_none());
    }

    #[test]
    fn archive_frees_the_slug() {
        let r = repo();
        org(&r, "o1", "acme");
        org::archive_org(&r, "o1").unwrap();
        org(&r, "o2", "acme");
        assert_eq!(org::list_orgs(&r).unwrap().len(), 1);
    }

    #[test]
    fn workspace_shards_slug_by_org() {
        let r = repo();
        org(&r, "o1", "org-a");
        org(&r, "o2", "org-b");
        workspace(&r, "w1", "o1", "eng");
        workspace(&r, "w2", "o2", "eng");
        assert!(matches!(
            workspace::create_workspace(&r, cmd_ws("w3", "o1", "eng")).unwrap_err(),
            Rejection::SlugTaken { .. }
        ));
        assert_eq!(workspace::list_workspaces(&r, "o1").unwrap().len(), 1);
        assert_eq!(workspace::list_workspaces(&r, "o2").unwrap().len(), 1);
        assert_eq!(
            workspace::get_workspace(&r, "w1").unwrap().unwrap().org,
            "o1"
        );
    }

    #[test]
    fn workspace_under_missing_or_archived_org_is_rejected() {
        let r = repo();
        assert_eq!(
            workspace::create_workspace(&r, cmd_ws("w1", "ghost", "eng")).unwrap_err(),
            Rejection::ParentMissing
        );
        org(&r, "o1", "org");
        org::archive_org(&r, "o1").unwrap();
        assert_eq!(
            workspace::create_workspace(&r, cmd_ws("w1", "o1", "eng")).unwrap_err(),
            Rejection::ParentArchived
        );
    }

    #[test]
    fn update_and_archive_are_tier_safe() {
        let r = repo();
        org(&r, "o1", "org");
        workspace(&r, "w1", "o1", "eng");
        assert_eq!(
            org::update_org(
                &r,
                UpdateOrg {
                    id: "w1".into(),
                    name: Some("hijack".into()),
                    description: None
                }
            )
            .unwrap_err(),
            Rejection::NotFound
        );
        assert_eq!(org::archive_org(&r, "w1").unwrap_err(), Rejection::NotFound);
    }

    fn cmd_org(id: &str, slug: &str) -> CreateOrg {
        CreateOrg {
            id: id.into(),
            name: format!("Org {id}"),
            slug: slug.into(),
            description: None,
        }
    }

    fn cmd_ws(id: &str, org: &str, slug: &str) -> CreateWorkspace {
        CreateWorkspace {
            id: id.into(),
            org: org.into(),
            name: format!("Workspace {id}"),
            slug: slug.into(),
            description: None,
        }
    }
}
