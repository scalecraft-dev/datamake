use anyhow::{bail, Result};

use crate::config::{
    is_remote, CellDef, ResolvedBindings, ResolvedConnection, ResolvedSource, SnowflakeAuth,
};
use crate::deploy::target::Workloads;

/// Inputs for the target-agnostic pre-flight. All resolved without a database:
/// `bindings` comes from the pure `config::resolve`.
pub struct PreflightInput<'a> {
    pub def: &'a CellDef,
    pub bindings: &'a ResolvedBindings,
    pub supports: Workloads,
    pub allow_anonymous: bool,
    pub profile: &'a str,
}

/// Validate the deploy invariants every backend shares (§7/§8) and refuse with an
/// actionable error before anything is applied. Server-specific checks are gated
/// on the target hosting the long-lived workload.
pub fn check(i: &PreflightInput) -> Result<()> {
    check_remote_storage(i)?;
    check_no_catalog(i)?;
    check_no_interactive_connections(i)?;
    if i.supports.long_lived() {
        check_servable(i)?;
        check_auth(i)?;
    }
    Ok(())
}

/// A deployed workload has no browser: a snowflake connection using
/// `authenticator: externalbrowser` (the local-dev SSO shape) would hang the
/// Builder pod waiting for an interactive login. Refused here, where it's
/// knowable offline, instead of as a wedged init Job.
fn check_no_interactive_connections(i: &PreflightInput) -> Result<()> {
    for (name, src) in &i.bindings.sources {
        if let ResolvedSource::Connection {
            connection,
            config:
                ResolvedConnection::Snowflake {
                    auth: SnowflakeAuth::ExternalBrowser { .. },
                    ..
                },
            ..
        } = src
        {
            bail!(
                "source '{name}' uses snowflake connection '{connection}' with `authenticator: \
                 externalbrowser`, which needs an interactive browser login — a deployed \
                 workload has none.\n\
                 In profiles/{p}.yaml, switch this connection to key-pair auth \
                 (`private_key_path:` pointing at a service account's key mounted in the pod).",
                p = i.profile,
            );
        }
    }
    Ok(())
}

/// §7: a deployed workload can't reach a `./.cell` / local-file object store.
fn check_remote_storage(i: &PreflightInput) -> Result<()> {
    if !is_remote(&i.bindings.storage) {
        bail!(
            "profile '{p}' storage `{s}` is local; a deployed workload can't reach it.\n\
             Point `storage:` in profiles/{p}.yaml at a shared object store (s3://… or gs://…).",
            p = i.profile,
            s = i.bindings.storage,
        );
    }
    Ok(())
}

/// ADR 0004 §11: a deployed cell has no separate catalog — it derives from
/// `storage` and publishes an immutable artifact per execution. *Any*
/// `catalog:` value is rejected (a DSN is the superseded shared-live model; a
/// file path is unreachable from a pod).
fn check_no_catalog(i: &PreflightInput) -> Result<()> {
    if let Some(c) = &i.bindings.catalog {
        bail!(
            "deploy: profiles/{p}.yaml sets `catalog:` ({c}), but a deployed cell derives its \
             catalog from `storage` and publishes an immutable catalog artifact per execution — \
             it has no separate catalog DSN.\n\
             Remove the `catalog:` line; `storage` is a deployed cell's only external dependency. \
             See ADR 0004.",
            p = i.profile,
        );
    }
    Ok(())
}

/// §7: a cell that refuses every request or exposes nothing is a dead Server.
fn check_servable(i: &PreflightInput) -> Result<()> {
    if !i.def.access.shareable {
        bail!(
            "cell '{c}' won't serve: `access.shareable` is false, so `serve` rejects every request.\n\
             Set `access.shareable: true` in cell.yaml to deploy a Server.",
            c = i.def.cell,
        );
    }
    if i.def.interface.is_empty() {
        bail!(
            "cell '{c}' has an empty `interface:` — there's nothing to serve.\n\
             Declare at least one export in cell.yaml before deploying.",
            c = i.def.cell,
        );
    }
    Ok(())
}

