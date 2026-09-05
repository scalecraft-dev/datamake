//! The `target: kubernetes` deploy overlay schema (ADR 0002 §3). Pure `serde` +
//! hand-rolled shape validation — no bindings, no cluster, no I/O.

use anyhow::{bail, Result};
use serde::Deserialize;

/// The Kubernetes-specific topology, deserialized from the deploy overlay
/// (`deploy/<profile>.yaml`, ADR 0001 §6). Deliberately **not**
/// `#[serde(deny_unknown_fields)]`: the overlay also carries top-level `target`
/// and `allow_anonymous` (read by `config::DeployConfig`, ADR 0001), which this
/// struct must silently ignore rather than fail on.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct KubernetesConfig {
    #[serde(default)]
    pub(crate) namespace: Option<String>,
    /// Builder cron. Absent ⇒ serve-only, no CronJob (ADR 0002 §1).
    #[serde(default)]
    pub(crate) schedule: Option<String>,
    /// Compaction window in days for the Builder (ADR 0004 §10); rendered as
    /// `--retention-days` on the init Job and CronJob. 0 disables compaction.
    #[serde(default)]
    pub(crate) retention_days: Option<u64>,
    /// Server topology. Presence-based (issue #8, ADR 0002 §1 amendment):
    /// absent ⇒ no Deployment and no Service — a cell that exists only to be
    /// composed has no HTTP consumer. `serve: {}` deploys a Server with every
    /// default; a bare `serve:` (YAML null) is *absent*, not empty.
    #[serde(default)]
    pub(crate) serve: Option<ServeTopology>,
    #[serde(default)]
    pub(crate) image: Option<String>,
    #[serde(default, rename = "imagePullSecret")]
    pub(crate) image_pull_secret: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ServeTopology {
    #[serde(default)]
    pub(crate) port: Option<u16>,
    #[serde(default)]
    pub(crate) replicas: Option<u32>,
    /// Seconds between the Server's LATEST-pointer checks — the staleness
    /// bound for experimental "latest" routes (ADR 0004 §6). Ops tuning, so it
    /// lives here in the tracked overlay, not the secret-bearing profile.
    #[serde(default)]
    pub(crate) poll_interval: Option<u64>,
}

impl KubernetesConfig {
    /// The namespace to deploy into. Defaults to `default` — the overlay may
    /// reasonably omit it for a single-tenant cluster.
    pub(crate) fn namespace(&self) -> &str {
        self.namespace.as_deref().unwrap_or("default")
    }

    /// The Server's port, inside the cluster and on the Service. `serve`'s own
    /// CLI default (`cli.rs`) is 8080; mirrored here so an omitted overlay still
    /// renders a coherent Service + Deployment + probe port.
    pub(crate) fn port(&self) -> u16 {
        self.serve.as_ref().and_then(|s| s.port).unwrap_or(8080)
    }

    pub(crate) fn replicas(&self) -> u32 {
        self.serve.as_ref().and_then(|s| s.replicas).unwrap_or(1)
    }

    /// Mirrors `serve`'s own CLI default (`cli.rs`).
    pub(crate) fn poll_interval(&self) -> u64 {
        self.serve.as_ref().and_then(|s| s.poll_interval).unwrap_or(15)
    }

    /// Whether this overlay deploys a Server at all (issue #8): `serve:` is
    /// present. Read by the target-agnostic pre-flight (via
    /// `DeployTarget::serves`) to gate the Server-only checks, and by
    /// `manifests()` to decide whether a Service + Deployment render.
    pub(crate) fn serves(&self) -> bool {
        self.serve.is_some()
    }

    /// Mirrors `run`'s own CLI default (`cli.rs`).
    pub(crate) fn retention_days(&self) -> u64 {
        self.retention_days.unwrap_or(30)
    }

    /// The image to run. Defaults to this binary's own version — the base image
    /// (ADR 0001 §5) is cell-agnostic and versioned alongside datamk itself.
    /// Image tags mirror the git tag (`v` prefix); CARGO_PKG_VERSION is bare
    /// semver, so the prefix is added here.
    pub(crate) fn image_ref(&self) -> String {
        self.image.clone().unwrap_or_else(|| {
            format!(
                "ghcr.io/scalecraft-dev/datamk:v{}",
                env!("CARGO_PKG_VERSION")
            )
        })
    }

