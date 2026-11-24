//! ADR 0019: checked SQL in `docs:` pages.
//!
//! A page that declares `check: <connection>` promises its fenced `sql`
//! blocks run as written. `verify` dry-runs each of them through the same
//! connector path ADR 0007 §4's `query:` preflight uses, free on BigQuery and
//! unbilled, and fails on a query error. `serve` never runs any of this: a
//! docs page must not be able to stop a server from starting.

use anyhow::{bail, Result};
use duckdb::Connection;
use std::collections::HashMap;
use std::path::Path;

use crate::config::docs::{clause, read_page, DocsOwner};
use crate::config::{Bindings, CellDef, ResolvedConnection};

/// One fenced block to check.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SqlBlock {
    /// 1-based, counted among the page's checkable blocks (info string
    /// exactly `sql`) in document order. `sql no-check` blocks don't count.
    pub index: usize,
    /// 1-based line of the opening fence.
    pub line: usize,
    pub sql: String,
}

/// Every block in `page` whose info string is exactly `sql`, in document
/// order. `sql no-check` is the per-block opt-out; any other language or
/// info string is prose the author never meant to run.
///
/// A deliberately small markdown reader: fenced blocks only (no indented
/// code), a fence opens on a line of up to three spaces then three or more
/// backticks or tildes, and closes on a matching run at least as long with
/// nothing but whitespace after it. An unclosed fence renders nothing, so
/// it checks nothing.
pub(crate) fn extract_sql_blocks(page: &str) -> Vec<SqlBlock> {
    let mut blocks = Vec::new();
    // (fence char, fence length, opening line, whether this block is checked)
    let mut open: Option<(char, usize, usize, bool)> = None;
    let mut body = String::new();

    for (i, raw) in page.lines().enumerate() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let lineno = i + 1;
        match open {
            Some((ch, len, start, checked)) => {
                if is_closing_fence(line, ch, len) {
                    if checked {
                        let index = blocks.len() + 1;
                        blocks.push(SqlBlock {
                            index,
                            line: start,
                            sql: body.trim_end().to_string(),
                        });
                    }
                    open = None;
                    body.clear();
                } else {
                    body.push_str(line);
                    body.push('\n');
                }
            }
            None => {
                if let Some((ch, len, info)) = parse_open_fence(line) {
                    // A non-`sql` block is still consumed as a fence so its
                    // content never parses as prose that opens another one.
                    open = Some((ch, len, lineno, info.trim() == "sql"));
                }
            }
        }
    }
    blocks
}

fn parse_open_fence(line: &str) -> Option<(char, usize, &str)> {
    let rest = line.trim_start_matches(' ');
    if line.len() - rest.len() > 3 {
        return None;
    }
    let ch = rest.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let len = rest.chars().take_while(|c| *c == ch).count();
    if len < 3 {
        return None;
    }
    // A backtick fence's info string may not contain a backtick (CommonMark);
    // it would be a closing fence's character in disguise.
    let info = &rest[len..];
    if ch == '`' && info.contains('`') {
        return None;
    }
    Some((ch, len, info))
}

fn is_closing_fence(line: &str, ch: char, open_len: usize) -> bool {
    let rest = line.trim_start_matches(' ');
    if line.len() - rest.len() > 3 {
        return false;
    }
    let len = rest.chars().take_while(|c| *c == ch).count();
    len >= open_len && len >= 3 && rest[len..].trim().is_empty()
}

/// A `docs:` page that opted in, with enough identity to name it in an error
/// and to report its blocks.
struct CheckedPage<'a> {
    owner: DocsOwner<'a>,
    path: &'a str,
    connection: &'a str,
}

fn checked_pages(def: &CellDef) -> Vec<CheckedPage<'_>> {
    let mut pages = Vec::new();
    if let (Some(path), Some(connection)) = (&def.docs, &def.check) {
        pages.push(CheckedPage {
            owner: DocsOwner::Cell,
            path,
            connection,
        });
    }
    for export in &def.interface {
        if let (Some(path), Some(connection)) = (&export.docs, &export.check) {
            pages.push(CheckedPage {
                owner: DocsOwner::Export(&export.name),
                path,
                connection,
            });
        }
    }
    for d in &def.definitions {
        if let (Some(path), Some(connection)) = (&d.docs, &d.check) {
            pages.push(CheckedPage {
                owner: DocsOwner::Definition(&d.term),
                path,
                connection,
            });
        }
    }
    pages
}

