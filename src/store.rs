//! Native object-store client + the published-catalog layout (ADR 0004 §2/§3).
//!
//! DuckDB's httpfs is the *query* engine's S3 path; this module is the
//! *publisher's*: plain GET/PUT/LIST plus the conditional PUT (create-if-absent)
//! the single-writer guard rests on. Credentials come from the same `s3:`
//! profile block that configures DuckDB's secret — one block drives both
//! clients, or the profile contract is a lie.
//!
//! Layout under a cell's storage prefix:
//! ```text
//! <storage>/
//!   data/…                          # Parquet (DuckLake DATA_PATH)
//!   catalog/
//!     executions/00000047.ducklake  # immutable catalog artifact for execution 47
//!     LATEST                        # pointer: zero-padded number, UTF-8, no newline
//! ```

use anyhow::{bail, Context, Result};
use futures::TryStreamExt;
use object_store::path::Path as StorePath;
use object_store::{ObjectStore, PutMode, PutOptions, PutPayload};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::ResolvedS3;

pub const LATEST_KEY: &str = "catalog/LATEST";
pub const EXECUTIONS_PREFIX: &str = "catalog/executions";

/// What a conditional-PUT capability probe determined (ADR 0004 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    Enforced,
    NotEnforced,
}

/// The hard-failure message for a store that answered the probe and failed it.
pub const NOT_ENFORCED_MSG: &str =
    "object store does not enforce conditional PUT (create-if-absent): a second create of the \
     same key succeeded. The published-catalog single-writer guard (ADR 0004) cannot be \
     enforced against this store.";

/// The key of execution N's artifact. Zero-padded so lexicographic listing is
/// numeric ordering.
pub fn execution_key(n: u64) -> String {
    format!("{EXECUTIONS_PREFIX}/{n:08}.ducklake")
}

/// The normative `LATEST` content for execution N (ADR 0004 §2): the padded
/// number as UTF-8, no newline. Operators may write this by hand (§9's escape
/// hatch), so the format is a contract.
pub fn latest_content(n: u64) -> String {
    format!("{n:08}")
}

/// The AWS credential chain bridged into object_store. Resolved fresh per
/// request on purpose: `Store::block_on` builds and drops a runtime per call,
/// so a cached SSO/IMDS provider would hold HTTP state from a dead runtime.
/// Store calls are a handful per command; re-reading the chain is noise.
#[derive(Debug)]
struct AwsChainCredentials {
    /// The profile's `s3.region`, passed to the loader ONLY so it skips its
    /// own region resolution — which otherwise probes EC2 metadata
    /// (169.254.169.254) on laptops: seconds of connect timeouts and a wall
    /// of WARNs per client. S3 request signing never reads this; the
    /// builder's `with_region` owns that.
    region: Option<String>,
}

#[async_trait::async_trait]
impl object_store::CredentialProvider for AwsChainCredentials {
    type Credential = object_store::aws::AwsCredential;

    async fn get_credential(&self) -> object_store::Result<Arc<Self::Credential>> {
        use aws_credential_types::provider::ProvideCredentials;
        let generic =
            |source: Box<dyn std::error::Error + Send + Sync>| object_store::Error::Generic {
                store: "S3",
                source,
            };
        let region = aws_config::Region::new(
            // Any static value suppresses the IMDS region probe; the chain
            // uses it only for its own STS/SSO calls, which accept any
            // region. us-east-1 is the SDK's own last-resort convention.
            self.region.clone().unwrap_or_else(|| "us-east-1".into()),
        );
        let cfg = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(region)
            .load()
            .await;
        let creds = cfg
            .credentials_provider()
            .ok_or_else(|| generic("no AWS credentials provider in the credential chain".into()))?
            .provide_credentials()
            .await
            .map_err(|e| generic(Box::new(e)))?;
        Ok(Arc::new(object_store::aws::AwsCredential {
            key_id: creds.access_key_id().to_string(),
            secret_key: creds.secret_access_key().to_string(),
            token: creds.session_token().map(str::to_string),
        }))
    }
}

/// A cell's slice of an object store: all keys are relative to the cell's
/// storage prefix. Sync surface over an async client — every call drives its
/// future on a fresh thread so callers are safe from any tokio context
/// (`engine::run` executes on the runtime's main thread; a nested `block_on`
/// there would panic).
pub struct Store {
    inner: Arc<dyn ObjectStore>,
    prefix: StorePath,
}

