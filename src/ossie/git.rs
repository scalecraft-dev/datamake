//! `semantic_model.git` (ADR 0018 §3): shell out to the machine's own
//! `git` — the user's ssh agent / credential helper does auth, datamk holds
//! no secrets and never links `git2`. Every invocation is an explicit
//! `Vec<String>` argv, never a joined string; the URL is allowlisted before
//! `git` is ever spawned.

use anyhow::{anyhow, bail, Context as _, Result};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// What a fetch resolved to and where it landed. `checkout_dir` is already
/// the final directory a caller walks — `semantic_model.path`, when
/// present, has been joined and validated (ADR 0018 §3: "the subpath is
/// canonicalized and checked under the checkout root").
#[derive(Debug)]
pub struct Fetched {
    pub checkout_dir: PathBuf,
    pub commit: String,
}

pub struct FetchOptions {
    /// Tests only — `cell.yaml` cannot set this (ADR 0018 §3). Accepts a
    /// local absolute path or `file://` remote, and loosens git's own
    /// `protocol.file.allow` gate so a local bare repo (`git init --bare`
    /// in a tempdir) can be fetched without a network.
    pub allow_local: bool,
    pub timeout: Duration,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            allow_local: false,
            timeout: Duration::from_secs(120),
        }
    }
}

/// Fetch `url` at `git_ref` (default: the remote's HEAD) into the cache
/// dir keyed by `sha256(url)[..16]` under `<cell_dir>/.cell/semantic/`
/// (gitignored, the `.cell/attach/` precedent), then resolve `path` (when
/// present) inside the checkout.
pub fn fetch(
    cell_dir: &Path,
    url: &str,
    path: Option<&str>,
    git_ref: Option<&str>,
    opts: &FetchOptions,
) -> Result<Fetched> {
    validate_url(url, opts.allow_local)?;
    // C1/C2: `ref`/`path` are validated before `git` is ever spawned, not
    // merely before the argument that carries them — a later call site
    // re-checking would still have let an earlier one shell out first.
    if let Some(r) = git_ref {
        validate_ref(r)?;
    }
    if let Some(p) = path {
        validate_path(p)?;
    }

    let cache_root = cell_dir.join(".cell").join("semantic");
    std::fs::create_dir_all(&cache_root)
        .with_context(|| format!("creating {}", cache_root.display()))?;
    let key = &sha256_hex(url.as_bytes())[..16];
    let checkout_dir = cache_root.join(key);

    // H1: `clone`'s and `refresh`'s non-sha checkout target differ. `git
    // clone` (even `--no-checkout`) leaves `HEAD` pointed at the branch it
    // just cloned — correct to check out directly, and it writes no
    // `FETCH_HEAD` to check out instead. `git fetch` (what `refresh` runs
    // on a cached checkout) is the opposite: it writes `FETCH_HEAD` but
    // never touches the local `HEAD`, so checking out `HEAD` after a
    // refresh re-checks-out whatever was there *before* the fetch — a
    // moving branch would never advance. A sha ref is unaffected either
    // way: it's checked out by content, which `fetch_by_sha`'s real `git
    // fetch` always makes available.
    let target = if checkout_dir.join(".git").is_dir() {
        refresh(&checkout_dir, url, git_ref, opts)?;
        match git_ref.filter(|r| is_full_sha(r)) {
            Some(sha) => sha.to_string(),
            None => "FETCH_HEAD".to_string(),
        }
    } else {
        clone(&checkout_dir, url, git_ref, opts)?;
        match git_ref.filter(|r| is_full_sha(r)) {
            Some(sha) => sha.to_string(),
            None => "HEAD".to_string(),
        }
    };
    finalize_checkout(&checkout_dir, &target, git_ref, path, opts)?;
    let commit = rev_parse_head(&checkout_dir, opts)?;

    let resolved_dir = match path {
        Some(p) => resolve_subpath(&checkout_dir, p)?,
        None => checkout_dir,
    };
    Ok(Fetched {
        checkout_dir: resolved_dir,
        commit,
    })
}

fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// `semantic_model.ref` (ADR 0018 §3, C1): a branch, tag, or commit sha —
/// `^[A-Za-z0-9._/-]{1,255}$`, no leading `-` (argv injection: `git fetch
/// origin --upload-pack=...` executes a command), no `..` anywhere (a
/// path-traversal-shaped ref name). Checked before `git` is ever spawned,
/// not merely before the argument that carries it.
fn validate_ref(r: &str) -> Result<()> {
    if r.starts_with('-') {
        bail!(
            "`semantic_model.ref: {r}` must not start with '-' — refused (it would be read as \
             a git option, not a ref)."
        );
    }
    let shape_ok = !r.is_empty()
        && r.len() <= 255
        && r.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'));
    if !shape_ok || r.contains("..") {
        bail!(
            "`semantic_model.ref: {r}` is not a valid git ref — refused (expected \
             [A-Za-z0-9._/-]{{1,255}}, no leading '-', no '..')."
        );
    }
    Ok(())
}

