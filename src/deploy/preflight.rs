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
    /// Whether this deploy renders a Server (issue #8): `supports.long_lived()
    /// && target.serves(cfg)`, computed once in `deploy::run`. Gates
    /// `check_servable`/`check_auth`. A target that can host a Server but
    /// whose overlay omits it (Kubernetes without `serve:`) deploys no HTTP
    /// surface, so refusing it for `shareable: false` or a missing
    /// `allow_anonymous` would protect a door that doesn't exist.
    pub serves: bool,
    pub allow_anonymous: bool,
    pub profile: &'a str,
    /// `config::builds_no_snapshot(&transforms)` — see `DeployContext::
    /// all_bound`'s doc comment for why this is threaded in from
    /// `deploy::run` rather than derived here: resolved transforms
    /// themselves have no other reader on this path (every check that used
    /// to read `transforms` directly now reads this instead), so carrying
    /// the slice just to compute one bool at the one call site would be
    /// dead weight.
    pub all_bound: bool,
}

/// Validate the deploy invariants every backend shares (§7/§8) and refuse with an
/// actionable error before anything is applied. Server-specific checks are gated
/// on this deploy actually rendering the long-lived workload (`serves`, issue
/// #8) — unchanged in strictness whenever a Server *is* rendered.
pub fn check(i: &PreflightInput) -> Result<()> {
    check_remote_storage(i)?;
    check_no_catalog(i)?;
    check_no_all_never(i)?;
    check_no_interactive_connections(i)?;
    if i.serves {
        check_servable(i)?;
        check_auth(i)?;
    }
    Ok(())
}

