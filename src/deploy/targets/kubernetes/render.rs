//! Pure manifest rendering (ADR 0002 §4/§5/§7): cell + config → typed
//! `k8s-openapi` structs → YAML. No `kube`, no async, no cluster access —
//! everything here is a deterministic function of its inputs, which is what
//! makes it heavily unit-testable without a cluster (or even a `kube` client).
//!
//! Typed structs (not `DynamicObject`) so a field rename in `k8s-openapi` is a
//! compile error here, not a silently-wrong manifest.

use anyhow::{Context, Result};
use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::batch::v1::{CronJob, CronJobSpec, Job, JobSpec, JobTemplateSpec};
use k8s_openapi::api::core::v1::{
    ConfigMap, ConfigMapVolumeSource, Container, ContainerPort, HTTPGetAction, KeyToPath,
    LocalObjectReference, PodSpec, PodTemplateSpec, Probe, SecretVolumeSource, Service,
    ServicePort, ServiceSpec, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use super::schema::KubernetesConfig;
use crate::deploy::artifact::{ArtifactFile, CellArtifact};
use crate::deploy::target::RenderedDoc;

/// Cell content mount (ConfigMap: `cell.yaml` + `sql/*`, ADR 0002 §4).
const CELL_MOUNT: &str = "/cell";
/// Profile mount (Secret — carries the catalog DSN + S3 creds, ADR 0002 §5).
const PROFILE_MOUNT: &str = "/cell/profiles";
/// Principals mount dir (Secret, ADR 0002 §5). `serve`'s `principals:` path
/// points here — no `src/serve/` change needed.
const PRINCIPALS_MOUNT_DIR: &str = "/etc/datamk";
/// The key/file name inside the principals Secret + mount. `PRINCIPALS_MOUNT_DIR`
/// joined with this is the full path the in-cluster profile's `principals:`
/// must equal — asserted by the cluster-side pre-flight (ADR 0002 §5/§6) via
/// `principals_mount_path` below. `pub(crate)` so the pre-flight reads the
/// *same* key it validates in the live Secret's `data`, rather than a
/// re-typed string that could drift.
pub(crate) const PRINCIPALS_FILE: &str = "principals.json";

/// The full in-cluster path the profile's `principals:` must equal
/// (`/etc/datamk/principals.json`). The pre-flight (ADR 0002 §6) checks
/// `ResolvedBindings::principals` against this so a profile pointed anywhere
/// else fails before apply, not at pod startup.
pub(crate) fn principals_mount_path() -> String {
    format!("{PRINCIPALS_MOUNT_DIR}/{PRINCIPALS_FILE}")
}

/// Everything pure rendering needs. Deliberately narrower than `DeployContext`:
/// no `ResolvedBindings` (the profile/DSN must never reach a rendered manifest —
/// see the `render_all` doc comment and the integration test that asserts
/// `postgres://` never appears in `--dry-run` stdout).
pub(crate) struct RenderInput<'a> {
    pub cell: &'a str,
    pub profile: &'a str,
    pub k8s: &'a KubernetesConfig,
    pub artifact: &'a CellArtifact,
    /// Whether `access.roles` is non-empty — mounts the principals Secret when
    /// true. Derived from `CellDef`, not `ResolvedBindings`, deliberately: it's a
    /// `cell.yaml` fact, not an environment one.
    pub has_roles: bool,
    /// The principals Secret's `resourceVersion` (or a hash of it), stamped as a
    /// pod-template annotation so rotating tokens rolls the Server (ADR 0002 §5).
    /// `None` on this pure-render path (dry-run, unit tests) — step 3's apply
    /// reads the live Secret and fills it in before rendering the Deployment.
    pub secret_checksum: Option<&'a str>,
}

/// The five typed manifests a cell's topology can produce, bundled so the async
/// apply path (ADR 0002 step 3, `kubernetes::apply`) patches the *same* typed
/// objects `--dry-run` prints — never a `DynamicObject` rebuilt from YAML,
/// which would reopen the "field rename is silently wrong" gap `render.rs`'s
/// module doc exists to close.
pub(crate) struct Manifests {
    pub(crate) configmap: ConfigMap,
    /// The one-shot Builder run (`datamk run`) apply waits on before the Server
    /// is ever applied — it initializes the DuckLake catalog so `serve`'s
    /// `READ_ONLY` attach doesn't crash-loop on a fresh catalog DB (the gap the
    /// `kind` e2e harness found; see `apply::apply_and_wait_init`).
    pub(crate) init_job: Job,
    pub(crate) service: Service,
    pub(crate) deployment: Deployment,
    pub(crate) cronjob: Option<CronJob>,
}

