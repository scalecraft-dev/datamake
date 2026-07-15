//! Postgres connection config: the `type: postgres` shape in a profile's
//! `connections:` map, and its resolve-time (`${VAR}` expansion) logic.
//!
//! Not to be confused with `catalog: postgres://…` (datamk's own DuckLake
//! metadata DB, `is_metadata_db_catalog`) — same DBMS, unrelated role. This
//! is an *upstream* you read tables from.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::config::bindings::{expand, expand_opt, Redacted, ResolvedConnection};

/// The `sslmode` values libpq accepts, in its own order. datamk's default is
/// `require` — a deliberate divergence from libpq's `prefer`, which silently
/// downgrades to plaintext when the server offers no TLS. A remote source
/// gets encrypted transport unless the profile explicitly opts out
/// (`sslmode: disable`, the local-server shape).
const SSLMODES: [&str; 6] = [
    "disable",
    "allow",
    "prefer",
    "require",
    "verify-ca",
    "verify-full",
];

/// Postgres connection settings. Discrete fields, never a DSN — a
/// `postgres://user:pass@host/db` URL embeds the password literally in a
/// profile field (ADR 0003 §2), and a libpq keyword string that fails to
/// parse echoes the password back in the error text (live-verified). Auth is
/// libpq's ambient chain (`PGPASSWORD`, `~/.pgpass`) unless `password:` is
/// set — and then it must be a pure `${VAR}` reference, never a literal.
/// One connection ≡ one database; a cross-database read is a second
/// connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresConnection {
    /// Server hostname or IP — a hostname, not a URL.
    pub host: String,
    /// Server port. Defaults to 5432.
    #[serde(default)]
    pub port: Option<String>,
    /// The database whose schemas are read — the environment root, analog of
    /// BigQuery's `project`. `table:` paths are `schema.table` under it.
    pub database: String,
    /// The Postgres role datamk logs in as. Required — libpq's fallback (the
    /// OS username) is never right in a deployed pod, and a read-only role
    /// is the recommended shape (GRANT USAGE + SELECT, nothing else).
    /// `Option` only so the missing-user error can say that instead of
    /// serde's terse "missing field".
    #[serde(default)]
    pub user: Option<String>,
    /// Must be a single `${VAR}` reference, never a literal — no secret ever
    /// lives in a profile field (ADR 0003 §2; same rule as Snowflake's
    /// `private_key_passphrase`). Omitted ⇒ libpq's ambient chain
    /// (`PGPASSWORD`, `~/.pgpass`), the analog of BigQuery's ambient ADC.
    #[serde(default)]
    pub password: Option<String>,
    /// libpq `sslmode`. Defaults to `require` (encrypted transport), NOT
    /// libpq's `prefer` (silent plaintext downgrade). Local servers without
    /// TLS set `sslmode: disable` explicitly.
    #[serde(default)]
    pub sslmode: Option<String>,
}