impl Store {
    /// Build a client for a cell's storage URI (`s3://bucket/pre/fix`,
    /// `gs://bucket/pre/fix`), configured from the same `s3:` profile block as
    /// DuckDB's secret (endpoint, url_style, use_ssl, region, keys/ambient).
    pub fn for_storage(storage: &str, s3: Option<&ResolvedS3>) -> Result<Store> {
        let (scheme, rest) = storage
            .split_once("://")
            .with_context(|| format!("storage '{storage}' is not a remote URI"))?;
        let (bucket, prefix) = match rest.split_once('/') {
            Some((b, p)) => (b, p.trim_end_matches('/')),
            None => (rest, ""),
        };
        if bucket.is_empty() {
            bail!("storage '{storage}' has no bucket");
        }

        let inner: Arc<dyn ObjectStore> = match scheme {
            "s3" => {
                let mut b = object_store::aws::AmazonS3Builder::from_env()
                    .with_bucket_name(bucket)
                    // Conditional PUT is load-bearing (ADR 0004 §5); S3's
                    // native If-None-Match is what PutMode::Create uses.
                    .with_conditional_put(object_store::aws::S3ConditionalPut::ETagMatch);
                if let Some(s3) = s3 {
                    if let Some(r) = &s3.region {
                        b = b.with_region(r.clone());
                    }
                    if let Some(e) = &s3.endpoint {
                        // Profile endpoints are bare host[:port] (DuckDB's
                        // ENDPOINT convention); object_store wants a URL.
                        let scheme = if s3.use_ssl == Some(false) {
                            "http"
                        } else {
                            "https"
                        };
                        let url = if e.contains("://") {
                            e.clone()
                        } else {
                            format!("{scheme}://{e}")
                        };
                        b = b.with_endpoint(url);
                    }
                    if s3.use_ssl == Some(false) {
                        b = b.with_allow_http(true);
                    }
                    match s3.url_style.as_deref() {
                        Some("path") => b = b.with_virtual_hosted_style_request(false),
                        Some("vhost") => b = b.with_virtual_hosted_style_request(true),
                        _ => {}
                    }
                    if let (Some(k), Some(s)) = (&s3.key_id, &s3.secret) {
                        b = b
                            .with_access_key_id(k.clone())
                            .with_secret_access_key(s.clone());
                        if let Some(t) = &s3.session_token {
                            b = b.with_token(t.clone());
                        }
                    }
                }
                // No explicit keys in the profile -> the standard AWS
                // credential chain, exactly like DuckDB's `credential_chain`
                // secret provider (engine::create_s3_secret). object_store's
                // own fallback is env vars + IMDS only, which silently skips
                // shared-config profiles (AWS_PROFILE, SSO).
                if s3.is_none_or(|s| s.key_id.is_none() || s.secret.is_none()) {
                    b = b.with_credentials(Arc::new(AwsChainCredentials {
                        region: s3.and_then(|s| s.region.clone()),
                    }));
                }
                Arc::new(b.build().context("building S3 client for storage")?)
            }
            "gs" | "gcs" => Arc::new(
                object_store::gcp::GoogleCloudStorageBuilder::from_env()
                    .with_bucket_name(bucket)
                    .build()
                    .context("building GCS client for storage")?,
            ),
            other => bail!("storage scheme '{other}://' is not a supported object store"),
        };

        Ok(Store::new(inner, prefix))
    }

    fn new(inner: Arc<dyn ObjectStore>, prefix: &str) -> Store {
        Store {
            inner,
            prefix: StorePath::from(prefix),
        }
    }

    /// In-memory store for tests of the published-catalog protocol.
    #[cfg(test)]
    pub fn in_memory() -> Store {
        Store::new(Arc::new(object_store::memory::InMemory::new()), "cells/t")
    }

    fn key(&self, rel: &str) -> StorePath {
        if self.prefix.as_ref().is_empty() {
            StorePath::from(rel)
        } else {
            StorePath::from(format!("{}/{rel}", self.prefix))
        }
    }