    /// Shape-validate the overlay. Pure: no bindings, no cluster access — a typo'd
    /// namespace or a malformed cron string is caught before anything renders.
    ///
    /// NOT checked here: `serve.replicas > 1 ⇒ catalog must be postgres`. That's a
    /// **bindings** cross-check (this struct never sees `ResolvedBindings`) and
    /// belongs in the Kubernetes pre-flight (ADR 0002 step 3), not this pure
    /// schema validation.
    pub(crate) fn validate(&self) -> Result<()> {
        if let Some(ns) = &self.namespace {
            if !is_dns_label(ns) {
                bail!(
                    "kubernetes overlay `namespace: {ns}` is not a valid DNS-1123 label \
                     (1-63 lowercase alphanumeric/'-' characters, starting and ending \
                     alphanumeric)"
                );
            }
        }

        if let Some(schedule) = &self.schedule {
            let fields = schedule.split_whitespace().count();
            if fields != 5 {
                bail!(
                    "kubernetes overlay `schedule: \"{schedule}\"` doesn't look like a 5-field \
                     cron expression (minute hour day month weekday); got {fields} field(s)"
                );
            }
        }

        if let Some(serve) = &self.serve {
            if serve.port == Some(0) {
                bail!("kubernetes overlay `serve.port` must be non-zero");
            }

            if serve.poll_interval == Some(0) {
                bail!("kubernetes overlay `serve.poll_interval` must be non-zero (seconds)");
            }
        }

        // Issue #8: with neither block the render would be a ConfigMap and a
        // one-shot init Job — nothing runs after the first build, which is
        // never what anyone meant. `serve: {}` is named explicitly because a
        // bare `serve:` parses as YAML null, i.e. absent.
        if self.serve.is_none() && self.schedule.is_none() {
            bail!(
                "kubernetes overlay defines neither `serve:` nor `schedule:` — that renders only \
                 a ConfigMap and a one-shot init Job, so nothing runs after the first build.\n\
                 Add `serve: {{}}` to deploy the Server (`datamk serve`), or \
                 `schedule: \"0 * * * *\"` to deploy the Builder CronJob (`datamk run`), or both."
            );
        }

        if let Some(secret) = &self.image_pull_secret {
            if !is_dns_subdomain(secret) {
                bail!(
                    "kubernetes overlay `imagePullSecret: {secret}` is not a valid Kubernetes \
                     object name (lowercase alphanumeric, '.', '-', at most 253 characters)"
                );
            }
        }

        Ok(())
    }
}

/// DNS-1123 label: 1-63 characters, lowercase alphanumeric or `-`, starting and
/// ending with an alphanumeric character. Hand-rolled (no regex crate) — the
/// alphabet is small enough that a byte scan reads at least as clearly.
fn is_dns_label(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes.len() > 63 {
        return false;
    }
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    alnum(bytes[0]) && alnum(bytes[bytes.len() - 1]) && bytes.iter().all(|&b| alnum(b) || b == b'-')
}