/// Resolve-time `${VAR}` expansion + validation for a `type: postgres`
/// connection block. `name` is the connection's reference name, for errors.
pub fn resolve_postgres(name: &str, pg: &PostgresConnection) -> Result<ResolvedConnection> {
    let Some(user) = expand_opt(&pg.user)? else {
        bail!(
            "connection '{name}' (postgres): `user:` is required — the Postgres role datamk \
             logs in as. libpq's fallback (your OS username) is never right in a deployed pod; \
             use a read-only role (GRANT USAGE ON SCHEMA …; GRANT SELECT ON … TO <role>)."
        );
    };
    let port = match expand_opt(&pg.port)? {
        Some(p) => p.parse::<u16>().map_err(|_| {
            anyhow::anyhow!(
                "connection '{name}' (postgres): `port: {p}` is not a valid port number \
                 (1-65535)"
            )
        })?,
        None => 5432,
    };
    // The password is secret material, not a path — constrain it to pure
    // `${VAR}` form (the whole value, not merely containing one) so the
    // secret lives in the environment, never even partially in the profile
    // file. Same rule, same reasons as Snowflake's `private_key_passphrase`.
    let password = match &pg.password {
        Some(raw) => {
            let is_pure_var =
                raw.starts_with("${") && raw.ends_with('}') && raw.matches("${").count() == 1;
            if !is_pure_var {
                bail!(
                    "connection '{name}' (postgres): `password:` must be a single `${{VAR}}` \
                     reference (e.g. `${{PG_PASSWORD}}`) and nothing else, so the secret lives \
                     in the environment, not in a profile field. Omit it entirely to use \
                     libpq's ambient chain (PGPASSWORD, ~/.pgpass)."
                );
            }
            let expanded = expand(raw)?;
            if expanded.is_empty() {
                bail!(
                    "connection '{name}' (postgres): `password:` ({raw}) expanded to an empty \
                     string — the environment variable is set but empty. Set it to the role's \
                     password, or remove the field to use libpq's ambient chain."
                );
            }
            Some(Redacted(expanded))
        }
        None => None,
    };
    let sslmode = match expand_opt(&pg.sslmode)? {
        Some(m) => {
            if !SSLMODES.contains(&m.as_str()) {
                bail!(
                    "connection '{name}' (postgres): `sslmode: {m}` is not a libpq sslmode — \
                     valid values are {}. datamk defaults to `require` (encrypted transport); \
                     a local server without TLS needs an explicit `sslmode: disable`.",
                    SSLMODES.join(", ")
                );
            }
            m
        }
        None => "require".to_string(),
    };
    Ok(ResolvedConnection::Postgres {
        host: expand(&pg.host)?,
        port,
        database: expand(&pg.database)?,
        user,
        password,
        sslmode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> PostgresConnection {
        PostgresConnection {
            host: "db.internal.example.com".to_string(),
            port: None,
            database: "analytics".to_string(),
            user: Some("datamk_ro".to_string()),
            password: None,
            sslmode: None,
        }
    }

    #[test]
    fn resolve_maps_a_minimal_connection_with_safe_defaults() {
        let r = resolve_postgres("pg", &conn()).unwrap();
        let ResolvedConnection::Postgres {
            host,
            port,
            database,
            user,
            password,
            sslmode,
        } = r
        else {
            panic!("expected postgres");
        };
        assert_eq!(host, "db.internal.example.com");
        assert_eq!(port, 5432);
        assert_eq!(database, "analytics");
        assert_eq!(user, "datamk_ro");
        assert!(password.is_none());
        // The deliberate divergence from libpq's `prefer`: encrypted
        // transport unless the profile explicitly opts out.
        assert_eq!(sslmode, "require");
    }

    #[test]
    fn resolve_requires_user_naming_the_pod_footgun() {
        let mut c = conn();
        c.user = None;
        let err = resolve_postgres("pg", &c).unwrap_err().to_string();
        assert!(err.contains("`user:` is required"), "{err}");
        assert!(err.contains("read-only role"), "{err}");
    }

    #[test]
    fn resolve_parses_the_port_and_rejects_a_bad_one() {
        let mut c = conn();
        c.port = Some("6432".to_string());
        let ResolvedConnection::Postgres { port, .. } = resolve_postgres("pg", &c).unwrap() else {
            panic!("expected postgres");
        };
        assert_eq!(port, 6432);

        c.port = Some("not-a-port".to_string());
        let err = resolve_postgres("pg", &c).unwrap_err().to_string();
        assert!(err.contains("not a valid port"), "{err}");
        assert!(err.contains("not-a-port"), "{err}");
    }

    #[test]
    fn resolve_rejects_a_literal_password() {
        let mut c = conn();
        c.password = Some("hunter2".to_string());
        let err = resolve_postgres("pg", &c).unwrap_err().to_string();
        assert!(err.contains("single `${VAR}` reference"), "{err}");
        assert!(err.contains("PGPASSWORD"), "{err}");
    }

    #[test]
    fn resolve_rejects_a_password_that_merely_embeds_a_var() {
        // `contains("${")` would pass this — literal secret material around
        // a var reference must still be refused.
        std::env::set_var("DATAMK_TEST_PG_PART", "x");
        let mut c = conn();
        c.password = Some("hunter${DATAMK_TEST_PG_PART}2".to_string());
        let err = resolve_postgres("pg", &c).unwrap_err().to_string();
        assert!(err.contains("single `${VAR}` reference"), "{err}");
    }

    #[test]
    fn resolve_fails_loud_on_an_empty_password_expansion() {
        std::env::set_var("DATAMK_TEST_PG_EMPTY_PASS", "");
        let mut c = conn();
        c.password = Some("${DATAMK_TEST_PG_EMPTY_PASS}".to_string());
        let err = resolve_postgres("pg", &c).unwrap_err().to_string();
        assert!(err.contains("expanded to an empty string"), "{err}");
        assert!(err.contains("DATAMK_TEST_PG_EMPTY_PASS"), "{err}");
    }

    #[test]
    fn resolve_accepts_and_expands_a_var_password_into_redacted() {
        std::env::set_var("DATAMK_TEST_PG_PASSWORD", "s3cret");
        let mut c = conn();
        c.password = Some("${DATAMK_TEST_PG_PASSWORD}".to_string());
        let ResolvedConnection::Postgres { password, .. } = resolve_postgres("pg", &c).unwrap()
        else {
            panic!("expected postgres");
        };
        assert_eq!(password.unwrap().0, "s3cret");
    }

    #[test]
    fn resolve_rejects_an_unknown_sslmode_naming_the_valid_set() {
        let mut c = conn();
        c.sslmode = Some("required".to_string());
        let err = resolve_postgres("pg", &c).unwrap_err().to_string();
        assert!(err.contains("sslmode: required"), "{err}");
        assert!(err.contains("verify-full"), "{err}");
        assert!(err.contains("disable"), "{err}");
    }

    #[test]
    fn resolve_accepts_every_libpq_sslmode() {
        for mode in SSLMODES {
            let mut c = conn();
            c.sslmode = Some(mode.to_string());
            let ResolvedConnection::Postgres { sslmode, .. } = resolve_postgres("pg", &c).unwrap()
            else {
                panic!("expected postgres");
            };
            assert_eq!(sslmode, mode);
        }
    }
}