/// Issue #6/#11: a cell with no materializing transforms (every export
/// bound, under the binding model, or none declared) commits no snapshot at
/// all — `run` already refuses it, before BEGIN (see `engine::run`). That
/// alone still refuses a **Builder** for this cell: a Scheduled-only target
/// would crash-loop on every scheduled `run`. It no longer refuses a
/// **Server** — the founder decided `/context` is a real payload for an
/// all-bound cell (declared columns/grain/prose, live-verified against the
/// bound source — `datamk verify` writes `.cell/source_check.json` for
/// exactly this), so a target that can host the long-lived workload
/// (`supports.long_lived()`) is deployable here regardless.
///
/// A target reporting `Workloads::Both` (Kubernetes) still needs its own,
/// target-specific refusal of `schedule:` set together with an all-bound
/// cell — this agnostic check has no visibility into a target's own
/// topology config (`schedule:` lives in `deploy/<profile>.yaml`'s
/// target-specific block, e.g. `KubernetesConfig`) to refuse that
/// combination itself. See `targets::kubernetes::preflight::
/// check_no_schedule_for_an_all_bound_cell`.
fn check_no_all_never(i: &PreflightInput) -> Result<()> {
    if i.all_bound && !i.supports.long_lived() {
        bail!(
            "cell '{c}' has no materialized exports (no materializing transforms) and this \
             target cannot host the long-lived Server (`datamk serve`) — there is nothing this \
             target could ever run for this cell: no rows to build on a schedule, and no \
             `/context` to serve either. Publish the context document from CI instead:\n  \
             datamk verify  -f cell.yaml -p {p}\n  \
             datamk context -f cell.yaml -p {p} --out context.json",
            c = i.def.cell,
            p = i.profile,
        );
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
        //
        // Issue #6/#11: for an all-bound cell, "open endpoint" is not a
        // lesser risk than a materializing one just because no rows are
        // served — `/context` IS the payload here: every declared column
        // name, grain, and description (which can itself name upstream
        // fields, e.g. a customer's `email` column) is exactly what an
        // anonymous caller gets. Named explicitly so the decision is made
        // with that in view, not skipped as "there's no data anyway."
        let virtual_cell_clause = if i.all_bound {
            " This cell has no materializing transforms — every export is bound rather than \
             served from a snapshot — but that makes it *more* worth this decision, not less: \
             `/context` (declared columns, grain, and descriptions, which can themselves name \
             upstream fields) is the entire payload an anonymous caller would get."
        } else {
            ""
        };
        bail!(
            "cell '{c}' is shareable with empty `access.roles`: this deploys an open, \
             unauthenticated endpoint.{virtual_cell_clause}\n\
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
        transforms: &[crate::config::ResolvedTransform],
        profile: &'a str,
        allow_anonymous: bool,
    ) -> PreflightInput<'a> {
        input_for(
            def,
            bindings,
            transforms,
            profile,
            allow_anonymous,
            Workloads::Both,
        )
    }

    fn input_for<'a>(
        def: &'a CellDef,
        bindings: &'a ResolvedBindings,
        transforms: &[crate::config::ResolvedTransform],
        profile: &'a str,
        allow_anonymous: bool,
        supports: Workloads,
    ) -> PreflightInput<'a> {
        PreflightInput {
            def,
            bindings,
            supports,
            // Mirrors `deploy::run`: a Server is rendered iff the target can
            // host one and the overlay asks for it; every existing test here
            // models a serving overlay.
            serves: supports.long_lived(),
            allow_anonymous,
            profile,
            all_bound: crate::config::builds_no_snapshot(transforms),
        }
    }

    fn loaded(profile: &str) -> crate::config::LoadedCell {
        crate::config::load(Path::new("test/integrations/orders/cell.yaml"), profile).unwrap()
    }

    #[test]
    fn local_profile_is_refused_for_storage() {
        let l = loaded("local");
        let err = check(&input(&l.def, &l.bindings, &l.transforms, "local", true))
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
        check(&input(&l.def, &l.bindings, &l.transforms, "prod", true)).unwrap();
    }

    // issue #6/#11: a target that CANNOT host the long-lived Server at all
    // (Scheduled-only, e.g. a future Airflow/Dagster target) still refuses
    // an all-bound cell outright — there is nothing that target could ever
    // run for it: no rows to build on a schedule, and no `/context` to
    // serve either.
    #[test]
    fn all_bound_cell_is_refused_for_a_scheduled_only_target() {
        let l = loaded("prod");
        let def: CellDef = serde_yaml::from_str("cell: t\ninterface: []\n").unwrap();
        let no_transforms = crate::config::resolve_transforms(&def.transforms).unwrap();
        let err = check(&input_for(
            &l.def,
            &l.bindings,
            &no_transforms,
            "prod",
            true,
            Workloads::Scheduled,
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("has no materialized exports"), "got: {err}");
        assert!(err.contains("no materializing transforms"), "got: {err}");
        assert!(
            err.contains("cannot host the long-lived Server"),
            "got: {err}"
        );
        assert!(err.contains("datamk verify"), "got: {err}");
        assert!(err.contains("datamk context"), "got: {err}");
    }

    // issue #6/#11 (the deploy relax): a target that CAN host the Server
    // (Kubernetes reports `Workloads::Both`) is deployable for an all-bound
    // cell — `check_no_all_never` no longer refuses it, and the rest of the
    // agnostic pre-flight (servable, auth) passes for a genuinely servable
    // all-bound cell.
    #[test]
    fn all_bound_cell_passes_for_a_target_that_can_host_the_server() {
        let l = loaded("prod");
        let def: CellDef = serde_yaml::from_str(
            "cell: t\n\
             interface:\n\
             \x20 - name: e\n\
             \x20   version: 1.0.0\n\
             \x20   bind: raw\n\
             \x20   schema:\n\
             \x20     id: integer\n\
             sources:\n\
             \x20 raw: ./raw.csv\n\
             access:\n\
             \x20 shareable: true\n",
        )
        .unwrap();
        let no_transforms = crate::config::resolve_transforms(&def.transforms).unwrap();
        check(&input(&def, &l.bindings, &no_transforms, "prod", true)).unwrap();
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
        let err = check(&input(&l.def, &bindings, &l.transforms, "prod", true))
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
        check(&input(&l.def, &bindings, &l.transforms, "prod", true)).unwrap();
    }

    /// Issue #8: a Builder-only deploy (Kubernetes overlay with `schedule:`
    /// and no `serve:`) renders no HTTP surface, so the servable/auth checks
    /// don't apply — `shareable: false` and a missing `allow_anonymous` are
    /// both fine. Everything else in the agnostic pre-flight still runs.
    #[test]
    fn builder_only_deploy_skips_the_servable_and_auth_checks() {
        let l = loaded("prod");
        let mut def = l.def.clone();
        def.access.shareable = false;
        let mut i = input(&def, &l.bindings, &l.transforms, "prod", false);
        // Sanity: with a Server this is refused twice over.
        assert!(check(&i).is_err());
        i.serves = false;
        check(&i).unwrap();

        // …but a local store is still refused — `serves` only gates the
        // Server-specific checks.
        let local = loaded("local");
        let mut i = input(&local.def, &local.bindings, &local.transforms, "local", false);
        i.serves = false;
        let err = check(&i).unwrap_err().to_string();
        assert!(err.contains("is local"), "got: {err}");
    }

    #[test]
    fn open_endpoint_refused_without_allow_anonymous() {
        let l = loaded("prod");
        let err = check(&input(&l.def, &l.bindings, &l.transforms, "prod", false))
            .unwrap_err()
            .to_string();
        assert!(err.contains("open, unauthenticated endpoint"), "got: {err}");
        // orders materializes normally — not the all-bound clause.
        assert!(
            !err.contains("entire payload"),
            "a materializing cell must not get the all-bound wording: got: {err}"
        );
    }

    /// Issue #6/#11: the same open-endpoint refusal, but naming the
    /// document-is-the-payload point for an all-bound cell — `/context`
    /// (declared columns/grain/descriptions) is exactly what an anonymous
    /// caller gets, even though no rows are ever served.
    #[test]
    fn open_endpoint_refused_names_the_document_as_payload_for_an_all_bound_cell() {
        let l = loaded("prod");
        let def: CellDef = serde_yaml::from_str(
            "cell: t\n\
             interface:\n\
             \x20 - name: e\n\
             \x20   version: 1.0.0\n\
             \x20   bind: raw\n\
             \x20   schema:\n\
             \x20     id: integer\n\
             sources:\n\
             \x20 raw: ./raw.csv\n\
             access:\n\
             \x20 shareable: true\n",
        )
        .unwrap();
        let no_transforms = crate::config::resolve_transforms(&def.transforms).unwrap();
        let err = check(&input(&def, &l.bindings, &no_transforms, "prod", false))
            .unwrap_err()
            .to_string();
        assert!(err.contains("open, unauthenticated endpoint"), "got: {err}");
        assert!(
            err.contains("entire payload"),
            "an all-bound cell's open-endpoint refusal must name /context as the payload: \
             got: {err}"
        );
        assert!(err.contains("allow_anonymous"), "got: {err}");
    }
}
