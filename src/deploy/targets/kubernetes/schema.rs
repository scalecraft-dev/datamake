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
    #[serde(default)]
    pub(crate) serve: ServeTopology,
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
        self.serve.port.unwrap_or(8080)
    }

    pub(crate) fn replicas(&self) -> u32 {
        self.serve.replicas.unwrap_or(1)
    }

    /// Mirrors `serve`'s own CLI default (`cli.rs`).
    pub(crate) fn poll_interval(&self) -> u64 {
        self.serve.poll_interval.unwrap_or(15)
    }

    /// Mirrors `run`'s own CLI default (`cli.rs`).
    pub(crate) fn retention_days(&self) -> u64 {
        self.retention_days.unwrap_or(30)
    }

    /// The image to run. Defaults to this binary's own version — the base image
    /// (ADR 0001 §5) is cell-agnostic and versioned alongside datamk itself.
    pub(crate) fn image_ref(&self) -> String {
        self.image.clone().unwrap_or_else(|| {
            format!(
                "ghcr.io/scalecraft-dev/datamk:{}",
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

        if self.serve.port == Some(0) {
            bail!("kubernetes overlay `serve.port` must be non-zero");
        }

        if self.serve.poll_interval == Some(0) {
            bail!("kubernetes overlay `serve.poll_interval` must be non-zero (seconds)");
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
    fn defaults_apply_when_the_overlay_omits_everything() {
        let k8s = parse("target: kubernetes\n");
        k8s.validate().unwrap();
        assert_eq!(k8s.namespace(), "default");
        assert_eq!(k8s.port(), 8080);
        assert_eq!(k8s.replicas(), 1);
        assert!(k8s
            .image_ref()
            .starts_with("ghcr.io/scalecraft-dev/datamk:"));
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
            serve: ServeTopology {
                port: Some(0),
                replicas: None,
                poll_interval: None,
            },
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
