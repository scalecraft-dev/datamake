//! Snowflake connection config: the `type: snowflake` shape in a profile's
//! `connections:` map, and its resolve-time (`${VAR}` expansion) logic.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::config::bindings::{expand, expand_opt, Redacted, ResolvedConnection, SnowflakeAuth};

/// Snowflake connection settings. Auth is exactly one of two mechanisms:
/// key-pair (`private_key_path`, a path to a PKCS#8 key file — like BigQuery's
/// `credentials` and `principals`, never a literal token; the typical prod
/// service-account shape) or `authenticator: externalbrowser` (SSO through the
/// user's own browser; the typical local-dev shape — interactive, so refused
/// by deploy pre-flight). One connection ≡ one database; a cross-database
/// read is a second connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnowflakeConnection {
    /// Snowflake account identifier (e.g. `MYORG-ACCOUNT123`). Passed through
    /// unvalidated — account identifiers have too many legitimate forms
    /// (org-account, legacy locator, region-suffixed) for a shape check.
    pub account: String,
    /// The Snowflake user datamk authenticates as. Required by both auth
    /// mechanisms (the extension refuses an externalbrowser secret without
    /// it too) — `Option` only so each mode's missing-user error can say
    /// what the user *is* in that mode.
    #[serde(default)]
    pub user: Option<String>,
    /// The database whose schemas are read — the environment root, analog of
    /// BigQuery's `project`. `table:` paths are `schema.table` under it.
    pub database: String,
    /// Path to a PKCS#8 private key file (key-pair auth). Exactly one of
    /// this or `authenticator:` must be set — `Option` so the neither-set
    /// error can explain both mechanisms instead of serde's terse
    /// "missing field".
    #[serde(default)]
    pub private_key_path: Option<String>,
    /// Passphrase for an encrypted private key. Must be a `${VAR}` reference,
    /// never a literal — this is the one field where the auth model involves
    /// secret material rather than a path, and a literal here would put a
    /// secret in a profile field (ADR 0003 §2).
    #[serde(default)]
    pub private_key_passphrase: Option<String>,
    /// `externalbrowser` (or `ext_browser`) selects SSO auth through the
    /// user's browser. Any other value is a resolve-time error naming the
    /// supported set — password-shaped authenticators (okta, mfa) carry
    /// literal secrets and are refused on the same grounds as `password:`.
    #[serde(default)]
    pub authenticator: Option<String>,
    /// Virtual warehouse to run reads on. Omitted = the user's default
    /// warehouse; a user with no default fails at read time with a rewrite
    /// naming this field.
    #[serde(default)]
    pub warehouse: Option<String>,
    /// Role to assume. Omitted = the user's default role.
    #[serde(default)]
    pub role: Option<String>,
    /// Sentinel: password auth is not supported. Captured (instead of being
    /// silently dropped by serde) so the rejection can say why and name the
    /// supported mechanisms.
    #[serde(default)]
    pub password: Option<String>,
}

