//! Operational commands over the published-catalog layout (ADR 0004):
//! `datamk status` (the read side that makes operations legible) and
//! `datamk rollback` (repoint `LATEST`, guarded so a supported route's pin can
//! never be stranded). Both need only bucket credentials — no cluster access,
//! the same "datamk references infrastructure" posture as deploy.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::sync::Arc;

use crate::config;
use crate::engine;
use crate::store::{latest_content, Store, LATEST_KEY};

/// Load the profile and build the cell's store handle; both commands only make
/// sense for published-artifact profiles.
fn published_store(file: &Path, profile: &str) -> Result<(config::LoadedCell, Arc<Store>)> {
    let loaded = config::load(file, profile)?;
    if let Some(c) = &loaded.bindings.catalog {
        bail!(
            "profiles/{profile}.yaml sets `catalog:` ({c}) — direct-attach mode has no \
             published executions. `status`/`rollback` apply to published-artifact profiles \
             (no `catalog:`; ADR 0004)."
        );
    }
    let store = Store::for_storage(&loaded.bindings.storage, loaded.bindings.s3.as_ref())?;
    Ok((loaded, Arc::new(store)))
}

/// Print the published range, the `LATEST` pointer, and its age.
pub fn status(file: &Path, profile: &str) -> Result<()> {
    let (loaded, store) = published_store(file, profile)?;
    let executions = store.list_executions()?;
    let latest = store.latest()?;
    let modified = store.last_modified(LATEST_KEY)?;

    println!("cell: {}   profile: {}", loaded.def.cell, profile);
    println!("storage: {}", loaded.bindings.storage);
    match (executions.first(), executions.last()) {
        (Some(first), Some(last)) => println!(
            "executions: {first}..{last} published ({} artifact{})",
            executions.len(),
            if executions.len() == 1 { "" } else { "s" }
        ),
        _ => println!("executions: none published"),
    }
    match latest {
        Some(n) => match modified {
            Some(ts) => println!("LATEST -> {n}   (pointer written {ts})"),
            None => println!("LATEST -> {n}"),
        },
        None => println!("LATEST: absent (no execution published yet — run `datamk run`)"),
    }
    Ok(())
}

/// Repoint `LATEST` to an earlier (or explicit) execution. Refuses a target
/// that doesn't exist or that lacks any currently pinned snapshot — the guard
/// that keeps a supported route from 500-ing on `AT (VERSION => <pin>)`
/// against an artifact from before the pin (ADR 0004 §9).
pub fn rollback(file: &Path, profile: &str, execution: Option<u64>) -> Result<()> {
    let (loaded, store) = published_store(file, profile)?;
    let executions = store.list_executions()?;
    let current = store
        .latest()?
        .context("no LATEST pointer — nothing has been published, nothing to roll back")?;

    let target = match execution {
        Some(n) => n,
        // Default: the newest published artifact before the one LATEST names
        // (execution numbers can have gaps — dead branches from prior rollbacks).
        None => *executions
            .iter()
            .rfind(|&&n| n < current)
            .with_context(|| {
                format!("LATEST -> {current} and no earlier artifact exists to roll back to")
            })?,
    };

    if target == current {
        bail!("LATEST already points at execution {current}; nothing to do");
    }
    if !executions.contains(&target) {
        let range = match (executions.first(), executions.last()) {
            (Some(f), Some(l)) => format!("{f}..{l} ({} published)", executions.len()),
            _ => "none published".to_string(),
        };
        bail!("execution {target} does not exist; available: {range}");
    }

    // The pin guard: every snapshot the release manifest pins must exist in
    // the target artifact.
    let pins = load_pins(&loaded.dir);
    if !pins.is_empty() {
        let scratch = std::env::temp_dir().join(format!("datamk-rollback-{}", std::process::id()));
        let local = store.download_execution(target, &scratch)?;
        let result = check_pins_present(&local, &loaded.bindings.storage, &loaded, &pins, target);
        let _ = std::fs::remove_dir_all(&scratch);
        result?;
    }

    store.put(LATEST_KEY, latest_content(target).into_bytes())?;
    println!("LATEST {current} -> {target}");
    println!(
        "note: the next scheduled execution builds on execution {target}'s lineage and \
         publishes a fresh number; suspend the Builder CronJob if you want the world frozen here."
    );
    Ok(())
}

/// Pinned snapshot ids from the release manifest, if any.
fn load_pins(dir: &Path) -> Vec<(String, i64)> {
    let path = dir.join(".cell").join("published.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<crate::manifest::Published>(&raw).ok())
        .map(|p| p.routes.into_iter().collect())
        .unwrap_or_default()
}

fn check_pins_present(
    local: &Path,
    storage: &str,
    loaded: &config::LoadedCell,
    pins: &[(String, i64)],
    target: u64,
) -> Result<()> {
    let conn = engine::open_artifact(local, storage, loaded.bindings.s3.as_ref())?;
    let mut stmt = conn
        .prepare("SELECT snapshot_id FROM ducklake_snapshots('lake')")
        .context("querying target artifact's snapshots")?;
    let snapshots: Vec<i64> = stmt
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<std::result::Result<_, _>>()?;
    for (route, pin) in pins {
        if !snapshots.contains(pin) {
            bail!(
                "rollback to execution {target} refused: supported route '{route}' is pinned to \
                 snapshot {pin} (release manifest), which does not exist in that artifact — the \
                 route would 500 on every request.\n\
                 Roll back to an artifact that contains snapshot {pin}, or re-run `datamk \
                 release` against the rolled-back state first (a reviewed re-pin)."
            );
        }
    }
    Ok(())
}