/// Build every manifest this cell's topology needs: ConfigMap, init Job,
/// Service, Deployment always; CronJob only when `schedule` is set (ADR 0002
/// §1).
///
/// The **profile never appears here** — it's delivered as a Secret volume
/// referenced by name (`profile_secret_name`), never embedded. Only the
/// content-addressed, secret-free `CellArtifact` and the overlay's own
/// (secret-free, ADR 0002 §3) `KubernetesConfig` feed the render.
pub(crate) fn manifests(input: &RenderInput) -> Result<Manifests> {
    Ok(Manifests {
        configmap: render_configmap(input)?,
        init_job: render_init_job(input),
        service: render_service(input),
        deployment: render_deployment(input),
        cronjob: render_cronjob(input),
    })
}

impl Manifests {
    /// Serialize to the same `ConfigMap, Job, Service, Deployment, CronJob?`
    /// order `--dry-run` has always printed in — dependency order (ADR 0002
    /// step 3: the ConfigMap must exist before anything mounts it, and the
    /// init Job must run and complete before the Server is ever applied), and
    /// incidentally also alphabetical-ish, so no unit test needs to change for
    /// this refactor.
    pub(crate) fn docs(&self) -> Result<Vec<RenderedDoc>> {
        let mut docs = Vec::with_capacity(5);
        docs.push(rendered_doc(
            "ConfigMap",
            &self.configmap.metadata,
            &self.configmap,
        )?);
        docs.push(rendered_doc(
            "Job",
            &self.init_job.metadata,
            &self.init_job,
        )?);
        docs.push(rendered_doc(
            "Service",
            &self.service.metadata,
            &self.service,
        )?);
        docs.push(rendered_doc(
            "Deployment",
            &self.deployment.metadata,
            &self.deployment,
        )?);
        if let Some(cj) = &self.cronjob {
            docs.push(rendered_doc("CronJob", &cj.metadata, cj)?);
        }
        Ok(docs)
    }
}

/// Thin wrapper over `manifests(input)?.docs()`, kept only so this module's
/// existing render unit tests (below) didn't need to change shape for the
/// step-3 typed-object refactor. Production code (`kubernetes::mod`, both the
/// dry-run and apply branches) calls `manifests()`/`docs()` directly instead,
/// since the apply branch also needs the typed objects `render_all` throws
/// away — hence `#[cfg(test)]` rather than a real production entry point.
#[cfg(test)]
pub(crate) fn render_all(input: &RenderInput) -> Result<Vec<RenderedDoc>> {
    manifests(input)?.docs()
}

fn rendered_doc<T: serde::Serialize>(
    kind: &str,
    metadata: &ObjectMeta,
    obj: &T,
) -> Result<RenderedDoc> {
    let name = metadata.name.clone().unwrap_or_default();
    let body = serde_yaml::to_string(obj)
        .with_context(|| format!("rendering {kind} manifest for '{name}'"))?;
    Ok(RenderedDoc {
        kind: kind.to_string(),
        name,
        body,
    })
}

// --- naming (ADR 0002 §4/§5) ------------------------------------------------

/// The content hash, truncated to the 12 hex chars both the ConfigMap name and
/// its `datamk.io/content-hash` label use. A Kubernetes label **value** is
/// capped at 63 bytes; the full 64-char SHA-256 hex digest blows past that by
/// one byte and a real API server rejects the object outright (`must be no
/// more than 63 bytes`) — caught only by actually applying to a cluster (the
/// `kind` e2e harness, test/integrations/kind_e2e/), never by a unit test that
/// just inspects the typed struct. One helper, used by both the name and the
/// label, so they can never drift out of sync with each other or with this
/// length limit.
fn content_hash_short(content_hash: &str) -> &str {
    &content_hash[..12]
}

/// ConfigMap name: content-hashed, so a content change is a new object rather
/// than a mutation of an existing one (ADR 0002 §4's "immutable" discipline).
fn configmap_name(cell: &str, content_hash: &str) -> String {
    format!("{cell}-{}", content_hash_short(content_hash))
}

/// Init Job name: content-addressed, same discipline as `configmap_name` — one
/// helper, so the name can never drift out of sync with the hash the ConfigMap
/// itself uses. Re-deploying identical content re-applies a Job with the same
/// name (SSA finds it already `Complete` and `apply_and_wait_init` is an
/// idempotent no-op); changed content gets a fresh Job name, so it actually
/// runs a fresh build rather than SSA-patching an immutable, already-finished
/// `spec.template`.
pub(crate) fn init_job_name(cell: &str, content_hash: &str) -> String {
    format!("{cell}-init-{}", content_hash_short(content_hash))
}

/// Named so the naming can't drift from `volumes()` above and the pre-flight
/// (ADR 0002 §6) below — both resolve the same Secret by calling this, never by
/// re-formatting the string themselves.
pub(crate) fn principals_secret_name(cell: &str) -> String {
    format!("{cell}-principals")
}

/// See `principals_secret_name` — same naming-can't-drift rationale.
pub(crate) fn profile_secret_name(cell: &str, profile: &str) -> String {
    format!("{cell}-{profile}")
}

fn app_label(cell: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert("app".to_string(), cell.to_string());
    m
}