/// `semantic_model.path` (ADR 0018 §3, C2): `^[A-Za-z0-9._/-]{1,512}$`, no
/// leading `-` (`sparse-checkout set --no-cone` flips cone mode instead of
/// naming a path) or `/` (must be relative inside the repo), no `..`.
fn validate_path(p: &str) -> Result<()> {
    if p.starts_with('-') {
        bail!(
            "`semantic_model.path: {p}` must not start with '-' — refused (it would be read as \
             a git option, e.g. `--no-cone`)."
        );
    }
    if p.starts_with('/') {
        bail!("`semantic_model.path: {p}` must be a relative path inside the repository.");
    }
    let shape_ok = !p.is_empty()
        && p.len() <= 512
        && p.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'));
    if !shape_ok || p.contains("..") {
        bail!(
            "`semantic_model.path: {p}` is not a valid path — refused (expected \
             [A-Za-z0-9._/-]{{1,512}}, no leading '-' or '/', no '..')."
        );
    }
    Ok(())
}

/// `url`, redacted to scheme+host+path for embedding in any error message
/// (ADR 0018 §3/M5): userinfo (`user:token@`), query, and fragment are
/// stripped — an operator's credential must never round-trip into a log
/// line or an `anyhow::Context`.
pub fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        // scp form (`user@host:path`) or an already-rejected shape — drop
        // anything before the last `@` (the "userinfo"-shaped part), keep
        // the rest verbatim.
        return match url.rfind('@') {
            Some(at) => url[at + 1..].to_string(),
            None => url.to_string(),
        };
    };
    let host_start = rest.rfind('@').map(|i| i + 1).unwrap_or(0);
    let no_userinfo = &rest[host_start..];
    let path_start = no_userinfo.find('/').unwrap_or(no_userinfo.len());
    let host = &no_userinfo[..path_start];
    let path = no_userinfo[path_start..]
        .split(['?', '#'])
        .next()
        .unwrap_or("");
    format!("{scheme}://{host}{path}")
}

/// Whether `url`'s authority (the part between `scheme://` and the first
/// `/`) carries userinfo (`user[:pass]@host`) — refused outright for
/// `https://` (M5): a credential in `cell.yaml` would round-trip into every
/// log line and the sync sidecar. Not applied to `ssh://`, where
/// `user@host` (almost always `git@host`) names the login user, not a
/// secret — the same syntax the scp form (`git@host:path`) already accepts.
fn has_url_userinfo(url: &str) -> bool {
    match url.split_once("://") {
        Some((_, rest)) => rest.split('/').next().unwrap_or(rest).contains('@'),
        None => false,
    }
}

/// `https://`, `ssh://`, or `git@host:path` (scp form) only. Refuses
/// `ext::`/any remote-helper prefix (`::` anywhere), `file://`, `git://`,
/// and a leading `-` (argv injection) — all before `git` is ever invoked.
/// `allow_local` (tests only) additionally accepts a local absolute path or
/// `file://`.
fn validate_url(url: &str, allow_local: bool) -> Result<()> {
    if url.starts_with('-') {
        bail!(
            "`semantic_model.git: {}` must not start with '-' — refused (it would be read as a \
             git option, not a URL).",
            redact_url(url)
        );
    }
    if url.contains("::") {
        bail!(
            "`semantic_model.git: {}` contains '::' (a git remote-helper prefix, e.g. `ext::`) \
             — refused; only a plain `https://`, `ssh://`, or `git@host:path` URL is accepted.",
            redact_url(url)
        );
    }
    if url.starts_with("https://") {
        if has_url_userinfo(url) {
            bail!(
                "`semantic_model.git: {}` carries a credential in the URL (userinfo) — refused; \
                 use a credential helper instead.",
                redact_url(url)
            );
        }
        return Ok(());
    }
    if url.starts_with("ssh://") {
        return Ok(());
    }
    if is_scp_form(url) {
        return Ok(());
    }
    if allow_local
        && (url.starts_with("file://") || (Path::new(url).is_absolute() && !url.contains("://")))
    {
        return Ok(());
    }
    bail!(
        "`semantic_model.git: {}` is not an allowed URL — only `https://`, `ssh://`, or \
         `git@host:path` (scp form) are accepted; `file://`, `git://`, and remote-helper \
         prefixes are refused (meaning must not vary by an untrusted transport).",
        redact_url(url)
    );
}