/// What a connector can do for a page check.
enum Capability {
    Ready,
    /// The connector has no free dry run (Postgres, Snowflake, DuckDB), an
    /// expected capability gap, never a failure.
    NoDryRun(&'static str),
    /// Setting the connection up failed in a way that isn't the author's SQL
    /// (extension unavailable, attach/credentials error): warn and skip.
    Unavailable,
}

enum BlockOutcome {
    Ok,
    /// The author's SQL is wrong (`is_dry_run_query_error`): verify fails.
    QueryError(String),
    /// A transport-ish failure: warn and continue.
    Transport(String),
}

/// The seam a test injects a fake at: no warehouse in the default suite.
trait DryRunner {
    fn prepare(&mut self, connection: &str) -> Capability;
    fn dry_run(&mut self, connection: &str, sql: &str) -> BlockOutcome;
}

/// ADR 0019's step in `datamk verify`, after every existing check. Returns
/// `Ok(())` when every opted-in block passes (or is skipped), and an error
/// naming every failing block otherwise.
///
/// `bindings` is the profile's raw config: `check:` names an environment
/// fact, so the name is validated here, at verify time, never at parse time
/// (a connections-free Server profile must still load the cell).
pub(crate) fn run(
    conn: &Connection,
    dir: &Path,
    def: &CellDef,
    bindings: &Bindings,
    profile: &str,
) -> Result<()> {
    validate_connections(def, bindings, profile)?;
    let mut runner = LiveRunner {
        conn,
        bindings,
        prepared: HashMap::new(),
    };
    run_pages(dir, def, &mut runner)
}

fn validate_connections(def: &CellDef, bindings: &Bindings, profile: &str) -> Result<()> {
    for page in checked_pages(def) {
        if !bindings.connections.contains_key(page.connection) {
            bail!(
                "{} `check: {}` names a connection the profile has no `connections.{}` entry \
                 for (profile '{profile}'). Add it to the profile, or drop `check:` to stop \
                 dry-running this page's SQL.",
                clause(page.owner),
                page.connection,
                page.connection
            );
        }
    }
    Ok(())
}

fn run_pages(dir: &Path, def: &CellDef, runner: &mut impl DryRunner) -> Result<()> {
    let mut failures: Vec<String> = Vec::new();
    for page in checked_pages(def) {
        let content = read_page(dir, page.path, page.owner)?;
        let blocks = extract_sql_blocks(&content);
        if blocks.is_empty() {
            continue;
        }
        match runner.prepare(page.connection) {
            Capability::Ready => {}
            Capability::NoDryRun(ty) => {
                tracing::info!(
                    "docs page {} (check: {}): connector '{ty}' has no free dry run, page \
                     skipped",
                    page.path,
                    page.connection
                );
                continue;
            }
            Capability::Unavailable => continue,
        }
        for block in blocks {
            match runner.dry_run(page.connection, &block.sql) {
                BlockOutcome::Ok => {}
                BlockOutcome::QueryError(msg) => failures.push(format!(
                    "  - {} block {} (line {}): {msg}",
                    page.path, block.index, block.line
                )),
                BlockOutcome::Transport(msg) => tracing::warn!(
                    "docs page {} block {} (line {}): dry run failed in a way that doesn't look \
                     like a query error, skipping it: {msg}",
                    page.path,
                    block.index,
                    block.line
                ),
            }
        }
    }
    if !failures.is_empty() {
        bail!(
            "{} `sql` block(s) in `docs:` pages failed their dry run: a recipe that no longer \
             runs is prose that lies. Fix the SQL, or mark a deliberately unrunnable block \
             `sql no-check`:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
    Ok(())
}

struct LiveRunner<'a> {
    conn: &'a Connection,
    bindings: &'a Bindings,
    /// Per connection, so a page set sharing one connection installs and
    /// attaches it once; the setup batch is idempotent either way.
    prepared: HashMap<String, Prepared>,
}

enum Prepared {
    Ready(ResolvedConnection),
    NoDryRun(&'static str),
    Unavailable,
}

impl LiveRunner<'_> {
    fn prepare_connection(&mut self, connection: &str) -> Prepared {
        let cfg = match crate::config::resolve_named_connection(self.bindings, connection) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!(
                    "docs page check: connection '{connection}' could not be resolved ({e}); \
                     its pages are skipped"
                );
                return Prepared::Unavailable;
            }
        };
        if !cfg.has_dry_run() {
            return Prepared::NoDryRun(cfg.type_name());
        }
        // The same sequence `bind_sources` runs for a connection source: a
        // `check:` connection need not back any source, so nothing else may
        // have loaded its extension or attached it.
        let alias = crate::engine::connectors::attach_alias(connection);
        if let Err(e) = self
            .conn
            .execute_batch(cfg.install_load_sql())
            .and_then(|()| self.conn.execute_batch(&cfg.attach_sql(&alias)))
        {
            tracing::warn!(
                "docs page check: connection '{connection}' could not be prepared ({e}); its \
                 pages are skipped"
            );
            return Prepared::Unavailable;
        }
        Prepared::Ready(cfg)
    }
}