// --- ConfigMap ---------------------------------------------------------------

/// The cell's deliverable content (ADR 0002 §4). `immutable: true` + a
/// content-hashed name close the "mounted ConfigMap is a silent mutability
/// hole" gap; `render_deployment` stamps the hash as a checksum annotation so
/// the Server actually rolls when it changes.
///
/// Keys are **sanitized** (`configmap_key`) because a ConfigMap `data` key may
/// not contain `/`; the real relative path (with its `sql/` subdir) is restored
/// at mount time by `cell_volume`'s `items[].path`.
fn render_configmap(input: &RenderInput) -> Result<ConfigMap> {
    let art = input.artifact;
    let mut data = BTreeMap::new();
    for f in artifact_files(art) {
        let key = configmap_key(&f.rel_path);
        if data.insert(key.clone(), decode_text(f)?).is_some() {
            anyhow::bail!(
                "two cell files collide on ConfigMap key '{key}' after sanitizing path \
                 separators (e.g. `a/b` and `a_b`); rename one so they differ by more than `/` vs `_`"
            );
        }
    }

    let mut labels = app_label(input.cell);
    labels.insert(
        "datamk.io/content-hash".to_string(),
        content_hash_short(&art.content_hash).to_string(),
    );

    Ok(ConfigMap {
        metadata: ObjectMeta {
            name: Some(configmap_name(input.cell, &art.content_hash)),
            namespace: Some(input.k8s.namespace().to_string()),
            labels: Some(labels),
            ..Default::default()
        },
        data: Some(data),
        immutable: Some(true),
        ..Default::default()
    })
}

/// The cell's deliverable files, in a stable order, so `render_configmap` (keys)
/// and `cell_volume` (`items` restoring paths) always agree on the same set.
fn artifact_files(art: &CellArtifact) -> Vec<&ArtifactFile> {
    let mut files = vec![&art.cell_yaml];
    files.extend(art.sql.iter());
    if let Some(pin) = &art.published {
        files.push(pin);
    }
    files
}