/// `user@host:path`, never containing `://` anywhere (which would make it
/// `ssh://` or similar instead) and with something between `@` and the
/// first `:` after it.
fn is_scp_form(url: &str) -> bool {
    if url.contains("://") {
        return false;
    }
    let Some(at) = url.find('@') else {
        return false;
    };
    let after_at = &url[at + 1..];
    match after_at.find(':') {
        Some(colon) => colon > 0,
        None => false,
    }
}

fn clone(checkout_dir: &Path, url: &str, git_ref: Option<&str>, opts: &FetchOptions) -> Result<()> {
    match git_ref.filter(|r| is_full_sha(r)) {
        Some(sha) => {
            run_git(
                None,
                &[
                    "init".into(),
                    "--".into(),
                    checkout_dir.display().to_string(),
                ],
                opts,
            )
            .map_err(|e| generic_error("git init", &e))?;
            run_git(
                Some(checkout_dir),
                &[
                    "remote".into(),
                    "add".into(),
                    "origin".into(),
                    "--".into(),
                    url.to_string(),
                ],
                opts,
            )
            .map_err(|e| generic_error("git remote add", &e))?;
            fetch_by_sha(checkout_dir, url, sha, opts)?;
        }
        None => {
            let mut args = vec![
                "clone".to_string(),
                "--depth".to_string(),
                "1".to_string(),
                "--single-branch".to_string(),
            ];
            if let Some(r) = git_ref {
                args.push("--branch".to_string());
                args.push(r.to_string());
            }
            args.push("--filter=blob:none".to_string());
            args.push("--no-checkout".to_string());
            args.push("--".to_string());
            args.push(url.to_string());
            args.push(checkout_dir.display().to_string());
            run_git(None, &args, opts).map_err(|e| classify_error(&e, url, git_ref))?;
        }
    }
    Ok(())
}

fn refresh(
    checkout_dir: &Path,
    url: &str,
    git_ref: Option<&str>,
    opts: &FetchOptions,
) -> Result<()> {
    match git_ref.filter(|r| is_full_sha(r)) {
        Some(sha) => fetch_by_sha(checkout_dir, url, sha, opts),
        None => {
            let target = git_ref.unwrap_or("HEAD");
            run_git(
                Some(checkout_dir),
                &[
                    "fetch".to_string(),
                    "--depth".to_string(),
                    "1".to_string(),
                    "--filter=blob:none".to_string(),
                    "origin".to_string(),
                    "--".to_string(),
                    target.to_string(),
                ],
                opts,
            )
            .map_err(|e| classify_error(&e, url, git_ref))?;
            Ok(())
        }
    }
}

/// `git fetch --depth 1 origin -- <sha>`; falls back to a full (unshallowed)
/// fetch of that ref if the server refuses shallow-by-sha (ADR 0018 §3).
fn fetch_by_sha(checkout_dir: &Path, url: &str, sha: &str, opts: &FetchOptions) -> Result<()> {
    let shallow = run_git(
        Some(checkout_dir),
        &[
            "fetch".to_string(),
            "--depth".to_string(),
            "1".to_string(),
            "--filter=blob:none".to_string(),
            "origin".to_string(),
            "--".to_string(),
            sha.to_string(),
        ],
        opts,
    );
    if shallow.is_ok() {
        return Ok(());
    }
    run_git(
        Some(checkout_dir),
        &[
            "fetch".to_string(),
            "origin".to_string(),
            "--".to_string(),
            sha.to_string(),
        ],
        opts,
    )
    .map(|_| ())
    .map_err(|e| classify_error(&e, url, Some(sha)))
}