/// DNS-1123 subdomain shape: at most 253 characters, lowercase alphanumeric,
/// `.`, or `-`. A basic sanity check on a referenced Secret **name** — the
/// stricter per-label start/end rule isn't worth hand-rolling twice for a value
/// that a real cluster will reject outright if malformed anyway.
fn is_dns_subdomain(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> KubernetesConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn a_good_config_parses_and_validates() {
        let k8s = parse(
            r#"
namespace: data-prod
schedule: "0 * * * *"
serve:
  port: 9090
  replicas: 3
image: ghcr.io/acme/datamk:1.2.3
imagePullSecret: regcred
"#,
        );
        k8s.validate().unwrap();
        assert_eq!(k8s.namespace(), "data-prod");
        assert_eq!(k8s.port(), 9090);
        assert_eq!(k8s.replicas(), 3);
        assert_eq!(k8s.image_ref(), "ghcr.io/acme/datamk:1.2.3");
    }

    #[test]
    fn defaults_apply_when_the_overlay_omits_everything_but_serve() {
        let k8s = parse("target: kubernetes\nserve: {}\n");
        k8s.validate().unwrap();
        assert!(k8s.serves());
        assert_eq!(k8s.namespace(), "default");
        assert_eq!(k8s.port(), 8080);
        assert_eq!(k8s.replicas(), 1);
        assert_eq!(k8s.poll_interval(), 15);
        // Image tags mirror the git tag: v-prefixed semver.
        assert!(k8s
            .image_ref()
            .starts_with("ghcr.io/scalecraft-dev/datamk:v"));
    }

    /// Issue #8: `serve:` is presence-based, mirroring `schedule:`.
    #[test]
    fn serve_alone_and_schedule_alone_are_both_valid() {
        let serve_only = parse("serve:\n  replicas: 2\n");
        serve_only.validate().unwrap();
        assert!(serve_only.serves());
        assert!(serve_only.schedule.is_none());

        let schedule_only = parse("schedule: \"0 * * * *\"\n");
        schedule_only.validate().unwrap();
        assert!(!schedule_only.serves());
        // Accessors still answer with defaults — nothing reads them without
        // a Server, but they must not panic.
        assert_eq!(schedule_only.port(), 8080);
    }

    /// Issue #8: a bare `serve:` is YAML null ⇒ absent, NOT an empty block.
    /// Pinned so nobody "fixes" the footgun by treating null as `{}`; the
    /// validate message names `serve: {}` for exactly this reason.
    #[test]
    fn bare_serve_key_is_absent_not_empty() {
        let k8s = parse("serve:\nschedule: \"0 * * * *\"\n");
        assert!(!k8s.serves());
        k8s.validate().unwrap();
    }

    #[test]
    fn neither_serve_nor_schedule_is_rejected() {
        for yaml in ["target: kubernetes\n", "serve:\n", "namespace: data\n"] {
            let err = parse(yaml).validate().unwrap_err().to_string();
            assert!(err.contains("neither `serve:` nor `schedule:`"), "{yaml:?} -> {err}");
            assert!(err.contains("serve: {}"), "{yaml:?} -> {err}");
            assert!(err.contains("schedule:"), "{yaml:?} -> {err}");
        }
    }

    #[test]
    fn unknown_top_level_keys_are_ignored() {
        // `target` and `allow_anonymous` belong to config::DeployConfig, not this
        // struct — they must not fail parsing here.
        let k8s = parse(
            r#"
target: kubernetes
allow_anonymous: true
namespace: data-prod
serve: {}
"#,
        );
        k8s.validate().unwrap();
        assert_eq!(k8s.namespace(), "data-prod");
    }

    #[test]
    fn bad_namespace_is_rejected() {
        for bad in ["Data-Prod", "-data", "data-", "", "UPPER", "has_underscore"] {
            let k8s = KubernetesConfig {
                namespace: Some(bad.to_string()),
                serve: Some(ServeTopology::default()),
                ..Default::default()
            };
            let err = k8s.validate().unwrap_err().to_string();
            assert!(err.contains("DNS-1123 label"), "'{bad}' -> {err}");
        }
    }

    #[test]
    fn bad_cron_is_rejected() {
        for bad in ["* * * *", "@daily", "* * * * * *", ""] {
            let k8s = KubernetesConfig {
                schedule: Some(bad.to_string()),
                ..Default::default()
            };
            let err = k8s.validate().unwrap_err().to_string();
            assert!(err.contains("5-field cron expression"), "'{bad}' -> {err}");
        }
    }

    #[test]
    fn port_zero_is_rejected() {
        let k8s = KubernetesConfig {
            serve: Some(ServeTopology {
                port: Some(0),
                replicas: None,
                poll_interval: None,
            }),
            ..Default::default()
        };
        let err = k8s.validate().unwrap_err().to_string();
        assert!(err.contains("non-zero"), "got: {err}");
    }

    #[test]
    fn bad_image_pull_secret_is_rejected() {
        for bad in ["Reg-Cred", "reg_cred", &"a".repeat(254)] {
            let k8s = KubernetesConfig {
                image_pull_secret: Some(bad.to_string()),
                serve: Some(ServeTopology::default()),
                ..Default::default()
            };
            let err = k8s.validate().unwrap_err().to_string();
            assert!(
                err.contains("not a valid Kubernetes object name"),
                "'{bad}' -> {err}"
            );
        }
    }
}
