// `raw` is consumed by a target backend; with no target feature compiled it has
// no reader. The default build (kubernetes on) keeps the strict lint.
#![cfg_attr(not(feature = "kubernetes"), allow(dead_code))]

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

/// Orchestrators `deploy` can target. Only `kubernetes` is implemented; new
/// targets are additive (a cargo feature + a trait impl), not a change here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum Target {
    Kubernetes,
}

/// Deploy topology for one profile, from the tracked `deploy/<profile>.yaml`
/// overlay (§6). Distinct from `Bindings` (the gitignored, secret-bearing
/// profile): this overlay has no secret fields by design, so a credential can't
/// be committed to it. Loaded **only** by the deploy command, so target schema
/// and weight never reach the `run`/`serve` parse paths.
#[derive(Debug, Clone)]
pub struct DeployConfig {
    /// `None` lets pre-flight detect "no target configured and `--target` not
    /// passed" and refuse with a teaching error.
    pub target: Option<Target>,
    /// The deliberate, reviewed acknowledgement that an open, unauthenticated
    /// endpoint is intended (§8b). Read at the overlay's **top level** so the
    /// agnostic pre-flight never has to parse a target's sub-schema.
    pub allow_anonymous: bool,
    /// The full parsed overlay. The chosen backend deserializes its own
    /// target-specific topology (namespace, schedule, serve.*, image, …) from
    /// this, keeping that schema out of `config`.
    pub raw: serde_yaml::Value,
}

impl DeployConfig {
    /// Read `deploy/<profile>.yaml`. Mirrors `Bindings::load`'s voice; the
    /// not-found error teaches that topology is tracked, not in the profile.
    pub fn load(dir: &Path, profile: &str) -> Result<Self> {
        let path = dir.join("deploy").join(format!("{profile}.yaml"));
        let text = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "reading deploy overlay {} (create it; deploy topology is tracked, not in the profile)",
                path.display()
            )
        })?;
        let raw: serde_yaml::Value = serde_yaml::from_str(&text)
            .with_context(|| format!("parsing deploy overlay {}", path.display()))?;

        let target = match raw.get("target") {
            Some(v) => Some(
                serde_yaml::from_value::<Target>(v.clone())
                    .with_context(|| format!("`target` in deploy overlay {}", path.display()))?,
            ),
            None => None,
        };
        let allow_anonymous = raw
            .get("allow_anonymous")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Ok(DeployConfig {
            target,
            allow_anonymous,
            raw,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_target_and_allow_anonymous() {
        let dir = Path::new("test/integrations/orders");
        let cfg = DeployConfig::load(dir, "prod").unwrap();
        assert_eq!(cfg.target, Some(Target::Kubernetes));
        assert!(cfg.allow_anonymous);
    }

    #[test]
    fn missing_overlay_errors_with_guidance() {
        let dir = Path::new("test/integrations/orders");
        let err = DeployConfig::load(dir, "does-not-exist")
            .unwrap_err()
            .to_string();
        assert!(err.contains("deploy topology is tracked"), "got: {err}");
    }
}