/// H1: `target` is `fetch`'s pick between `HEAD` (fresh `clone`, which
/// leaves `HEAD` pointed at the branch it just cloned and writes no
/// `FETCH_HEAD`) and `FETCH_HEAD` (`refresh`'s `git fetch`, which is the
/// opposite: it writes `FETCH_HEAD` but never touches the local `HEAD`, so
/// checking out `HEAD` after a refresh would re-check-out whatever was
/// there *before* the fetch and a moving branch would never advance) — or a
/// sha, checked out by content either way. `git_ref` is threaded through
/// only for `classify_error`'s message, not the checkout command itself.
/// `--detach` so this never creates or advances a local branch.
fn finalize_checkout(
    checkout_dir: &Path,
    target: &str,
    git_ref: Option<&str>,
    path: Option<&str>,
    opts: &FetchOptions,
) -> Result<()> {
    if let Some(p) = path {
        run_git(
            Some(checkout_dir),
            &[
                "sparse-checkout".to_string(),
                "init".to_string(),
                "--cone".to_string(),
            ],
            opts,
        )
        .map_err(|e| generic_error("git sparse-checkout init", &e))?;
        run_git(
            Some(checkout_dir),
            &[
                "sparse-checkout".to_string(),
                "set".to_string(),
                "--".to_string(),
                p.to_string(),
            ],
            opts,
        )
        .map_err(|e| generic_error("git sparse-checkout set", &e))?;
    }
    // `target` is never attacker-controlled here — it's `fetch`'s own
    // literal `"HEAD"`/`"FETCH_HEAD"` or a sha already checked by
    // `is_full_sha` — so no `--` is needed (and `git checkout --detach --`
    // refuses a path argument; `--` forces pathspec interpretation, which a
    // tree-ish is not).
    run_git(
        Some(checkout_dir),
        &[
            "checkout".to_string(),
            "--detach".to_string(),
            target.to_string(),
        ],
        opts,
    )
    .map_err(|e| classify_error(&e, "", git_ref))?;
    Ok(())
}

fn rev_parse_head(checkout_dir: &Path, opts: &FetchOptions) -> Result<String> {
    let out = run_git(
        Some(checkout_dir),
        &["rev-parse".to_string(), "HEAD".to_string()],
        opts,
    )
    .map_err(|e| generic_error("git rev-parse HEAD", &e))?;
    Ok(out.trim().to_string())
}

/// `path` resolved and validated inside the checkout — canonicalized,
/// required to exist, required to still land under the checkout root.
fn resolve_subpath(checkout_dir: &Path, path: &str) -> Result<PathBuf> {
    let rel = Path::new(path);
    if rel.is_absolute() {
        bail!("`semantic_model.path: {path}` must be a relative path inside the repository.");
    }
    let root_canon = checkout_dir
        .canonicalize()
        .with_context(|| format!("resolving {}", checkout_dir.display()))?;
    let candidate = checkout_dir.join(rel);
    let resolved = candidate.canonicalize().map_err(|_| {
        anyhow!(
            "`semantic_model.path: {path}` does not exist in the checked-out repository — \
             check the subpath (default: repo root) and `ref`."
        )
    })?;
    if !resolved.starts_with(&root_canon) {
        bail!("`semantic_model.path: {path}` escapes the repository checkout — refused.");
    }
    Ok(resolved)
}

/// A failed `git` invocation: its stderr, for classification by the caller.
struct GitFailure {
    stderr: String,
}

/// The bound every reader thread gets to hand its buffer back once `git`'s
/// process (group) has been killed and its pipes have gone EOF — past this,
/// `run_git` gives up on the read and reports the timeout without it
/// (M2): a reader can still be blocked on a grandchild (`ssh`,
/// `git-remote-https`) that outlived a plain `child.kill()`.
const READER_JOIN_GRACE: Duration = Duration::from_secs(2);