/// §8: auth must be safely configured. Either roles are set and a principals path
/// is wired, or the endpoint is open and that was a deliberate, reviewed decision.
///
/// The agnostic layer only checks that a principals **path is configured** — the
/// file at that path is the in-cluster secret mount, unreadable from the deploy
/// host. The `serve` `load_principals` hardening (§8) is the runtime backstop
/// that catches a missing/malformed file where it actually lives.
fn check_auth(i: &PreflightInput) -> Result<()> {
    let roles = &i.def.access.roles;
    if !roles.is_empty() {
        if i.bindings.principals.is_none() {
            bail!(
                "cell '{c}' sets `access.roles: [{r}]`, but profile '{p}' has no `principals:` — \
                 `serve` would deny every request.\n\
                 Set `principals:` in profiles/{p}.yaml to the path your token→roles secret is mounted at.",
                c = i.def.cell,
                r = roles.join(", "),
                p = i.profile,
            );
        }
    } else if !i.allow_anonymous {
        // shareable is guaranteed true here (check_servable ran first): an open,
        // unauthenticated endpoint. Require an explicit opt-in.
        bail!(
            "cell '{c}' is shareable with empty `access.roles`: this deploys an open, \
             unauthenticated endpoint.\n\
             If that's intended, set `allow_anonymous: true` in deploy/{p}.yaml; otherwise add \
             `access.roles:` to cell.yaml.",
            c = i.def.cell,
            p = i.profile,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CellDef;
    use std::path::Path;

    fn input<'a>(
        def: &'a CellDef,
        bindings: &'a ResolvedBindings,
        profile: &'a str,
        allow_anonymous: bool,
    ) -> PreflightInput<'a> {
        PreflightInput {
            def,
            bindings,
            supports: Workloads::Both,
            allow_anonymous,
            profile,
        }
    }

    fn loaded(profile: &str) -> crate::config::LoadedCell {
        crate::config::load(Path::new("test/integrations/orders/cell.yaml"), profile).unwrap()
    }

    #[test]
    fn local_profile_is_refused_for_storage() {
        let l = loaded("local");
        let err = check(&input(&l.def, &l.bindings, "local", true))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("storage `./.cell/data` is local"),
            "got: {err}"
        );
    }

    #[test]
    fn deployable_prod_profile_passes() {
        // orders is shareable+no-roles, and deploy/prod.yaml sets allow_anonymous.
        let l = loaded("prod");
        check(&input(&l.def, &l.bindings, "prod", true)).unwrap();
    }

    #[test]
    fn external_browser_snowflake_connection_is_refused() {
        let l = loaded("prod");
        let mut bindings = l.bindings.clone();
        bindings.sources.insert(
            "models".to_string(),
            ResolvedSource::Connection {
                connection: "wh".to_string(),
                config: ResolvedConnection::Snowflake {
                    account: "A".to_string(),
                    database: "D".to_string(),
                    auth: SnowflakeAuth::ExternalBrowser {
                        user: "U".to_string(),
                    },
                    warehouse: None,
                    role: None,
                },
                target: crate::config::ConnectionTarget::Table("raw.t".to_string()),
                incremental: None,
            },
        );
        let err = check(&input(&l.def, &bindings, "prod", true))
            .unwrap_err()
            .to_string();
        assert!(err.contains("externalbrowser"), "got: {err}");
        assert!(err.contains("private_key_path"), "got: {err}");
        assert!(err.contains("connection 'wh'"), "got: {err}");
    }

    #[test]
    fn keypair_snowflake_connection_passes_preflight() {
        let l = loaded("prod");
        let mut bindings = l.bindings.clone();
        bindings.sources.insert(
            "models".to_string(),
            ResolvedSource::Connection {
                connection: "wh".to_string(),
                config: ResolvedConnection::Snowflake {
                    account: "A".to_string(),
                    database: "D".to_string(),
                    auth: SnowflakeAuth::KeyPair {
                        user: "U".to_string(),
                        private_key_path: "/etc/datamk/sf-key.p8".to_string(),
                        passphrase: None,
                    },
                    warehouse: None,
                    role: None,
                },
                target: crate::config::ConnectionTarget::Table("raw.t".to_string()),
                incremental: None,
            },
        );
        check(&input(&l.def, &bindings, "prod", true)).unwrap();
    }

    #[test]
    fn open_endpoint_refused_without_allow_anonymous() {
        let l = loaded("prod");
        let err = check(&input(&l.def, &l.bindings, "prod", false))
            .unwrap_err()
            .to_string();
        assert!(err.contains("open, unauthenticated endpoint"), "got: {err}");
    }
}
