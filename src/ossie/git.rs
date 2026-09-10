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

    let cache_root = cell_dir.join(".cell").join("semantic");
    std::fs::create_dir_all(&cache_root)
        .with_context(|| format!("creating {}", cache_root.display()))?;
    let key = &sha256_hex(url.as_bytes())[..16];
    let checkout_dir = cache_root.join(key);

    if checkout_dir.join(".git").is_dir() {
        refresh(&checkout_dir, url, git_ref, opts)?;
    } else {
        clone(&checkout_dir, url, git_ref, opts)?;
    }
    finalize_checkout(&checkout_dir, git_ref, path, opts)?;
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

/// `https://`, `ssh://`, or `git@host:path` (scp form) only. Refuses
/// `ext::`/any remote-helper prefix (`::` anywhere), `file://`, `git://`,
/// and a leading `-` (argv injection) — all before `git` is ever invoked.
/// `allow_local` (tests only) additionally accepts a local absolute path or
/// `file://`.
fn validate_url(url: &str, allow_local: bool) -> Result<()> {
    if url.starts_with('-') {
        bail!(
            "`semantic_model.git: {url}` must not start with '-' — refused (it would be read \
             as a git option, not a URL)."
        );
    }
    if url.contains("::") {
        bail!(
            "`semantic_model.git: {url}` contains '::' (a git remote-helper prefix, e.g. \
             `ext::`) — refused; only a plain `https://`, `ssh://`, or `git@host:path` URL is \
             accepted."
        );
    }
    if url.starts_with("https://") || url.starts_with("ssh://") {
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
        "`semantic_model.git: {url}` is not an allowed URL — only `https://`, `ssh://`, or \
         `git@host:path` (scp form) are accepted; `file://`, `git://`, and remote-helper \
         prefixes are refused (meaning must not vary by an untrusted transport)."
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
                    target.to_string(),
                ],
                opts,
            )
            .map_err(|e| classify_error(&e, url, git_ref))?;
            Ok(())
        }
    }
}

/// `git fetch --depth 1 origin <sha>`; falls back to a full (unshallowed)
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
            sha.to_string(),
        ],
        opts,
    );
    if shallow.is_ok() {
        return Ok(());
    }
    run_git(
        Some(checkout_dir),
        &["fetch".to_string(), "origin".to_string(), sha.to_string()],
        opts,
    )
    .map(|_| ())
    .map_err(|e| classify_error(&e, url, Some(sha)))
}

fn finalize_checkout(
    checkout_dir: &Path,
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
                p.to_string(),
            ],
            opts,
        )
        .map_err(|e| generic_error("git sparse-checkout set", &e))?;
    }
    let target = match git_ref.filter(|r| is_full_sha(r)) {
        Some(sha) => sha.to_string(),
        None => "HEAD".to_string(),
    };
    run_git(Some(checkout_dir), &["checkout".to_string(), target], opts)
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

    let mut child = cmd.spawn().map_err(|e| GitFailure {
        stderr: format!("failed to run git: {e}"),
    })?;
    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if start.elapsed() > opts.timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_handle.join();
                    let _ = stderr_handle.join();
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
    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();
    if !status.success() {
        return Err(GitFailure {
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

/// Classify a failed clone/fetch by its stderr (ADR 0018 §3): auth, ref not
/// found, or generic (git's stderr tail included).
fn classify_error(fail: &GitFailure, url: &str, git_ref: Option<&str>) -> anyhow::Error {
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
        let dir = std::env::temp_dir().join(format!(
            "datamk-ossie-git-{tag}-{}-{}",
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
}