/// A ConfigMap `data` key must match `[-._a-zA-Z0-9]+` — notably no `/` — but a
/// cell references transforms by relative path (`sql/stg_orders.sql`). Sanitize
/// every non-conforming char to `_`; `cell_volume` maps the key back to the real
/// `path` (which *may* contain `/`), so the mount still reproduces `/cell/sql/…`.
fn configmap_key(rel_path: &str) -> String {
    rel_path
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Cell content (`cell.yaml`, `sql/*`, `.cell/published.json`) is text, so it
/// lives in `data`, not `binaryData`.
fn decode_text(f: &ArtifactFile) -> Result<String> {
    String::from_utf8(f.bytes.clone()).with_context(|| {
        format!(
            "artifact file '{}' is not valid UTF-8 text (ConfigMap `data` requires text; \
             see `binaryData` if that ever changes)",
            f.rel_path
        )
    })
}

// --- init Job (deploy-time catalog bootstrap) --------------------------------

/// The one-shot Builder run (`datamk run`) apply applies-and-waits-on before
/// the Server is ever applied (`apply::apply_and_wait_init`). Closes the
/// READ_ONLY bootstrap gap: `serve` opens DuckLake `READ_ONLY`, and DuckLake
/// refuses to auto-create a catalog under `READ_ONLY` — so on a fresh Postgres
/// the Server crash-loops until *some* Builder run initializes the catalog.
/// This makes that first Builder run part of `deploy` itself instead of an
/// undocumented ordering the operator has to know to do by hand.
///
/// Same pod plumbing as the CronJob builder (`render_cronjob`): the cell
/// ConfigMap volume (with the `items` path restore), the profile Secret, the
/// principals Secret when `has_roles`, `imagePullSecret` when set, and the
/// same `image_ref()`. `restartPolicy: Never` + `backoffLimit: 2` (kubelet
/// retries are handled here, not by a restarting container — the pod exits
/// after `datamk run` returns); `ttlSecondsAfterFinished: 3600` auto-cleans a
/// completed init Job so re-deploys don't accumulate them forever.
///
/// No checksum annotations — unlike the Server's Deployment, this Job never
/// gets mutated in place; a content change gets an entirely new, content-
/// addressed name (`init_job_name`) instead.
fn render_init_job(input: &RenderInput) -> Job {
    let container = Container {
        name: "init".to_string(),
        image: Some(input.k8s.image_ref()),
        command: Some(vec!["datamk".to_string()]),
        args: Some(builder_args(input)),
        volume_mounts: Some(volume_mounts(input)),
        ..Default::default()
    };

    let mut labels = app_label(input.cell);
    labels.insert(
        "datamk.io/content-hash".to_string(),
        content_hash_short(&input.artifact.content_hash).to_string(),
    );

    Job {
        metadata: ObjectMeta {
            name: Some(init_job_name(input.cell, &input.artifact.content_hash)),
            namespace: Some(input.k8s.namespace().to_string()),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(JobSpec {
            backoff_limit: Some(2),
            ttl_seconds_after_finished: Some(3600),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    containers: vec![container],
                    volumes: Some(volumes(input)),
                    restart_policy: Some("Never".to_string()),
                    image_pull_secrets: image_pull_secrets(input),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

// --- Service -----------------------------------------------------------------

/// ClusterIP only (ADR 0002 §7): `spec.type` is deliberately left `None` (which
/// Kubernetes defaults to `ClusterIP`). Never set it to `LoadBalancer` — a
/// freshly deployed, possibly-anonymous endpoint must not land on the public
/// network by default. `render_service_never_provisions_a_load_balancer` below
/// pins this down.
fn render_service(input: &RenderInput) -> Service {
    Service {
        metadata: ObjectMeta {
            name: Some(input.cell.to_string()),
            namespace: Some(input.k8s.namespace().to_string()),
            labels: Some(app_label(input.cell)),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: Some(app_label(input.cell)),
            ports: Some(vec![ServicePort {
                port: input.k8s.port().into(),
                target_port: Some(IntOrString::Int(input.k8s.port().into())),
                ..Default::default()
            }]),
            // type_ intentionally omitted -> ClusterIP.
            ..Default::default()
        }),
        ..Default::default()
    }
}

// --- shared pod plumbing (Deployment + CronJob) ------------------------------

/// `items` maps each sanitized ConfigMap key back to the file's real relative
/// `path` — which *may* contain `/` — so `cell.yaml` and `sql/*.sql` land at
/// `/cell/cell.yaml` and `/cell/sql/*.sql` exactly as the cell references them.
/// Without this, the sanitized keys would project as flat `/cell/sql_*.sql`
/// files and `datamk run -f /cell/cell.yaml` couldn't resolve its transforms.
fn cell_volume(input: &RenderInput) -> Volume {
    let items = artifact_files(input.artifact)
        .iter()
        .map(|f| KeyToPath {
            key: configmap_key(&f.rel_path),
            path: f.rel_path.clone(),
            mode: None,
        })
        .collect();
    Volume {
        name: "cell".to_string(),
        config_map: Some(ConfigMapVolumeSource {
            name: configmap_name(input.cell, &input.artifact.content_hash),
            items: Some(items),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn profile_volume(input: &RenderInput) -> Volume {
    Volume {
        name: "profile".to_string(),
        secret: Some(SecretVolumeSource {
            secret_name: Some(profile_secret_name(input.cell, input.profile)),
            default_mode: Some(0o400),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn principals_volume(input: &RenderInput) -> Volume {
    Volume {
        name: "principals".to_string(),
        secret: Some(SecretVolumeSource {
            secret_name: Some(principals_secret_name(input.cell)),
            default_mode: Some(0o400),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// `cell` + `profile` + `scratch` always; `principals` only when
/// `access.roles` is set (ADR 0002 §5) — an open cell has no token->roles
/// secret to mount. `scratch` is the emptyDir behind `/tmp`, where downloaded
/// catalog artifacts land (ADR 0004 §6) — explicit so the pods stay correct
/// under a future `readOnlyRootFilesystem` hardening.
fn volumes(input: &RenderInput) -> Vec<Volume> {
    let mut v = vec![
        cell_volume(input),
        profile_volume(input),
        Volume {
            name: "scratch".to_string(),
            empty_dir: Some(Default::default()),
            ..Default::default()
        },
    ];
    if input.has_roles {
        v.push(principals_volume(input));
    }
    v
}

fn volume_mounts(input: &RenderInput) -> Vec<VolumeMount> {
    let mut m = vec![
        VolumeMount {
            name: "cell".to_string(),
            mount_path: CELL_MOUNT.to_string(),
            read_only: Some(true),
            ..Default::default()
        },
        VolumeMount {
            name: "profile".to_string(),
            mount_path: PROFILE_MOUNT.to_string(),
            read_only: Some(true),
            ..Default::default()
        },
        VolumeMount {
            name: "scratch".to_string(),
            mount_path: "/tmp".to_string(),
            ..Default::default()
        },
    ];
    if input.has_roles {
        m.push(VolumeMount {
            name: "principals".to_string(),
            mount_path: PRINCIPALS_MOUNT_DIR.to_string(),
            read_only: Some(true),
            ..Default::default()
        });
    }
    m
}

fn image_pull_secrets(input: &RenderInput) -> Option<Vec<LocalObjectReference>> {
    input
        .k8s
        .image_pull_secret
        .as_ref()
        .map(|name| vec![LocalObjectReference { name: name.clone() }])
}

fn cell_yaml_path() -> String {
    format!("{CELL_MOUNT}/cell.yaml")
}

/// `datamk run` args, shared by the init Job and the CronJob builder so their
/// compaction behavior (ADR 0004 §10) can't drift apart.
fn builder_args(input: &RenderInput) -> Vec<String> {
    vec![
        "run".to_string(),
        "--file".to_string(),
        cell_yaml_path(),
        "--profile".to_string(),
        input.profile.to_string(),
        "--retention-days".to_string(),
        input.k8s.retention_days().to_string(),
    ]
}

fn http_probe(port: u16) -> Probe {
    Probe {
        http_get: Some(HTTPGetAction {
            path: Some("/".to_string()),
            port: IntOrString::Int(port.into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

// --- Deployment ---------------------------------------------------------------

/// The Server (`datamk serve`, ADR 0002 §1). `checksum/config` (and, once a
/// principals Secret exists, `checksum/secret`) are stamped on the pod template
/// so a content or token change actually rolls the Deployment — a mounted
/// ConfigMap/Secret updates the file in place without restarting the process
/// (ADR 0002 §4/§5).
fn render_deployment(input: &RenderInput) -> Deployment {
    let mut annotations = BTreeMap::new();
    annotations.insert(
        "checksum/config".to_string(),
        input.artifact.content_hash.clone(),
    );
    if let Some(secret_checksum) = input.secret_checksum {
        annotations.insert("checksum/secret".to_string(), secret_checksum.to_string());
    }

    let container = Container {
        name: "server".to_string(),
        image: Some(input.k8s.image_ref()),
        command: Some(vec!["datamk".to_string()]),
        args: Some(vec![
            "serve".to_string(),
            "--file".to_string(),
            cell_yaml_path(),
            "--profile".to_string(),
            input.profile.to_string(),
            "--port".to_string(),
            input.k8s.port().to_string(),
            "--poll-interval".to_string(),
            input.k8s.poll_interval().to_string(),
        ]),
        ports: Some(vec![ContainerPort {
            container_port: input.k8s.port().into(),
            ..Default::default()
        }]),
        volume_mounts: Some(volume_mounts(input)),
        readiness_probe: Some(http_probe(input.k8s.port())),
        liveness_probe: Some(http_probe(input.k8s.port())),
        ..Default::default()
    };

    Deployment {
        metadata: ObjectMeta {
            name: Some(input.cell.to_string()),
            namespace: Some(input.k8s.namespace().to_string()),
            labels: Some(app_label(input.cell)),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(input.k8s.replicas() as i32),
            selector: LabelSelector {
                match_labels: Some(app_label(input.cell)),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(app_label(input.cell)),
                    annotations: Some(annotations),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    containers: vec![container],
                    volumes: Some(volumes(input)),
                    image_pull_secrets: image_pull_secrets(input),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

// --- CronJob --------------------------------------------------------------

/// The Builder (`datamk run`, ADR 0002 §1). `None` when the overlay omits
/// `schedule` — serve-only, no CronJob. No checksum annotations: the CronJob
/// gets a fresh pod (and a fresh ConfigMap/Secret mount) every run regardless.
fn render_cronjob(input: &RenderInput) -> Option<CronJob> {
    let schedule = input.k8s.schedule.clone()?;

    let container = Container {
        name: "builder".to_string(),
        image: Some(input.k8s.image_ref()),
        command: Some(vec!["datamk".to_string()]),
        args: Some(builder_args(input)),
        volume_mounts: Some(volume_mounts(input)),
        ..Default::default()
    };

    Some(CronJob {
        metadata: ObjectMeta {
            name: Some(input.cell.to_string()),
            namespace: Some(input.k8s.namespace().to_string()),
            labels: Some(app_label(input.cell)),
            ..Default::default()
        },
        spec: Some(CronJobSpec {
            schedule,
            concurrency_policy: Some("Forbid".to_string()),
            // Without a deadline, 100 missed starts (long builds under Forbid)
            // make the controller stop scheduling permanently (ADR 0004 §5).
            starting_deadline_seconds: Some(300),
            job_template: JobTemplateSpec {
                metadata: None,
                spec: Some(JobSpec {
                    template: PodTemplateSpec {
                        metadata: Some(ObjectMeta {
                            labels: Some(app_label(input.cell)),
                            ..Default::default()
                        }),
                        spec: Some(PodSpec {
                            containers: vec![container],
                            volumes: Some(volumes(input)),
                            restart_policy: Some("OnFailure".to_string()),
                            image_pull_secrets: image_pull_secrets(input),
                            ..Default::default()
                        }),
                    },
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CellDef;
    use std::path::Path;

    /// Build a real `CellArtifact` from the `orders` fixture, the same way
    /// `artifact.rs`'s own tests do — pure I/O, no DuckDB.
    fn orders_artifact() -> CellArtifact {
        let dir = Path::new("test/integrations/orders");
        let def = CellDef::load(&dir.join("cell.yaml")).unwrap();
        CellArtifact::collect(dir, "cell.yaml", &def).unwrap()
    }

    fn input<'a>(k8s: &'a KubernetesConfig, artifact: &'a CellArtifact) -> RenderInput<'a> {
        RenderInput {
            cell: "orders",
            profile: "prod",
            k8s,
            artifact,
            has_roles: false,
            secret_checksum: None,
        }
    }

    #[test]
    fn configmap_name_uses_the_first_12_hash_chars_and_is_immutable() {
        let art = orders_artifact();
        let k8s = KubernetesConfig::default();
        let cm = render_configmap(&input(&k8s, &art)).unwrap();
        assert_eq!(
            cm.metadata.name.unwrap(),
            format!("orders-{}", &art.content_hash[..12])
        );
        assert_eq!(cm.immutable, Some(true));
        // The label value is the same truncated 12-char prefix as the name --
        // NOT the full 64-char hash, which exceeds Kubernetes' 63-byte label
        // value limit and a real API server rejects outright.
        assert_eq!(
            cm.metadata.labels.unwrap().get("datamk.io/content-hash"),
            Some(&art.content_hash[..12].to_string())
        );
    }

    #[test]
    fn every_label_value_fits_the_kubernetes_63_byte_limit() {
        // Regression pin for the 64-char-hash-as-a-label-value bug (found by
        // actually applying to a real cluster, not by any unit test that only
        // inspects the typed struct): whatever `render_configmap` puts in
        // `labels`, every value must be <= 63 bytes or a real API server
        // rejects the object with `metadata.labels: ... must be no more than
        // 63 bytes`.
        let art = orders_artifact();
        let k8s = KubernetesConfig::default();
        let cm = render_configmap(&input(&k8s, &art)).unwrap();
        for (k, v) in cm.metadata.labels.unwrap() {
            assert!(
                v.len() <= 63,
                "label '{k}' value '{v}' is {} bytes",
                v.len()
            );
        }
    }

    #[test]
    fn configmap_carries_the_published_pin_only_when_present() {
        let art = orders_artifact();
        let k8s = KubernetesConfig::default();
        let cm = render_configmap(&input(&k8s, &art)).unwrap();
        let data = cm.data.unwrap();
        // Keys are sanitized (`/` -> `_`) so they're valid ConfigMap keys.
        assert!(data.contains_key("cell.yaml"));
        assert!(data.contains_key("sql_stg_orders.sql"));
        assert!(data.contains_key("sql_orders_daily.sql"));
        // orders/.cell/published.json exists in the committed fixture (release
        // pin already run once for the `release` integration tests).
        if art.published.is_some() {
            assert!(data.contains_key(".cell_published.json"));
        } else {
            assert!(!data.contains_key(".cell_published.json"));
        }
    }

    #[test]
    fn configmap_keys_are_valid_and_the_volume_restores_slash_paths() {
        let art = orders_artifact();
        let k8s = KubernetesConfig::default();
        // Every ConfigMap data key must be valid (no `/`), or a real server-side
        // apply rejects the object.
        let cm = render_configmap(&input(&k8s, &art)).unwrap();
        for key in cm.data.unwrap().keys() {
            assert!(!key.contains('/'), "invalid ConfigMap key: {key}");
        }
        // The volume's `items` map each sanitized key back to the real relative
        // path (with its `sql/` subdir), so the mount reproduces the cell layout.
        let vol = cell_volume(&input(&k8s, &art));
        let items = vol.config_map.unwrap().items.unwrap();
        let by_path: BTreeMap<_, _> = items
            .iter()
            .map(|i| (i.path.as_str(), i.key.as_str()))
            .collect();
        assert_eq!(by_path.get("cell.yaml"), Some(&"cell.yaml"));
        assert_eq!(
            by_path.get("sql/stg_orders.sql"),
            Some(&"sql_stg_orders.sql")
        );
    }

    #[test]
    fn service_never_provisions_a_load_balancer() {
        let art = orders_artifact();
        let k8s = KubernetesConfig::default();
        let svc = render_service(&input(&k8s, &art));
        // ADR §7: ClusterIP only. `type_: None` lets Kubernetes default to
        // ClusterIP; it must never be set to LoadBalancer/NodePort here.
        assert_eq!(svc.spec.unwrap().type_, None);
    }

    #[test]
    fn deployment_checksum_annotation_tracks_content_hash() {
        let art = orders_artifact();
        let k8s = KubernetesConfig::default();
        let dep = render_deployment(&input(&k8s, &art));
        let annotations = dep
            .spec
            .unwrap()
            .template
            .metadata
            .unwrap()
            .annotations
            .unwrap();
        assert_eq!(annotations.get("checksum/config"), Some(&art.content_hash));
        assert!(!annotations.contains_key("checksum/secret"));
    }

    #[test]
    fn secret_checksum_annotation_present_when_supplied() {
        let art = orders_artifact();
        let k8s = KubernetesConfig::default();
        let mut i = input(&k8s, &art);
        i.secret_checksum = Some("abc123");
        let dep = render_deployment(&i);
        let annotations = dep
            .spec
            .unwrap()
            .template
            .metadata
            .unwrap()
            .annotations
            .unwrap();
        assert_eq!(
            annotations.get("checksum/secret").map(String::as_str),
            Some("abc123")
        );
    }

    #[test]
    fn principals_volume_absent_without_roles() {
        let art = orders_artifact();
        let k8s = KubernetesConfig::default();
        let dep = render_deployment(&input(&k8s, &art));
        let spec = dep.spec.unwrap().template.spec.unwrap();
        let volumes = spec.volumes.unwrap();
        assert!(!volumes.iter().any(|v| v.name == "principals"));
        let mounts = spec.containers[0].volume_mounts.clone().unwrap();
        assert!(!mounts.iter().any(|m| m.name == "principals"));
    }

    #[test]
    fn principals_volume_present_with_roles() {
        let art = orders_artifact();
        let k8s = KubernetesConfig::default();
        let mut i = input(&k8s, &art);
        i.has_roles = true;
        let dep = render_deployment(&i);
        let spec = dep.spec.unwrap().template.spec.unwrap();
        let volumes = spec.volumes.unwrap();
        let principals = volumes
            .iter()
            .find(|v| v.name == "principals")
            .expect("principals volume");
        assert_eq!(
            principals.secret.as_ref().unwrap().secret_name,
            Some("orders-principals".to_string())
        );
        assert_eq!(
            principals.secret.as_ref().unwrap().default_mode,
            Some(0o400)
        );
        let mounts = spec.containers[0].volume_mounts.clone().unwrap();
        let mount = mounts
            .iter()
            .find(|m| m.name == "principals")
            .expect("principals mount");
        assert_eq!(mount.mount_path, PRINCIPALS_MOUNT_DIR);
        assert_eq!(mount.read_only, Some(true));
    }

    #[test]
    fn cronjob_is_none_without_a_schedule() {
        let art = orders_artifact();
        let k8s = KubernetesConfig::default();
        assert!(render_cronjob(&input(&k8s, &art)).is_none());
    }

    #[test]
    fn cronjob_carries_the_configured_schedule_when_set() {
        let art = orders_artifact();
        let k8s: KubernetesConfig = serde_yaml::from_str("schedule: \"0 * * * *\"").unwrap();
        let cj = render_cronjob(&input(&k8s, &art)).unwrap();
        let spec = cj.spec.unwrap();
        assert_eq!(spec.schedule, "0 * * * *");
        assert_eq!(spec.concurrency_policy.as_deref(), Some("Forbid"));
        let pod_spec = spec.job_template.spec.unwrap().template.spec.unwrap();
        assert_eq!(pod_spec.restart_policy.as_deref(), Some("OnFailure"));
        assert_eq!(pod_spec.containers[0].name, "builder");
    }

    #[test]
    fn image_pull_secret_reaches_the_pod_spec_when_set() {
        let art = orders_artifact();
        let k8s: KubernetesConfig = serde_yaml::from_str("imagePullSecret: regcred").unwrap();
        let dep = render_deployment(&input(&k8s, &art));
        let secrets = dep
            .spec
            .unwrap()
            .template
            .spec
            .unwrap()
            .image_pull_secrets
            .unwrap();
        assert_eq!(secrets[0].name, "regcred");
    }

    #[test]
    fn image_pull_secret_absent_by_default() {
        let art = orders_artifact();
        let k8s = KubernetesConfig::default();
        let dep = render_deployment(&input(&k8s, &art));
        assert!(dep
            .spec
            .unwrap()
            .template
            .spec
            .unwrap()
            .image_pull_secrets
            .is_none());
    }

    #[test]
    fn serve_args_use_long_flags_and_include_port() {
        let art = orders_artifact();
        let k8s = KubernetesConfig::default();
        let dep = render_deployment(&input(&k8s, &art));
        let args = dep.spec.unwrap().template.spec.unwrap().containers[0]
            .args
            .clone()
            .unwrap();
        assert_eq!(
            args,
            vec![
                "serve".to_string(),
                "--file".to_string(),
                "/cell/cell.yaml".to_string(),
                "--profile".to_string(),
                "prod".to_string(),
                "--port".to_string(),
                "8080".to_string(),
                "--poll-interval".to_string(),
                "15".to_string(),
            ]
        );
    }

    #[test]
    fn builder_args_are_run_file_profile() {
        let art = orders_artifact();
        let k8s: KubernetesConfig = serde_yaml::from_str("schedule: \"0 * * * *\"").unwrap();
        let cj = render_cronjob(&input(&k8s, &art)).unwrap();
        let args = cj
            .spec
            .unwrap()
            .job_template
            .spec
            .unwrap()
            .template
            .spec
            .unwrap()
            .containers[0]
            .args
            .clone()
            .unwrap();
        assert_eq!(
            args,
            vec![
                "run".to_string(),
                "--file".to_string(),
                "/cell/cell.yaml".to_string(),
                "--profile".to_string(),
                "prod".to_string(),
                "--retention-days".to_string(),
                "30".to_string(),
            ]
        );
    }

    #[test]
    fn init_job_name_is_content_addressed() {
        let art = orders_artifact();
        let k8s = KubernetesConfig::default();
        let job = render_init_job(&input(&k8s, &art));
        assert_eq!(
            job.metadata.name.unwrap(),
            format!("orders-init-{}", &art.content_hash[..12])
        );
    }

    #[test]
    fn init_job_args_are_run_file_profile() {
        let art = orders_artifact();
        let k8s = KubernetesConfig::default();
        let job = render_init_job(&input(&k8s, &art));
        let spec = job.spec.unwrap().template.spec.unwrap();
        assert_eq!(
            spec.containers[0].args.clone().unwrap(),
            vec![
                "run".to_string(),
                "--file".to_string(),
                "/cell/cell.yaml".to_string(),
                "--profile".to_string(),
                "prod".to_string(),
                "--retention-days".to_string(),
                "30".to_string(),
            ]
        );
    }

    #[test]
    fn init_job_restart_policy_and_cleanup_are_set() {
        let art = orders_artifact();
        let k8s = KubernetesConfig::default();
        let job = render_init_job(&input(&k8s, &art));
        let spec = job.spec.clone().unwrap();
        assert_eq!(spec.backoff_limit, Some(2));
        assert_eq!(spec.ttl_seconds_after_finished, Some(3600));
        assert_eq!(
            spec.template.spec.unwrap().restart_policy.as_deref(),
            Some("Never")
        );
    }

    #[test]
    fn init_job_mounts_match_the_builder() {
        let art = orders_artifact();
        // A scheduled config so `render_cronjob` returns `Some` to compare
        // against; `render_init_job` itself never looks at `schedule`.
        let scheduled: KubernetesConfig = serde_yaml::from_str("schedule: \"0 * * * *\"").unwrap();
        let mut i = input(&scheduled, &art);
        i.has_roles = true;

        let job = render_init_job(&i);
        let cronjob = render_cronjob(&i).unwrap();

        let job_spec = job.spec.unwrap().template.spec.unwrap();
        let cron_spec = cronjob
            .spec
            .unwrap()
            .job_template
            .spec
            .unwrap()
            .template
            .spec
            .unwrap();

        let job_mounts: Vec<_> = job_spec.containers[0]
            .volume_mounts
            .clone()
            .unwrap()
            .into_iter()
            .map(|m| (m.name, m.mount_path))
            .collect();
        let cron_mounts: Vec<_> = cron_spec.containers[0]
            .volume_mounts
            .clone()
            .unwrap()
            .into_iter()
            .map(|m| (m.name, m.mount_path))
            .collect();
        assert_eq!(job_mounts, cron_mounts);

        let job_volume_names: Vec<_> = job_spec
            .volumes
            .unwrap()
            .into_iter()
            .map(|v| v.name)
            .collect();
        let cron_volume_names: Vec<_> = cron_spec
            .volumes
            .unwrap()
            .into_iter()
            .map(|v| v.name)
            .collect();
        assert_eq!(job_volume_names, cron_volume_names);
    }

    #[test]
    fn init_job_label_value_fits_the_kubernetes_63_byte_limit() {
        let art = orders_artifact();
        let k8s = KubernetesConfig::default();
        let job = render_init_job(&input(&k8s, &art));
        for (k, v) in job.metadata.labels.unwrap() {
            assert!(
                v.len() <= 63,
                "label '{k}' value '{v}' is {} bytes",
                v.len()
            );
        }
    }

    #[test]
    fn render_all_includes_cronjob_only_when_scheduled() {
        let art = orders_artifact();
        let no_schedule = KubernetesConfig::default();
        let docs = render_all(&input(&no_schedule, &art)).unwrap();
        let kinds: Vec<_> = docs.iter().map(|d| d.kind.as_str()).collect();
        assert_eq!(kinds, vec!["ConfigMap", "Job", "Service", "Deployment"]);

        let scheduled: KubernetesConfig = serde_yaml::from_str("schedule: \"0 * * * *\"").unwrap();
        let docs = render_all(&input(&scheduled, &art)).unwrap();
        let kinds: Vec<_> = docs.iter().map(|d| d.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec!["ConfigMap", "Job", "Service", "Deployment", "CronJob"]
        );
    }

    #[test]
    fn rendered_yaml_carries_kind_and_never_the_profile_dsn() {
        let art = orders_artifact();
        let k8s = KubernetesConfig::default();
        let docs = render_all(&input(&k8s, &art)).unwrap();
        for d in &docs {
            assert!(d.body.contains(&format!("kind: {}", d.kind)), "{}", d.body);
        }
        let all = docs.iter().map(|d| d.body.as_str()).collect::<String>();
        assert!(!all.contains("postgres://"));
    }
}
