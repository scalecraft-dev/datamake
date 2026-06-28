use super::schema::{Bindings, CellDef, Source};
use anyhow::{bail, Result};
use indexmap::IndexMap;

/// A cell's environment config with all `${VAR}` references expanded.
#[derive(Debug, Clone)]
pub struct ResolvedBindings {
    pub catalog: String,
    pub storage: String,
    pub s3: Option<ResolvedS3>,
    /// Source name -> resolved source.
    pub sources: IndexMap<String, ResolvedSource>,
    /// Resolved path to the token->roles file, if configured.
    pub principals: Option<String>,
}

/// A source with env references expanded.
#[derive(Debug, Clone)]
pub enum ResolvedSource {
    Raw(String),
    Cell {
        catalog: String,
        storage: String,
        table: String,
        version: Option<u64>,
    },
}

impl ResolvedSource {
    /// Whether this source reads from object storage (needs httpfs/S3 secret).
    pub fn is_remote(&self) -> bool {
        let loc = match self {
            ResolvedSource::Raw(uri) => uri.as_str(),
            ResolvedSource::Cell { storage, .. } => storage.as_str(),
        };
        loc.starts_with("s3://") || loc.starts_with("gs://") || loc.starts_with("gcs://")
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedS3 {
    pub region: Option<String>,
    pub endpoint: Option<String>,
    pub url_style: Option<String>,
    pub key_id: Option<String>,
    pub secret: Option<String>,
    pub use_ssl: Option<bool>,
}

/// Resolve a cell's sources (from `cell.yaml`) against a binding profile (from
/// `profiles/<name>.yaml`), expanding all `${VAR}` references.
pub fn resolve(def: &CellDef, b: &Bindings) -> Result<ResolvedBindings> {
    let s3 = match &b.s3 {
        Some(s) => Some(ResolvedS3 {
            region: expand_opt(&s.region)?,
            endpoint: expand_opt(&s.endpoint)?,
            url_style: expand_opt(&s.url_style)?,
            key_id: expand_opt(&s.key_id)?,
            secret: expand_opt(&s.secret)?,
            use_ssl: s.use_ssl,
        }),
        None => None,
    };

    let mut sources = IndexMap::new();
    for (name, src) in &def.sources {
        let resolved = match src {
            Source::Raw(uri) => ResolvedSource::Raw(expand(uri)?),
            Source::Cell {
                cell,
                table,
                version,
            } => {
                let loc = b.cells.get(cell).ok_or_else(|| {
                    anyhow::anyhow!(
                        "source '{name}' depends on cell '{cell}', but the profile has no \
                         `cells.{cell}` location"
                    )
                })?;
                ResolvedSource::Cell {
                    catalog: expand(&loc.catalog)?,
                    storage: expand(&loc.storage)?,
                    table: expand(table)?,
                    version: *version,
                }
            }
        };
        sources.insert(name.clone(), resolved);
    }

    let principals = expand_opt(&b.principals)?;

    Ok(ResolvedBindings {
        catalog: expand(&b.catalog)?,
        storage: expand(&b.storage)?,
        s3,
        sources,
        principals,
    })
}

fn expand_opt(o: &Option<String>) -> Result<Option<String>> {
    match o {
        Some(s) => {
            let e = expand(s)?;
            Ok((!e.is_empty()).then_some(e))
        }
        None => Ok(None),
    }
}

/// Expand `${VAR}` and `${VAR:-default}` from the environment.
pub fn expand(input: &str) -> Result<String> {
    let mut out = String::new();
    let mut i = 0;
    while i < input.len() {
        if input[i..].starts_with("${") {
            let end = input[i + 2..]
                .find('}')
                .map(|p| i + 2 + p)
                .ok_or_else(|| anyhow::anyhow!("unterminated ${{...}} in '{input}'"))?;
            let (var, default) = match input[i + 2..end].split_once(":-") {
                Some((v, d)) => (v, Some(d)),
                None => (&input[i + 2..end], None),
            };
            match std::env::var(var) {
                Ok(val) => out.push_str(&val),
                Err(_) => match default {
                    Some(d) => out.push_str(d),
                    None => bail!("env var '{var}' unset and has no default"),
                },
            }
            i = end + 1;
        } else {
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}
