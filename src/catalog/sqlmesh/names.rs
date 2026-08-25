//! SQLMesh model names as the state store writes them (ADR 0016 §3):
//! `"catalog"."schema"."table"` — double-quoted at every part, on every
//! dialect (BigQuery included), with `""` as the escape, and catalogs that
//! contain hyphens (`"dw-main-silver"`). Never `split('.')`.

use anyhow::{bail, Result};

/// A parsed model name. `catalog` is `None` for a two-part name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelName {
    pub catalog: Option<String>,
    pub schema: String,
    pub table: String,
}

impl ModelName {
    /// `catalog.schema.table` / `schema.table` — the unquoted form SQLMesh
    /// itself uses for `node.name`.
    pub fn unquoted(&self) -> String {
        match &self.catalog {
            Some(c) => format!("{c}.{}.{}", self.schema, self.table),
            None => format!("{}.{}", self.schema, self.table),
        }
    }

    /// `schema.table` — the virtual object a consumer queries in the
    /// model's catalog.
    pub fn object(&self) -> String {
        format!("{}.{}", self.schema, self.table)
    }
}

/// Parse a state-store name: quoted or bare parts separated by `.`, two or
/// three of them. A quoted part may contain any character (including `.`
/// and `-`); `""` inside quotes is a literal quote.
pub fn parse(name: &str) -> Result<ModelName> {
    let mut parts: Vec<String> = Vec::new();
    let mut chars = name.chars().peekable();
    loop {
        let mut part = String::new();
        match chars.peek() {
            Some('"') => {
                chars.next();
                loop {
                    match chars.next() {
                        Some('"') => {
                            if chars.peek() == Some(&'"') {
                                chars.next();
                                part.push('"');
                            } else {
                                break;
                            }
                        }
                        Some(c) => part.push(c),
                        None => bail!("unterminated quoted identifier in model name '{name}'"),
                    }
                }
            }
            Some(_) => {
                while let Some(&c) = chars.peek() {
                    if c == '.' {
                        break;
                    }
                    if c == '"' {
                        bail!("unexpected quote inside a bare identifier in model name '{name}'");
                    }
                    part.push(c);
                    chars.next();
                }
            }
            None => bail!("empty identifier in model name '{name}'"),
        }
        if part.is_empty() {
            bail!("empty identifier in model name '{name}'");
        }
        parts.push(part);
        match chars.next() {
            Some('.') => continue,
            None => break,
            Some(c) => bail!("unexpected '{c}' after an identifier in model name '{name}'"),
        }
    }
    match parts.len() {
        2 => Ok(ModelName {
            catalog: None,
            schema: parts.remove(0),
            table: parts.remove(0),
        }),
        3 => Ok(ModelName {
            catalog: Some(parts.remove(0)),
            schema: parts.remove(0),
            table: parts.remove(0),
        }),
        n => bail!(
            "model name '{name}' has {n} parts; expected schema.table or catalog.schema.table"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_fixture_and_mountain_shapes() {
        let n = parse("\"db\".\"sqlmesh_example\".\"documented_model\"").unwrap();
        assert_eq!(n.catalog.as_deref(), Some("db"));
        assert_eq!(n.object(), "sqlmesh_example.documented_model");
        assert_eq!(n.unquoted(), "db.sqlmesh_example.documented_model");

        // Hyphenated catalog — the mountain shape.
        let n = parse("\"dw-main-silver\".\"invoice\".\"flight_spend\"").unwrap();
        assert_eq!(n.catalog.as_deref(), Some("dw-main-silver"));
        assert_eq!(n.unquoted(), "dw-main-silver.invoice.flight_spend");
    }

    #[test]
    fn handles_bare_parts_dots_inside_quotes_and_escaped_quotes() {
        let n = parse("schema.table").unwrap();
        assert_eq!(n.catalog, None);
        assert_eq!(n.object(), "schema.table");

        let n = parse("\"a.b\".\"c\".\"say \"\"hi\"\"\"").unwrap();
        assert_eq!(n.catalog.as_deref(), Some("a.b"));
        assert_eq!(n.table, "say \"hi\"");
    }

    #[test]
    fn rejects_malformed_names_loudly() {
        for bad in ["\"unterminated", "a", "a.b.c.d", "a..b", "\"a\"x.b", ""] {
            assert!(parse(bad).is_err(), "{bad} should not parse");
        }
    }
}