    /// Drive a future to completion from any calling context. A dedicated
    /// thread per call, with a runtime built *and dropped* on that thread —
    /// network-bound operations dwarf the cost, and neither the execution nor
    /// the runtime's drop can ever collide with an ambient tokio context
    /// (`engine::run` executes on the runtime's main thread; `deploy` is
    /// async; a runtime owned by `Store` would be dropped wherever the Store
    /// is, which panics inside async contexts).
    fn block_on<T: Send>(&self, fut: impl std::future::Future<Output = T> + Send) -> T {
        std::thread::scope(|s| {
            s.spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("building store runtime")
                    .block_on(fut)
            })
            .join()
            .expect("store worker thread panicked")
        })
    }

    /// GET a small object. `Ok(None)` on not-found.
    pub fn get(&self, rel: &str) -> Result<Option<Vec<u8>>> {
        let key = self.key(rel);
        self.block_on(async {
            match self.inner.get(&key).await {
                Ok(r) => Ok(Some(r.bytes().await?.to_vec())),
                Err(object_store::Error::NotFound { .. }) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
    }

    /// Unconditional PUT (the `LATEST` pointer; §4 writes it last).
    pub fn put(&self, rel: &str, bytes: Vec<u8>) -> Result<()> {
        let key = self.key(rel);
        self.block_on(async {
            self.inner
                .put(&key, PutPayload::from(bytes))
                .await
                .map(|_| ())
                .map_err(anyhow::Error::from)
        })
        .with_context(|| format!("writing {rel}"))
    }

    /// Conditional PUT: create-if-absent. `Ok(false)` when the key already
    /// exists — the enforced single-writer guard (ADR 0004 §5).
    pub fn put_if_absent(&self, rel: &str, bytes: Vec<u8>) -> Result<bool> {
        let key = self.key(rel);
        let opts = PutOptions {
            mode: PutMode::Create,
            ..Default::default()
        };
        self.block_on(async {
            match self
                .inner
                .put_opts(&key, PutPayload::from(bytes), opts)
                .await
            {
                Ok(_) => Ok(true),
                Err(object_store::Error::AlreadyExists { .. }) => Ok(false),
                Err(e) => Err(anyhow::Error::from(e)),
            }
        })
        .with_context(|| format!("conditional-put {rel}"))
    }

    /// Conditional PUT of a local file (the artifact upload).
    pub fn put_file_if_absent(&self, rel: &str, src: &Path) -> Result<bool> {
        let bytes = std::fs::read(src)
            .with_context(|| format!("reading local artifact {}", src.display()))?;
        self.put_if_absent(rel, bytes)
    }

    /// Download an object to a local file. `Ok(false)` on not-found.
    pub fn get_to_file(&self, rel: &str, dest: &Path) -> Result<bool> {
        match self.get(rel)? {
            Some(bytes) => {
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(dest, bytes)
                    .with_context(|| format!("writing {}", dest.display()))?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Object names (final path component) under a relative prefix.
    pub fn list_names(&self, rel_prefix: &str) -> Result<Vec<String>> {
        Ok(self
            .list_meta(rel_prefix)?
            .into_iter()
            .map(|(name, _)| name)
            .collect())
    }

    /// Object names + last-modified (unix seconds) under a relative prefix.
    fn list_meta(&self, rel_prefix: &str) -> Result<Vec<(String, i64)>> {
        let key = self.key(rel_prefix);
        self.block_on(async {
            let metas: Vec<_> = self.inner.list(Some(&key)).try_collect().await?;
            Ok(metas
                .into_iter()
                .filter_map(|m| {
                    let name = m.location.filename()?.to_string();
                    Some((name, m.last_modified.timestamp()))
                })
                .collect())
        })
    }

    /// Last-modified of an object as RFC 3339, if it exists.
    pub fn last_modified(&self, rel: &str) -> Result<Option<String>> {
        let key = self.key(rel);
        self.block_on(async {
            match self.inner.head(&key).await {
                Ok(m) => Ok(Some(m.last_modified.to_rfc3339())),
                Err(object_store::Error::NotFound { .. }) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
    }

    pub fn delete(&self, rel: &str) -> Result<()> {
        let key = self.key(rel);
        self.block_on(async {
            match self.inner.delete(&key).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
                Err(e) => Err(e.into()),
            }
        })
    }

    /// The conditional-PUT capability probe (ADR 0004 §3): prove the store
    /// honors create-if-absent. Without it the single-writer guard silently
    /// degrades to last-writer-wins.
    ///
    /// `Ok(NotEnforced)` is a *determination* (the store answered and failed
    /// the test — callers hard-fail); `Err` means the probe couldn't run
    /// (unreachable store, missing credentials) — the deploy host may
    /// legitimately be unable to reach storage that pods can (in-cluster
    /// MinIO, private endpoints), so callers decide per-context whether that
    /// defers or fails.
    pub fn probe_conditional_put(&self) -> Result<ProbeOutcome> {
        let key = "catalog/.datamk-probe";
        self.delete(key)?;
        if !self.put_if_absent(key, b"probe".to_vec())? {
            bail!("conditional-put probe: key existed after delete (store is inconsistent)");
        }
        let second = self.put_if_absent(key, b"probe2".to_vec())?;
        self.delete(key)?;
        Ok(if second {
            ProbeOutcome::NotEnforced
        } else {
            ProbeOutcome::Enforced
        })
    }

    // ---- the published-catalog layout -------------------------------------

    /// The execution number `LATEST` points at, if any. Fails loud on a
    /// malformed pointer — a garbled hand-written value must not read as
    /// "no catalog yet".
    pub fn latest(&self) -> Result<Option<u64>> {
        match self.get(LATEST_KEY)? {
            None => Ok(None),
            Some(bytes) => {
                let s = String::from_utf8(bytes).context("LATEST pointer is not UTF-8")?;
                let n: u64 = s.trim().parse().with_context(|| {
                    format!("LATEST pointer content '{s}' is not an execution number")
                })?;
                Ok(Some(n))
            }
        }
    }

    /// Every published execution number, sorted ascending.
    pub fn list_executions(&self) -> Result<Vec<u64>> {
        let mut ns: Vec<u64> = self
            .list_names(EXECUTIONS_PREFIX)?
            .into_iter()
            .filter_map(|name| name.strip_suffix(".ducklake")?.parse().ok())
            .collect();
        ns.sort_unstable();
        Ok(ns)
    }

    /// Download execution N's artifact into `dest_dir`. Errors if absent.
    pub fn download_execution(&self, n: u64, dest_dir: &Path) -> Result<PathBuf> {
        let dest = dest_dir.join(format!("{n:08}.ducklake"));
        if !self.get_to_file(&execution_key(n), &dest)? {
            bail!("catalog artifact for execution {n} not found in the store");
        }
        Ok(dest)
    }

    /// Garbage-collect superseded execution artifacts (ADR 0004 §10): delete
    /// artifacts published before `older_than_unix`, never any execution in
    /// `keep` (what `LATEST` names must always survive, however old — a
    /// paused cell must not lose its serving artifact). Returns the deleted
    /// execution numbers. Dead branches from rollbacks age out here too.
    pub fn gc_artifacts(&self, older_than_unix: i64, keep: &[u64]) -> Result<Vec<u64>> {
        let mut deleted = Vec::new();
        for (name, modified) in self.list_meta(EXECUTIONS_PREFIX)? {
            let Some(n) = name
                .strip_suffix(".ducklake")
                .and_then(|s| s.parse::<u64>().ok())
            else {
                continue;
            };
            if keep.contains(&n) || modified >= older_than_unix {
                continue;
            }
            self.delete(&execution_key(n))?;
            deleted.push(n);
        }
        deleted.sort_unstable();
        Ok(deleted)
    }

    /// Publish a local catalog file as the next execution (ADR 0004 §4):
    /// number from `max(LIST)+1` (never `LATEST+1` — rollback moves the
    /// pointer backwards and must not wedge numbering), conditional PUT of the
    /// artifact (retry with a fresh listing on collision), then the pointer,
    /// written last. Returns the published execution number.
    pub fn publish_execution(&self, local: &Path) -> Result<u64> {
        for _ in 0..3 {
            let n = self.list_executions()?.last().copied().unwrap_or(0) + 1;
            if self.put_file_if_absent(&execution_key(n), local)? {
                self.put(LATEST_KEY, latest_content(n).into_bytes())
                    .context(
                        "artifact uploaded but LATEST pointer write failed; \
                              the next execution will number past it",
                    )?;
                return Ok(n);
            }
            tracing::warn!(
                execution = n,
                "artifact key already existed (concurrent writer?); re-listing"
            );
        }
        bail!(
            "could not allocate an execution number after 3 attempts — another writer is \
             actively publishing to this cell's prefix. One Builder per cell (ADR 0004 §5)."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_temp(name: &str, contents: &[u8]) -> PathBuf {
        let p = std::env::temp_dir().join(name);
        std::fs::write(&p, contents).unwrap();
        p
    }

    #[test]
    fn latest_is_none_on_a_fresh_store() {
        let s = Store::in_memory();
        assert_eq!(s.latest().unwrap(), None);
        assert!(s.list_executions().unwrap().is_empty());
    }

    #[test]
    fn publish_bootstraps_at_execution_one_and_writes_latest() {
        let s = Store::in_memory();
        let f = write_temp("datamk_store_pub1", b"catalog-bytes");
        assert_eq!(s.publish_execution(&f).unwrap(), 1);
        assert_eq!(s.latest().unwrap(), Some(1));
        assert_eq!(
            s.get(LATEST_KEY).unwrap().unwrap(),
            b"00000001".to_vec(),
            "LATEST content is the normative zero-padded format"
        );
        assert_eq!(s.list_executions().unwrap(), vec![1]);
    }

    #[test]
    fn publish_numbers_from_listing_not_from_latest() {
        // The rollback-wedge regression (ADR 0004 §4 step 4 / review blocker #1):
        // after LATEST is repointed backwards, the next publish must number past
        // every artifact that exists, not collide with the rolled-away one.
        let s = Store::in_memory();
        let f = write_temp("datamk_store_pub2", b"catalog-bytes");
        assert_eq!(s.publish_execution(&f).unwrap(), 1);
        assert_eq!(s.publish_execution(&f).unwrap(), 2);
        assert_eq!(s.publish_execution(&f).unwrap(), 3);

        // Roll back to execution 2.
        s.put(LATEST_KEY, latest_content(2).into_bytes()).unwrap();
        assert_eq!(s.latest().unwrap(), Some(2));

        // Next publish allocates 4 (max+1), not 3 — no wedge, and the dead
        // branch (3) remains immutable in the store.
        assert_eq!(s.publish_execution(&f).unwrap(), 4);
        assert_eq!(s.latest().unwrap(), Some(4));
        assert_eq!(s.list_executions().unwrap(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn gc_deletes_old_artifacts_but_never_the_kept_one() {
        let s = Store::in_memory();
        let f = write_temp("datamk_store_gc", b"catalog");
        for _ in 0..3 {
            s.publish_execution(&f).unwrap();
        }
        // Roll back to 2, publish again -> 4; 3 is a dead branch.
        s.put(LATEST_KEY, latest_content(2).into_bytes()).unwrap();
        assert_eq!(s.publish_execution(&f).unwrap(), 4);

        // Cutoff in the future = everything is "old"; keep protects 4 (LATEST)
        // regardless of age — dead branch 3 and superseded 1/2 go.
        let now_plus = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 60;
        let deleted = s.gc_artifacts(now_plus, &[4]).unwrap();
        assert_eq!(deleted, vec![1, 2, 3]);
        assert_eq!(s.list_executions().unwrap(), vec![4]);
        assert_eq!(s.latest().unwrap(), Some(4));

        // Within-window artifacts survive: publish 5, cutoff in the past.
        assert_eq!(s.publish_execution(&f).unwrap(), 5);
        let deleted = s.gc_artifacts(0, &[5]).unwrap();
        assert!(deleted.is_empty());
        assert_eq!(s.list_executions().unwrap(), vec![4, 5]);
    }

    #[test]
    fn conditional_put_refuses_an_existing_key() {
        let s = Store::in_memory();
        assert!(s.put_if_absent("k", b"a".to_vec()).unwrap());
        assert!(!s.put_if_absent("k", b"b".to_vec()).unwrap());
        // The loser did not clobber the winner.
        assert_eq!(s.get("k").unwrap().unwrap(), b"a".to_vec());
    }

    #[test]
    fn probe_reports_enforcement_on_a_conforming_store() {
        assert_eq!(
            Store::in_memory().probe_conditional_put().unwrap(),
            ProbeOutcome::Enforced
        );
    }

    #[test]
    fn malformed_latest_fails_loud_not_as_absent() {
        let s = Store::in_memory();
        s.put(LATEST_KEY, b"forty-seven".to_vec()).unwrap();
        let err = s.latest().unwrap_err().to_string();
        assert!(err.contains("not an execution number"), "got: {err}");
    }

    #[test]
    fn download_roundtrips_an_artifact() {
        let s = Store::in_memory();
        let f = write_temp("datamk_store_dl", b"the-catalog");
        let n = s.publish_execution(&f).unwrap();
        let dir = std::env::temp_dir().join("datamk_store_dl_out");
        let local = s.download_execution(n, &dir).unwrap();
        assert_eq!(std::fs::read(&local).unwrap(), b"the-catalog");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn download_of_a_missing_execution_errors() {
        let s = Store::in_memory();
        let err = s
            .download_execution(9, &std::env::temp_dir())
            .unwrap_err()
            .to_string();
        assert!(err.contains("execution 9"), "got: {err}");
    }

    #[test]
    fn for_storage_rejects_non_object_store_uris() {
        let err = match Store::for_storage("./local/path", None) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("local path must be rejected"),
        };
        assert!(err.contains("not a remote URI"), "got: {err}");
        let err = match Store::for_storage("ftp://bucket/x", None) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("ftp scheme must be rejected"),
        };
        assert!(err.contains("not a supported object store"), "got: {err}");
    }

    #[test]
    fn key_layout_is_the_adr_layout() {
        assert_eq!(execution_key(47), "catalog/executions/00000047.ducklake");
        assert_eq!(latest_content(47), "00000047");
        assert_eq!(LATEST_KEY, "catalog/LATEST");
    }
}