/// Resolve-time `${VAR}` expansion + validation for a `type: snowflake`
/// connection block. `name` is the connection's reference name, for errors.
pub fn resolve_snowflake(name: &str, sf: &SnowflakeConnection) -> Result<ResolvedConnection> {
    if sf.password.is_some() {
        bail!(
            "connection '{name}' (snowflake): `password:` is set, but datamk does not support \
             password authentication — no literal secret ever lives in a profile field (ADR \
             0003). Use `private_key_path:` (a PKCS#8 key file — the service-account shape) or \
             `authenticator: externalbrowser` (SSO — the local-dev shape). See \
             docs/guides/snowflake.md."
        );
    }
    let user = expand_opt(&sf.user)?;
    let auth = match (&sf.private_key_path, &sf.authenticator) {
        (Some(_), Some(_)) => bail!(
            "connection '{name}' (snowflake): both `private_key_path:` and `authenticator:` \
             are set — pick exactly one auth mechanism. Typical split: `authenticator: \
             externalbrowser` in a local profile (your own SSO login), `private_key_path:` in \
             a deployed profile (a service account's key file)."
        ),
        (Some(key_path), None) => {
            let Some(user) = user else {
                bail!(
                    "connection '{name}' (snowflake): key-pair auth needs `user:` — the \
                     Snowflake user the key is registered on (ALTER USER <user> SET \
                     RSA_PUBLIC_KEY=...)."
                );
            };
            // The passphrase is the one field whose *value* is secret
            // material, not a path — constrain it to pure `${VAR}` form (the
            // whole value, not merely containing one) so the secret lives in
            // the environment, never even partially in the profile file. And
            // fail loud on an empty expansion: a deployed pod whose secret
            // env var came up empty must say so here, not as a cryptic
            // key-decryption failure inside the ADBC driver.
            let passphrase = match &sf.private_key_passphrase {
                Some(raw) => {
                    let is_pure_var = raw.starts_with("${")
                        && raw.ends_with('}')
                        && raw.matches("${").count() == 1;
                    if !is_pure_var {
                        bail!(
                            "connection '{name}' (snowflake): `private_key_passphrase:` must \
                             be a single `${{VAR}}` reference (e.g. `${{SF_KEY_PASSPHRASE}}`) \
                             and nothing else, so the secret lives in the environment, not in \
                             a profile field."
                        );
                    }
                    let expanded = expand(raw)?;
                    if expanded.is_empty() {
                        bail!(
                            "connection '{name}' (snowflake): `private_key_passphrase:` \
                             ({raw}) expanded to an empty string — the environment variable is \
                             set but empty. Set it to the key's passphrase, or remove the \
                             field if the key is unencrypted."
                        );
                    }
                    Some(Redacted(expanded))
                }
                None => None,
            };
            SnowflakeAuth::KeyPair {
                user,
                private_key_path: expand(key_path)?,
                passphrase,
            }
        }
        (None, Some(authenticator)) => {
            if sf.private_key_passphrase.is_some() {
                bail!(
                    "connection '{name}' (snowflake): `private_key_passphrase:` only applies \
                     to key-pair auth (`private_key_path:`), not `authenticator: \
                     {authenticator}`."
                );
            }
            match expand(authenticator)?.as_str() {
                "externalbrowser" | "ext_browser" => {
                    let Some(user) = user else {
                        bail!(
                            "connection '{name}' (snowflake): `authenticator: externalbrowser` \
                             needs `user:` — the Snowflake login name the browser session \
                             authenticates as."
                        );
                    };
                    SnowflakeAuth::ExternalBrowser { user }
                }
                other => bail!(
                    "connection '{name}' (snowflake): `authenticator: {other}` is not \
                     supported — the only supported value is `externalbrowser` (SSO through \
                     your browser). For non-interactive auth use `private_key_path:` instead; \
                     password-shaped authenticators (okta, mfa) would put a literal secret in \
                     a profile field and are refused on the same grounds as `password:`."
                ),
            }
        }
        (None, None) => bail!(
            "connection '{name}' (snowflake): no auth configured — set exactly one of \
             `private_key_path:` (a PKCS#8 key file; the service-account shape, e.g.\n\n  \
             private_key_path: secrets/sf-key.p8\n\n  openssl genrsa 2048 | openssl pkcs8 \
             -topk8 -inform PEM -out sf-key.p8 -nocrypt\n  -- then in Snowflake: ALTER USER \
             <user> SET RSA_PUBLIC_KEY='<public key>';\n\n) or `authenticator: \
             externalbrowser` (SSO through your browser; the local-dev shape). See \
             docs/guides/snowflake.md."
        ),
    };
    Ok(ResolvedConnection::Snowflake {
        account: expand(&sf.account)?,
        database: expand(&sf.database)?,
        auth,
        warehouse: expand_opt(&sf.warehouse)?,
        role: expand_opt(&sf.role)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keypair() -> SnowflakeConnection {
        SnowflakeConnection {
            account: "MYORG-ACCT".to_string(),
            user: Some("SVC_USER".to_string()),
            database: "ANALYTICS".to_string(),
            private_key_path: Some("/keys/sf.p8".to_string()),
            private_key_passphrase: None,
            authenticator: None,
            warehouse: Some("WH".to_string()),
            role: None,
            password: None,
        }
    }

    fn browser() -> SnowflakeConnection {
        SnowflakeConnection {
            account: "MYORG-ACCT".to_string(),
            user: Some("scotty@example.com".to_string()),
            database: "ANALYTICS".to_string(),
            private_key_path: None,
            private_key_passphrase: None,
            authenticator: Some("externalbrowser".to_string()),
            warehouse: None,
            role: None,
            password: None,
        }
    }

    #[test]
    fn resolve_maps_a_keypair_connection() {
        let r = resolve_snowflake("wh", &keypair()).unwrap();
        let ResolvedConnection::Snowflake {
            account,
            database,
            auth,
            warehouse,
            role,
        } = r
        else {
            panic!("expected snowflake");
        };
        assert_eq!(account, "MYORG-ACCT");
        assert_eq!(database, "ANALYTICS");
        assert_eq!(warehouse.as_deref(), Some("WH"));
        assert_eq!(role, None);
        match auth {
            SnowflakeAuth::KeyPair {
                user,
                private_key_path,
                passphrase,
            } => {
                assert_eq!(user, "SVC_USER");
                assert_eq!(private_key_path, "/keys/sf.p8");
                assert!(passphrase.is_none());
            }
            other => panic!("expected key pair, got {other:?}"),
        }
    }

    #[test]
    fn resolve_maps_an_external_browser_connection() {
        let r = resolve_snowflake("wh", &browser()).unwrap();
        let ResolvedConnection::Snowflake { auth, .. } = r else {
            panic!("expected snowflake");
        };
        assert!(matches!(
            auth,
            SnowflakeAuth::ExternalBrowser { user } if user == "scotty@example.com"
        ));
    }

    #[test]
    fn resolve_accepts_the_ext_browser_spelling() {
        let mut c = browser();
        c.authenticator = Some("ext_browser".to_string());
        let r = resolve_snowflake("wh", &c).unwrap();
        let ResolvedConnection::Snowflake { auth, .. } = r else {
            panic!("expected snowflake");
        };
        assert!(matches!(auth, SnowflakeAuth::ExternalBrowser { .. }));
    }

    #[test]
    fn resolve_requires_user_for_external_browser() {
        // Live-verified: the extension refuses an externalbrowser secret
        // without USER ("Snowflake secret requires field 'user'") — fail at
        // resolve time with the fix, not at attach.
        let mut c = browser();
        c.user = None;
        let err = resolve_snowflake("wh", &c).unwrap_err().to_string();
        assert!(err.contains("needs `user:`"), "{err}");
        assert!(err.contains("browser session"), "{err}");
    }

    #[test]
    fn resolve_rejects_password_with_guidance() {
        let mut c = keypair();
        c.password = Some("hunter2".to_string());
        let err = resolve_snowflake("wh", &c).unwrap_err().to_string();
        assert!(err.contains("password authentication"), "{err}");
        assert!(err.contains("private_key_path"), "{err}");
        assert!(err.contains("externalbrowser"), "{err}");
        assert!(err.contains("connection 'wh'"), "{err}");
    }

    #[test]
    fn resolve_rejects_an_unknown_authenticator_naming_the_supported_set() {
        let mut c = browser();
        c.authenticator = Some("okta".to_string());
        let err = resolve_snowflake("wh", &c).unwrap_err().to_string();
        assert!(err.contains("okta"), "{err}");
        assert!(err.contains("externalbrowser"), "{err}");
        assert!(err.contains("private_key_path"), "{err}");
    }

    #[test]
    fn resolve_rejects_both_auth_mechanisms_at_once() {
        let mut c = keypair();
        c.authenticator = Some("externalbrowser".to_string());
        let err = resolve_snowflake("wh", &c).unwrap_err().to_string();
        assert!(err.contains("exactly one"), "{err}");
        assert!(err.contains("local profile"), "{err}");
    }

    #[test]
    fn resolve_rejects_no_auth_with_both_shapes_named() {
        let mut c = keypair();
        c.private_key_path = None;
        let err = resolve_snowflake("wh", &c).unwrap_err().to_string();
        assert!(err.contains("no auth configured"), "{err}");
        assert!(err.contains("openssl"), "{err}");
        assert!(err.contains("RSA_PUBLIC_KEY"), "{err}");
        assert!(err.contains("externalbrowser"), "{err}");
    }

    #[test]
    fn resolve_requires_user_for_keypair() {
        let mut c = keypair();
        c.user = None;
        let err = resolve_snowflake("wh", &c).unwrap_err().to_string();
        assert!(err.contains("needs `user:`"), "{err}");
    }

    #[test]
    fn resolve_rejects_a_literal_passphrase() {
        let mut c = keypair();
        c.private_key_passphrase = Some("hunter2".to_string());
        let err = resolve_snowflake("wh", &c).unwrap_err().to_string();
        assert!(err.contains("single `${VAR}` reference"), "{err}");
    }

    #[test]
    fn resolve_rejects_a_passphrase_that_merely_embeds_a_var() {
        // `contains("${")` would pass this — literal secret material around
        // a var reference must still be refused.
        std::env::set_var("DATAMK_TEST_SF_PART", "x");
        let mut c = keypair();
        c.private_key_passphrase = Some("hunter${DATAMK_TEST_SF_PART}2".to_string());
        let err = resolve_snowflake("wh", &c).unwrap_err().to_string();
        assert!(err.contains("single `${VAR}` reference"), "{err}");
    }

    #[test]
    fn resolve_fails_loud_on_an_empty_passphrase_expansion() {
        std::env::set_var("DATAMK_TEST_SF_EMPTY_PASS", "");
        let mut c = keypair();
        c.private_key_passphrase = Some("${DATAMK_TEST_SF_EMPTY_PASS}".to_string());
        let err = resolve_snowflake("wh", &c).unwrap_err().to_string();
        assert!(err.contains("expanded to an empty string"), "{err}");
        assert!(err.contains("DATAMK_TEST_SF_EMPTY_PASS"), "{err}");
    }

    #[test]
    fn resolve_rejects_a_passphrase_on_external_browser() {
        let mut c = browser();
        c.private_key_passphrase = Some("${X}".to_string());
        let err = resolve_snowflake("wh", &c).unwrap_err().to_string();
        assert!(err.contains("only applies to key-pair"), "{err}");
    }

    #[test]
    fn resolve_accepts_and_expands_a_var_passphrase() {
        std::env::set_var("DATAMK_TEST_SF_PASSPHRASE", "s3cret");
        let mut c = keypair();
        c.private_key_passphrase = Some("${DATAMK_TEST_SF_PASSPHRASE}".to_string());
        let r = resolve_snowflake("wh", &c).unwrap();
        let ResolvedConnection::Snowflake {
            auth: SnowflakeAuth::KeyPair { passphrase, .. },
            ..
        } = r
        else {
            panic!("expected key pair");
        };
        assert_eq!(passphrase.unwrap().0, "s3cret");
    }

    #[test]
    fn redacted_never_prints_its_contents() {
        let r = Redacted("s3cret".to_string());
        let dbg = format!("{r:?}");
        assert!(!dbg.contains("s3cret"), "{dbg}");
        assert!(dbg.contains("redacted"), "{dbg}");
    }
}