impl DryRunner for LiveRunner<'_> {
    fn prepare(&mut self, connection: &str) -> Capability {
        if !self.prepared.contains_key(connection) {
            let prepared = self.prepare_connection(connection);
            self.prepared.insert(connection.to_string(), prepared);
        }
        match &self.prepared[connection] {
            Prepared::Ready(_) => Capability::Ready,
            Prepared::NoDryRun(ty) => Capability::NoDryRun(ty),
            Prepared::Unavailable => Capability::Unavailable,
        }
    }

    fn dry_run(&mut self, connection: &str, sql: &str) -> BlockOutcome {
        let Some(Prepared::Ready(cfg)) = self.prepared.get(connection) else {
            return BlockOutcome::Transport("the connection was never prepared".to_string());
        };
        let Some(dry_run_sql) = cfg.query_dry_run_sql(sql) else {
            return BlockOutcome::Transport("connector has no dry run".to_string());
        };
        match self.conn.query_row(&dry_run_sql, [], |_| Ok(())) {
            Ok(()) => BlockOutcome::Ok,
            Err(e) if cfg.is_dry_run_query_error(&e) => BlockOutcome::QueryError(e.to_string()),
            Err(e) => BlockOutcome::Transport(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(lines: &[&str]) -> String {
        lines.join("\n")
    }

    #[test]
    fn extracts_only_a_bare_sql_info_string() {
        let text = page(&[
            "# Recipes",
            "",
            "```sql",
            "SELECT 1",
            "```",
            "",
            "```sql no-check",
            "SELECT broken",
            "```",
            "",
            "```python",
            "print(1)",
            "```",
            "",
            "```sql title=churn",
            "SELECT 2",
            "```",
        ]);
        let blocks = extract_sql_blocks(&text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].sql, "SELECT 1");
    }

    #[test]
    fn records_one_based_block_index_and_opening_fence_line() {
        let text = page(&[
            "intro", "```sql", "SELECT 1", "```", "", "more", "```sql", "SELECT 2", "```",
        ]);
        let blocks = extract_sql_blocks(&text);
        let summary: Vec<(usize, usize)> = blocks.iter().map(|b| (b.index, b.line)).collect();
        assert_eq!(summary, vec![(1, 2), (2, 7)]);
    }

    #[test]
    fn no_check_blocks_do_not_shift_the_index() {
        let text = page(&[
            "```sql no-check",
            "SELECT broken",
            "```",
            "```sql",
            "SELECT 1",
            "```",
        ]);
        let blocks = extract_sql_blocks(&text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].index, 1);
        assert_eq!(blocks[0].line, 4);
    }

    #[test]
    fn preserves_block_body_and_ignores_a_longer_closing_fence() {
        let text = page(&["~~~~sql", "SELECT 1;", "", "SELECT 2;", "~~~~~", "after"]);
        let blocks = extract_sql_blocks(&text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].sql, "SELECT 1;\n\nSELECT 2;");
    }

    #[test]
    fn an_unclosed_fence_checks_nothing() {
        let text = page(&["```sql", "SELECT 1"]);
        assert!(extract_sql_blocks(&text).is_empty());
    }

    // --- the check step, with a fake runner (no warehouse) ---------------

    struct FakeRunner {
        capability: fn() -> Capability,
        outcomes: Vec<BlockOutcome>,
        seen: Vec<(String, String)>,
    }

    impl FakeRunner {
        fn new(capability: fn() -> Capability, outcomes: Vec<BlockOutcome>) -> Self {
            Self {
                capability,
                outcomes,
                seen: Vec::new(),
            }
        }
    }

    impl DryRunner for FakeRunner {
        fn prepare(&mut self, _connection: &str) -> Capability {
            (self.capability)()
        }

        fn dry_run(&mut self, connection: &str, sql: &str) -> BlockOutcome {
            self.seen.push((connection.to_string(), sql.to_string()));
            if self.outcomes.is_empty() {
                BlockOutcome::Ok
            } else {
                self.outcomes.remove(0)
            }
        }
    }

    fn docs_cell(page: &str, check: &str) -> CellDef {
        serde_yaml::from_str(&format!(
            "cell: t\n\
             docs: {page}\n\
             check: {check}\n\
             interface:\n\
             \x20 - name: e\n    version: 1.0.0\n"
        ))
        .unwrap()
    }

    fn write_page(tag: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "datamk-doc-checks-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("page.md"), content).unwrap();
        dir
    }

    #[test]
    fn a_query_error_fails_naming_the_page_block_and_line() {
        let dir = write_page("query-error", "# t\n\n```sql\nSELECT nope\n```\n");
        let def = docs_cell("page.md", "warehouse");
        let mut runner = FakeRunner::new(
            || Capability::Ready,
            vec![BlockOutcome::QueryError(
                "Unrecognized name: nope".to_string(),
            )],
        );
        let err = run_pages(&dir, &def, &mut runner).unwrap_err().to_string();
        assert!(err.contains("page.md block 1 (line 3)"), "got: {err}");
        assert!(err.contains("Unrecognized name: nope"), "got: {err}");
        assert_eq!(runner.seen.len(), 1);
        assert_eq!(runner.seen[0].0, "warehouse");
    }

    #[test]
    fn a_transport_error_warns_and_continues_to_the_next_block() {
        let dir = write_page(
            "transport",
            "```sql\nSELECT 1\n```\n```sql\nSELECT 2\n```\n",
        );
        let def = docs_cell("page.md", "warehouse");
        let mut runner = FakeRunner::new(
            || Capability::Ready,
            vec![
                BlockOutcome::Transport("connection reset".to_string()),
                BlockOutcome::Ok,
            ],
        );
        run_pages(&dir, &def, &mut runner).expect("transport failures never fail verify");
        assert_eq!(runner.seen.len(), 2);
    }

    #[test]
    fn a_connector_with_no_dry_run_skips_the_page_without_running_anything() {
        let dir = write_page("no-dry-run", "```sql\nSELECT 1\n```\n");
        let def = docs_cell("page.md", "warehouse");
        let mut runner = FakeRunner::new(|| Capability::NoDryRun("duckdb"), vec![]);
        run_pages(&dir, &def, &mut runner).expect("a capability gap is not a failure");
        assert!(runner.seen.is_empty());
    }

    #[test]
    fn an_unknown_connection_name_is_rejected_at_verify_time() {
        let def = docs_cell("page.md", "warehouse");
        let bindings: Bindings =
            serde_yaml::from_str("storage: ./.cell/data\ncatalog: ./.cell/catalog.ducklake\n")
                .unwrap();
        let err = validate_connections(&def, &bindings, "local")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`check: warehouse`"), "got: {err}");
        assert!(err.contains("`connections.warehouse`"), "got: {err}");
        assert!(err.contains("profile 'local'"), "got: {err}");
    }

    #[test]
    fn a_declared_connection_validates() {
        let def = docs_cell("page.md", "warehouse");
        let bindings: Bindings = serde_yaml::from_str(
            "storage: ./.cell/data\n\
             catalog: ./.cell/catalog.ducklake\n\
             connections:\n\
             \x20 warehouse:\n    type: duckdb\n    path: ./wh.duckdb\n",
        )
        .unwrap();
        validate_connections(&def, &bindings, "local").unwrap();
    }

    #[test]
    fn a_block_inside_a_longer_non_sql_fence_is_not_extracted() {
        let text = page(&[
            "````markdown",
            "```sql",
            "SELECT 1",
            "```",
            "````",
            "```sql",
            "SELECT 2",
            "```",
        ]);
        let blocks = extract_sql_blocks(&text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].sql, "SELECT 2");
        assert_eq!(blocks[0].line, 6);
    }
}