/// Run `git` with the hardening config/env ADR 0018 §3 lists, an explicit
/// argv (never a joined string), and a hard wall-clock timeout.
fn run_git(cwd: Option<&Path>, args: &[String], opts: &FetchOptions) -> Result<String, GitFailure> {
    let mut cmd = Command::new("git");
    cmd.args([
        "-c",
        "protocol.allow=never",
        "-c",
        "protocol.https.allow=always",
        "-c",
        "protocol.ssh.allow=always",
        "-c",
        "core.hooksPath=/dev/null",
        // L1: never follow a submodule into another repository, regardless
        // of what the fetched tree declares.
        "-c",
        "submodule.recurse=false",
    ]);
    if opts.allow_local {
        cmd.args(["-c", "protocol.file.allow=always"]);
    }
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/bin/false")
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")
        .env("GIT_LFS_SKIP_SMUDGE", "1");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    // M2: run `git` in its own process group so a timeout can kill the
    // whole tree (`ssh`, `git-remote-https`) it may have spawned, not just
    // the direct child — `child.kill()` alone leaves those holding the
    // stdout/stderr pipes open, and the reader threads below block forever
    // joining on them.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }

    let mut child = cmd.spawn().map_err(|e| GitFailure {
        stderr: format!("failed to run git: {e}"),
    })?;
    let pid = child.id();
    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel();
    let (stderr_tx, stderr_rx) = std::sync::mpsc::channel();
    let _stdout_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        let _ = stdout_tx.send(buf);
    });
    let _stderr_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        let _ = stderr_tx.send(buf);
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if start.elapsed() > opts.timeout {
                    kill_process_tree(&mut child, pid);
                    let _ = child.wait();
                    // Bounded, not `.join()`: a reader can still be parked
                    // on a grandchild's pipe if the kill above didn't reach
                    // it (non-unix, or a race). Drop the handle rather than
                    // block `run_git`'s caller on it forever.
                    let _ = stdout_rx.recv_timeout(READER_JOIN_GRACE);
                    let _ = stderr_rx.recv_timeout(READER_JOIN_GRACE);
                    return Err(GitFailure {
                        stderr: format!(
                            "git {} timed out after {:?}",
                            args.join(" "),
                            opts.timeout
                        ),
                    });
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                return Err(GitFailure {
                    stderr: format!("waiting for git: {e}"),
                })
            }
        }
    };
    let stdout = stdout_rx
        .recv_timeout(READER_JOIN_GRACE)
        .unwrap_or_default();
    let stderr = stderr_rx
        .recv_timeout(READER_JOIN_GRACE)
        .unwrap_or_default();
    if !status.success() {
        return Err(GitFailure {
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

/// Kill `child` and, on unix, its whole process group (`run_git` put it in
/// its own group via `process_group(0)`) — `ssh`/`git-remote-https` survive
/// a plain `child.kill()` otherwise. Best-effort: a failure here still
/// leaves `child.wait()` (called by the caller right after) to reap
/// whatever did die.
#[cfg_attr(not(unix), allow(unused_variables))]
fn kill_process_tree(child: &mut std::process::Child, pid: u32) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-9", "--", &format!("-{pid}")])
            .status();
    }
    let _ = child.kill();
}

/// Classify a failed clone/fetch by its stderr (ADR 0018 §3): auth, ref not
/// found, or generic (git's stderr tail included).
fn classify_error(fail: &GitFailure, url: &str, git_ref: Option<&str>) -> anyhow::Error {
    // M5: `url` is echoed into every message below — scheme+host+path only,
    // never the raw string, in case it carries userinfo `validate_url`
    // didn't catch (e.g. a git config credential helper rewrite).
    let url = redact_url(url);
    let stderr = &fail.stderr;
    let is_auth = [
        "Authentication failed",
        "Permission denied",
        "could not read Username",
    ]
    .iter()
    .any(|s| stderr.contains(s));
    if is_auth {
        return anyhow!(
            "git could not authenticate to '{url}' — datamk shells out to your machine's git \
             and holds no credentials of its own; make sure your ssh agent or git credential \
             helper can already reach this remote (try `git ls-remote {url}`).\n{}",
            tail(stderr)
        );
    }
    let is_ref_not_found = (stderr.contains("Remote branch") && stderr.contains("not found"))
        || stderr.contains("couldn't find remote ref");
    if is_ref_not_found {
        let r = git_ref.unwrap_or("HEAD");
        return anyhow!(
            "git could not find ref '{r}' in '{url}' — check `semantic_model.ref` (a branch, \
             tag, or commit sha) against the repository.\n{}",
            tail(stderr)
        );
    }
    generic_error("git", fail)
}

fn generic_error(op: &str, fail: &GitFailure) -> anyhow::Error {
    anyhow!("{op} failed:\n{}", tail(&fail.stderr))
}

/// The last few lines of git's stderr — enough to diagnose, not a wall of
/// text.
fn tail(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().collect();
    let start = lines.len().saturating_sub(8);
    lines[start..].join("\n")
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        // A counter on top of pid+nanos: tests in this module run
        // concurrently (`cargo test` parallelizes within one process), and
        // two calls landing in the same timer tick collided in practice
        // (`git clone --bare` into an already-populated directory) once the
        // suite grew enough tests to make that likely.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "datamk-ossie-git-{tag}-{}-{}-{n}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t.invalid")
            .status()
            .expect("git available for tests");
        assert!(status.success(), "git {args:?} failed");
    }

    /// A local bare repo, seeded with one commit carrying `osi/model.yaml`,
    /// for `allow_local` tests to fetch against with no network.
    fn make_bare_repo() -> (PathBuf, String) {
        let work = tempdir("work");
        run(&work, &["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(work.join("osi")).unwrap();
        std::fs::write(
            work.join("osi/model.yaml"),
            "version: 0.1.1\nsemantic_model:\n  - name: m\n    datasets:\n      - name: d\n        source: s\n",
        )
        .unwrap();
        run(&work, &["add", "."]);
        run(&work, &["commit", "-q", "-m", "seed"]);
        let bare = tempdir("bare");
        std::fs::remove_dir_all(&bare).unwrap();
        run(
            &work,
            &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
        );
        let commit_out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&work)
            .output()
            .unwrap();
        let commit = String::from_utf8_lossy(&commit_out.stdout)
            .trim()
            .to_string();
        let _ = std::fs::remove_dir_all(&work);
        (bare, commit)
    }

    #[test]
    fn fetch_against_local_bare_repo_by_default_head() {
        let (bare, commit) = make_bare_repo();
        let cell_dir = tempdir("cell-default");
        let opts = FetchOptions {
            allow_local: true,
            timeout: Duration::from_secs(30),
        };
        let fetched = fetch(&cell_dir, bare.to_str().unwrap(), Some("osi"), None, &opts).unwrap();
        assert_eq!(fetched.commit, commit);
        assert!(fetched.checkout_dir.join("model.yaml").exists());
        let _ = std::fs::remove_dir_all(&bare);
        let _ = std::fs::remove_dir_all(&cell_dir);
    }

    #[test]
    fn fetch_by_full_sha_and_then_refresh_reuses_the_cache() {
        let (bare, commit) = make_bare_repo();
        let cell_dir = tempdir("cell-sha");
        let opts = FetchOptions {
            allow_local: true,
            timeout: Duration::from_secs(30),
        };
        let first = fetch(
            &cell_dir,
            bare.to_str().unwrap(),
            None,
            Some(&commit),
            &opts,
        )
        .unwrap();
        assert_eq!(first.commit, commit);
        // Second call hits the existing cache dir (refresh, not clone).
        let second = fetch(
            &cell_dir,
            bare.to_str().unwrap(),
            None,
            Some(&commit),
            &opts,
        )
        .unwrap();
        assert_eq!(second.commit, commit);
        let _ = std::fs::remove_dir_all(&bare);
        let _ = std::fs::remove_dir_all(&cell_dir);
    }

    #[test]
    fn fetch_unknown_ref_is_a_clear_error() {
        let (bare, _commit) = make_bare_repo();
        let cell_dir = tempdir("cell-badref");
        let opts = FetchOptions {
            allow_local: true,
            timeout: Duration::from_secs(30),
        };
        let err = fetch(
            &cell_dir,
            bare.to_str().unwrap(),
            None,
            Some("does-not-exist"),
            &opts,
        )
        .unwrap_err();
        assert!(err.to_string().to_lowercase().contains("ref"));
        let _ = std::fs::remove_dir_all(&bare);
        let _ = std::fs::remove_dir_all(&cell_dir);
    }

    #[test]
    fn url_allowlist_refuses_ext_helper() {
        assert!(validate_url("ext::sh -c false", false).is_err());
    }

    #[test]
    fn url_allowlist_refuses_file_scheme_without_allow_local() {
        assert!(validate_url("file:///tmp/x", false).is_err());
        assert!(validate_url("file:///tmp/x", true).is_ok());
    }

    #[test]
    fn url_allowlist_refuses_git_scheme() {
        assert!(validate_url("git://example.com/x.git", false).is_err());
    }

    #[test]
    fn url_allowlist_refuses_leading_dash() {
        assert!(validate_url("--upload-pack=touch x", false).is_err());
    }

    #[test]
    fn url_allowlist_accepts_https_ssh_and_scp_form() {
        assert!(validate_url("https://example.com/x.git", false).is_ok());
        assert!(validate_url("ssh://git@example.com/x.git", false).is_ok());
        assert!(validate_url("git@example.com:acme/x.git", false).is_ok());
    }

    #[test]
    fn url_allowlist_refuses_local_absolute_path_without_allow_local() {
        assert!(validate_url("/tmp/some/repo", false).is_err());
        assert!(validate_url("/tmp/some/repo", true).is_ok());
    }

    // --- C1: `semantic_model.ref` validation --------------------------------

    #[test]
    fn ref_validation_accepts_ordinary_branch_tag_and_sha() {
        assert!(validate_ref("main").is_ok());
        assert!(validate_ref("release/v1.2.0").is_ok());
        assert!(validate_ref("a".repeat(40).as_str()).is_ok());
    }

    #[test]
    fn ref_validation_refuses_leading_dash_argv_injection() {
        let err = validate_ref("--upload-pack=touch x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("must not start with '-'"), "{err}");
    }

    #[test]
    fn ref_validation_refuses_dotdot_and_bad_characters() {
        assert!(validate_ref("../../etc/passwd").is_err());
        assert!(validate_ref("has space").is_err());
        assert!(validate_ref("semicolon;here").is_err());
        assert!(validate_ref("").is_err());
        assert!(validate_ref(&"x".repeat(256)).is_err());
    }

    #[test]
    fn fetch_refuses_an_injection_shaped_ref_before_spawning_git() {
        let cell_dir = tempdir("cell-ref-injection");
        let opts = FetchOptions {
            allow_local: true,
            timeout: Duration::from_secs(30),
        };
        let err = fetch(
            &cell_dir,
            "/nonexistent/repo/does/not/matter",
            None,
            Some("--upload-pack=touch /tmp/datamk-ref-injection-pwned"),
            &opts,
        )
        .unwrap_err();
        assert!(err.to_string().contains("must not start with '-'"));
        // Refused before any `git` invocation (and before the cache
        // directory `git` would run in is even created).
        assert!(!cell_dir.join(".cell").join("semantic").exists());
        let _ = std::fs::remove_dir_all(&cell_dir);
    }

    // --- C2: `semantic_model.path` validation -------------------------------

    #[test]
    fn path_validation_accepts_ordinary_relative_paths() {
        assert!(validate_path("osi").is_ok());
        assert!(validate_path("osi/sub-dir").is_ok());
    }

    #[test]
    fn path_validation_refuses_leading_dash_option_injection() {
        let err = validate_path("--no-cone").unwrap_err().to_string();
        assert!(err.contains("must not start with '-'"), "{err}");
    }

    #[test]
    fn path_validation_refuses_leading_slash_and_dotdot() {
        assert!(validate_path("/etc/passwd").is_err());
        assert!(validate_path("../escape").is_err());
        assert!(validate_path("osi/../../escape").is_err());
    }

    #[test]
    fn fetch_refuses_an_injection_shaped_path_before_spawning_git() {
        let cell_dir = tempdir("cell-path-injection");
        let opts = FetchOptions {
            allow_local: true,
            timeout: Duration::from_secs(30),
        };
        let err = fetch(
            &cell_dir,
            "/nonexistent/repo/does/not/matter",
            Some("--no-cone"),
            None,
            &opts,
        )
        .unwrap_err();
        assert!(err.to_string().contains("must not start with '-'"));
        let _ = std::fs::remove_dir_all(&cell_dir);
    }

    // --- H1: refresh must advance a moving branch ---------------------------

    #[test]
    fn refresh_by_branch_ref_advances_when_upstream_moves() {
        let (bare, first_commit) = make_bare_repo();
        let cell_dir = tempdir("cell-refresh-branch");
        let opts = FetchOptions {
            allow_local: true,
            timeout: Duration::from_secs(30),
        };

        let first = fetch(
            &cell_dir,
            bare.to_str().unwrap(),
            Some("osi"),
            Some("main"),
            &opts,
        )
        .unwrap();
        assert_eq!(first.commit, first_commit);

        // Advance the bare repo's `main` past the seed commit by cloning
        // it, committing, and pushing back — the same shape a real upstream
        // edit takes.
        let work = tempdir("refresh-work");
        run(&work, &["clone", "-q", bare.to_str().unwrap(), "."]);
        std::fs::write(
            work.join("osi/model.yaml"),
            "version: 0.1.1\nsemantic_model:\n  - name: m2\n    datasets:\n      - name: d\n        source: s\n",
        )
        .unwrap();
        run(&work, &["add", "."]);
        run(&work, &["commit", "-q", "-m", "advance"]);
        run(&work, &["push", "-q", "origin", "main"]);
        let second_commit_out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&work)
            .output()
            .unwrap();
        let second_commit = String::from_utf8_lossy(&second_commit_out.stdout)
            .trim()
            .to_string();
        assert_ne!(second_commit, first_commit);

        // `fetch` reuses the cache dir from the first call — a refresh, not
        // a clone — and must land on the new tip.
        let second = fetch(
            &cell_dir,
            bare.to_str().unwrap(),
            Some("osi"),
            Some("main"),
            &opts,
        )
        .unwrap();
        assert_eq!(second.commit, second_commit);
        assert_ne!(second.commit, first_commit);
        let content = std::fs::read_to_string(second.checkout_dir.join("model.yaml")).unwrap();
        assert!(content.contains("m2"), "{content}");

        let _ = std::fs::remove_dir_all(&bare);
        let _ = std::fs::remove_dir_all(&cell_dir);
        let _ = std::fs::remove_dir_all(&work);
    }

    // --- M2: timeout kills the whole process group, never hangs -----------

    #[test]
    fn run_git_timeout_returns_promptly_instead_of_hanging_on_pipes() {
        // `git ls-remote` against a repo path that doesn't exist still
        // spawns and exits fast on its own; what this test actually
        // exercises is that a near-zero timeout takes the timeout branch
        // (kill + bounded reader join) and returns within a small bound
        // rather than hanging on `stdout_handle.join()`/`stderr_handle.
        // join()` the way the pre-fix code could.
        let opts = FetchOptions {
            allow_local: true,
            timeout: Duration::from_millis(1),
        };
        let start = Instant::now();
        let err = run_git(
            None,
            &[
                "ls-remote".to_string(),
                "https://github.com/apache/ossie.git".to_string(),
            ],
            &opts,
        )
        .unwrap_err();
        assert!(err.stderr.contains("timed out"), "{}", err.stderr);
        // Generous bound (READER_JOIN_GRACE is 2s each for stdout/stderr);
        // well under what an actual hang would look like (the test
        // timeout).
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{:?}",
            start.elapsed()
        );
    }

    // --- M5: no credential in the URL, ever echoed in full -----------------

    #[test]
    fn https_url_with_userinfo_is_refused() {
        let err = validate_url("https://user:s3cr3t@example.com/x.git", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("credential"), "{err}");
        assert!(!err.contains("s3cr3t"), "{err}");
    }

    #[test]
    fn ssh_url_with_login_user_is_not_treated_as_a_credential() {
        // `ssh://git@host/...` is ordinary syntax naming the login user,
        // not a secret — only `https://` userinfo is refused.
        assert!(validate_url("ssh://git@example.com/x.git", false).is_ok());
    }

    #[test]
    fn redact_url_strips_userinfo_query_and_fragment() {
        assert_eq!(
            redact_url("https://user:s3cr3t@example.com/acme/x.git?x=1#y"),
            "https://example.com/acme/x.git"
        );
        assert_eq!(
            redact_url("ssh://git@example.com/acme/x.git"),
            "ssh://example.com/acme/x.git"
        );
    }

    #[test]
    fn classify_error_never_prints_a_credential_from_the_url() {
        let fail = GitFailure {
            stderr: "fatal: Authentication failed for 'https://example.com/x.git'".to_string(),
        };
        let err = classify_error(&fail, "https://user:s3cr3t@example.com/x.git", Some("main"))
            .to_string();
        assert!(!err.contains("s3cr3t"), "{err}");
        assert!(err.contains("example.com"), "{err}");
    }

    // --- L1: submodules are never followed ----------------------------------

    #[test]
    fn fetch_never_recurses_into_a_submodule() {
        // A trap submodule: if `finalize_checkout`'s `git checkout` ever
        // recursed into it (submodule.recurse=true), the submodule's own
        // content would land in the checkout; with submodule.recurse=false
        // it stays an uninitialized, empty directory.
        let sub_work = tempdir("submodule-inner");
        run(&sub_work, &["init", "-q", "-b", "main"]);
        std::fs::write(sub_work.join("PWNED"), "should never be fetched").unwrap();
        run(&sub_work, &["add", "."]);
        run(&sub_work, &["commit", "-q", "-m", "inner"]);
        let sub_bare = tempdir("submodule-bare");
        std::fs::remove_dir_all(&sub_bare).unwrap();
        run(
            &sub_work,
            &["clone", "-q", "--bare", ".", sub_bare.to_str().unwrap()],
        );

        let work = tempdir("submodule-outer");
        run(&work, &["init", "-q", "-b", "main"]);
        std::fs::create_dir_all(work.join("osi")).unwrap();
        std::fs::write(
            work.join("osi/model.yaml"),
            "version: 0.1.1\nsemantic_model:\n  - name: m\n    datasets:\n      - name: d\n        source: s\n",
        )
        .unwrap();
        run(
            &work,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub_bare.to_str().unwrap(),
                "trap",
            ],
        );
        run(&work, &["add", "."]);
        run(&work, &["commit", "-q", "-m", "outer with submodule"]);
        let bare = tempdir("submodule-outer-bare");
        std::fs::remove_dir_all(&bare).unwrap();
        run(
            &work,
            &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
        );

        let cell_dir = tempdir("cell-no-submodule");
        let opts = FetchOptions {
            allow_local: true,
            timeout: Duration::from_secs(30),
        };
        let fetched = fetch(&cell_dir, bare.to_str().unwrap(), None, None, &opts).unwrap();
        assert!(fetched.checkout_dir.join("osi/model.yaml").exists());
        assert!(
            !fetched.checkout_dir.join("trap/PWNED").exists(),
            "submodule content was fetched despite submodule.recurse=false"
        );

        let _ = std::fs::remove_dir_all(&sub_work);
        let _ = std::fs::remove_dir_all(&sub_bare);
        let _ = std::fs::remove_dir_all(&work);
        let _ = std::fs::remove_dir_all(&bare);
        let _ = std::fs::remove_dir_all(&cell_dir);
    }
}
