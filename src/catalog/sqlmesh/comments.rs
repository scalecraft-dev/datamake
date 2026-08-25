//! Inline column descriptions, extracted from a model's SQL exactly the way
//! SQLMesh derives them (ADR 0016 §4; `core/model/definition.py`
//! `column_descriptions`): when a model declares no `column_descriptions`,
//! each projection of the **outermost** `SELECT` (the left side of a set
//! operation, after any CTEs) takes the **last** comment attached to it —
//! `select.comments[-1].strip()`.
//!
//! "Attached to" is sqlglot's rule, reproduced here:
//! - a comment that starts on the same line the previous token ended on
//!   attaches to that token; otherwise it attaches to the next token;
//! - the parser then lifts a token's comments onto the expression that
//!   token begins or ends, and a comment on the comma *after* a projection
//!   onto that projection (`_parse_csv`).
//!
//! So a projection's comments are, in source order, those anchored on:
//! its **first** token (a leading comment on its own line); the token that
//! **closes its expression** — the `)`/`]` of a call or subscript, the `END`
//! of a `CASE`, or the sole token of a bare/dotted column or literal — whose
//! node the parser builds right after consuming it (an operand of a binary
//! operator is a child, so `a + b /* c */` reaches nothing); its **alias**
//! token; and the **comma** after it. A comment on any other token (inside a
//! call's arguments, a `WHEN` branch, a CTE) attaches to an inner expression
//! and never reaches the projection — and neither do we. A comment after the
//! last projection on its own line attaches to `FROM`. Verified shape by
//! shape against sqlglot, and by the differential fixture.
//!
//! This is a tokenizer over strings, quoted identifiers, comments and paren
//! depth — deliberately not a SQL parser — held to a differential test
//! against SQLMesh's own output on the fixture project
//! (`test/fixtures/sqlmesh/project_column_descriptions.json`). Any
//! mismatch class is a bug here, not a tolerance.

use indexmap::IndexMap;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// An unquoted word: identifier, keyword, number, or `@macro`.
    Word,
    /// A quoted identifier (`"x"`, `` `x` ``), unquoted value.
    QuotedIdent,
    /// A string literal.
    Str,
    /// One punctuation character.
    Punct(char),
}

#[derive(Debug, Clone)]
struct Tok {
    kind: Kind,
    text: String,
    /// Line the token ended on — what "same line" is measured against.
    end_line: usize,
    /// Comments attached to this token, in source order.
    comments: Vec<String>,
}

impl Tok {
    fn is_word(&self, w: &str) -> bool {
        self.kind == Kind::Word && self.text.eq_ignore_ascii_case(w)
    }
}

/// Words that end a projection list at the list's own paren depth.
const TERMINATORS: &[&str] = &[
    "FROM",
    "WHERE",
    "GROUP",
    "HAVING",
    "WINDOW",
    "QUALIFY",
    "ORDER",
    "LIMIT",
    "OFFSET",
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "FETCH",
    "INTO",
];

/// Reserved words that are never an *implicit* alias (`expr word`). After
/// an explicit `AS`, any word is an alias — `AS time`, `AS date`, `AS
/// first` are all real column names in the wild.
const NOT_AN_ALIAS: &[&str] = &[
    "END",
    "NULL",
    "TRUE",
    "FALSE",
    "DISTINCT",
    "ALL",
    "ASC",
    "DESC",
    "INTERVAL",
    "AS",
    "AND",
    "OR",
    "NOT",
    "IN",
    "IS",
    "LIKE",
    "ILIKE",
    "BETWEEN",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "OVER",
    "PARTITION",
    "BY",
    "ROWS",
    "RANGE",
    "PRECEDING",
    "FOLLOWING",
    "SELECT",
    "EXISTS",
    "CAST",
    "TRY_CAST",
    "SAFE_CAST",
    "COLLATE",
    "ESCAPE",
    "USING",
    "ON",
    "JOIN",
    "LEFT",
    "RIGHT",
    "INNER",
    "OUTER",
    "CROSS",
    "FULL",
    "NATURAL",
    "LATERAL",
    "UNNEST",
    "IGNORE",
    "RESPECT",
    "NULLS",
];

fn tokenize(sql: &str, dialect: &str) -> Vec<Tok> {
    // BigQuery: `"..."` and `'...'` are both strings, identifiers are
    // backticked, and backslash escapes are honored inside strings. The
    // other dialects datamk meets quote identifiers with `"` (and `` ` ``),
    // and escape a quote inside a string by doubling it.
    let bigquery = dialect.eq_ignore_ascii_case("bigquery");
    let chars: Vec<char> = sql.chars().collect();
    let mut toks: Vec<Tok> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let mut line = 1usize;
    let mut i = 0usize;

    let push = |toks: &mut Vec<Tok>,
                pending: &mut Vec<String>,
                kind: Kind,
                text: String,
                end_line: usize| {
        toks.push(Tok {
            kind,
            text,
            end_line,
            comments: std::mem::take(pending),
        });
    };
    let attach_comment = |toks: &mut Vec<Tok>,
                          pending: &mut Vec<String>,
                          text: String,
                          start_line: usize| {
        match toks.last_mut() {
            Some(prev) if prev.end_line == start_line => prev.comments.push(text),
            _ => pending.push(text),
        }
    };

    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // Line comment.
        if c == '-' && chars.get(i + 1) == Some(&'-') {
            let start_line = line;
            let mut j = i + 2;
            let mut text = String::new();
            while j < chars.len() && chars[j] != '\n' {
                text.push(chars[j]);
                j += 1;
            }
            attach_comment(&mut toks, &mut pending, text, start_line);
            i = j;
            continue;
        }
        // Block comment.
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            let start_line = line;
            let mut j = i + 2;
            let mut text = String::new();
            while j < chars.len() && !(chars[j] == '*' && chars.get(j + 1) == Some(&'/')) {
                if chars[j] == '\n' {
                    line += 1;
                }
                text.push(chars[j]);
                j += 1;
            }
            attach_comment(&mut toks, &mut pending, text, start_line);
            i = (j + 2).min(chars.len());
            continue;
        }
        // Strings and quoted identifiers.
        let string_quote = c == '\'' || (bigquery && c == '"');
        let ident_quote = c == '`' || (!bigquery && c == '"');
        if string_quote || ident_quote {
            let quote = c;
            let mut j = i + 1;
            let mut text = String::new();
            while j < chars.len() {
                let d = chars[j];
                if d == '\n' {
                    line += 1;
                }
                if bigquery && string_quote && d == '\\' && j + 1 < chars.len() {
                    text.push(chars[j + 1]);
                    j += 2;
                    continue;
                }
                if d == quote {
                    if chars.get(j + 1) == Some(&quote) {
                        text.push(quote);
                        j += 2;
                        continue;
                    }
                    j += 1;
                    break;
                }
                text.push(d);
                j += 1;
            }
            let kind = if string_quote {
                Kind::Str
            } else {
                Kind::QuotedIdent
            };
            push(&mut toks, &mut pending, kind, text, line);
            i = j;
            continue;
        }
        // Words: identifiers, keywords, numbers, `@macro`s.
        if c.is_alphanumeric() || c == '_' || c == '@' || c == '$' {
            let mut j = i;
            let mut text = String::new();
            while j < chars.len()
                && (chars[j].is_alphanumeric()
                    || chars[j] == '_'
                    || chars[j] == '@'
                    || chars[j] == '$')
            {
                text.push(chars[j]);
                j += 1;
            }
            push(&mut toks, &mut pending, Kind::Word, text, line);
            i = j;
            continue;
        }
        push(&mut toks, &mut pending, Kind::Punct(c), c.to_string(), line);
        i += 1;
    }
    toks
}

/// The index of the outermost `SELECT`: the first at paren depth 0 (after
/// any `WITH` — CTE bodies are parenthesized, so they never qualify), else
/// the first at depth 1 (a parenthesized left side of a set operation).
fn main_select(toks: &[Tok]) -> Option<(usize, i32)> {
    let mut depth = 0i32;
    let mut at_depth_one = None;
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            Kind::Punct('(') | Kind::Punct('[') => depth += 1,
            Kind::Punct(')') | Kind::Punct(']') => depth -= 1,
            _ => {}
        }
        if t.is_word("SELECT") {
            if depth == 0 {
                return Some((i, 0));
            }
            if depth == 1 && at_depth_one.is_none() {
                at_depth_one = Some((i, 1));
            }
        }
    }
    at_depth_one
}

/// Normalize a projection's name the way SQLMesh's rendered query will
/// carry it: case-insensitive dialects (BigQuery, DuckDB) lowercase every
/// identifier, quoted or not; Postgres lowercases unquoted ones and keeps
/// quoted ones; Snowflake uppercases unquoted ones.
fn normalize(name: &str, quoted: bool, dialect: &str) -> String {
    match dialect.to_ascii_lowercase().as_str() {
        "bigquery" | "duckdb" => name.to_ascii_lowercase(),
        "snowflake" => {
            if quoted {
                name.to_string()
            } else {
                name.to_ascii_uppercase()
            }
        }
        _ => {
            if quoted {
                name.to_string()
            } else {
                name.to_ascii_lowercase()
            }
        }
    }
}

/// A word that can name a column: an identifier, not a macro, number, or
/// (for implicit aliases) a reserved word.
fn is_name(t: &Tok, explicit: bool) -> bool {
    match t.kind {
        Kind::QuotedIdent => true,
        Kind::Word => {
            !t.text.starts_with('@')
                && !t.text.starts_with('$')
                && !t.text.chars().next().is_some_and(|c| c.is_ascii_digit())
                && (explicit || !NOT_AN_ALIAS.iter().any(|k| t.text.eq_ignore_ascii_case(k)))
        }
        _ => false,
    }
}

/// `x`, `t.x`, `"T"."X"` — one column reference, and nothing else.
fn is_column_ref(p: &[Tok]) -> bool {
    !p.is_empty()
        && p.len() % 2 == 1
        && p.iter().enumerate().all(|(i, t)| {
            if i % 2 == 0 {
                is_name(t, false)
            } else {
                t.kind == Kind::Punct('.')
            }
        })
}

/// One projection, split into the expression (`this`) and its alias token.
struct Shape<'a> {
    this: &'a [Tok],
    alias: Option<&'a Tok>,
}

fn shape(p: &[Tok]) -> Shape<'_> {
    let n = p.len();
    if n >= 3 && p[n - 2].is_word("AS") && is_name(&p[n - 1], true) {
        return Shape {
            this: &p[..n - 2],
            alias: Some(&p[n - 1]),
        };
    }
    if n >= 2 && is_name(&p[n - 1], false) {
        let before = &p[n - 2];
        let complete = match before.kind {
            Kind::Punct(')') | Kind::Punct(']') | Kind::QuotedIdent | Kind::Str => true,
            Kind::Word => {
                before.is_word("END")
                    || !NOT_AN_ALIAS
                        .iter()
                        .any(|k| before.text.eq_ignore_ascii_case(k))
            }
            Kind::Punct(_) => false,
        };
        if complete {
            return Shape {
                this: &p[..n - 1],
                alias: Some(&p[n - 1]),
            };
        }
    }
    Shape {
        this: p,
        alias: None,
    }
}

/// `alias_or_name` for one projection: its alias, else a bare or dotted
/// column's last part; `None` for anything without a name (`*`, a function
/// call with no alias, a macro).
fn projection_name(sh: &Shape<'_>, dialect: &str) -> Option<String> {
    if let Some(a) = sh.alias {
        return Some(normalize(&a.text, a.kind == Kind::QuotedIdent, dialect));
    }
    if is_column_ref(sh.this) {
        let last = sh.this.last()?;
        return Some(normalize(
            &last.text,
            last.kind == Kind::QuotedIdent,
            dialect,
        ));
    }
    None
}

/// Whether a comment on the last token of `this` reaches the projection:
/// the parser builds `this`'s root node right after that token when it is a
/// closing `)`/`]` or `END`, or when `this` is one column reference or one
/// literal. An operand of a binary operator (`a + b`) is a child node.
fn closes_this(this: &[Tok]) -> bool {
    match this.last() {
        None => false,
        Some(t) => match t.kind {
            Kind::Punct(')') | Kind::Punct(']') => true,
            Kind::Word if t.is_word("END") => true,
            Kind::Str => this.len() == 1,
            Kind::Word | Kind::QuotedIdent => is_column_ref(this) || this.len() == 1,
            Kind::Punct(_) => false,
        },
    }
}

/// Column name -> description for `sql`, per the rules in the module doc.
/// Empty when the query has no comments on its projections.
pub fn column_descriptions(sql: &str, dialect: &str) -> IndexMap<String, String> {
    let toks = tokenize(sql, dialect);
    let mut out = IndexMap::new();
    let Some((select_idx, select_depth)) = main_select(&toks) else {
        return out;
    };
    let mut i = select_idx + 1;
    // `SELECT DISTINCT` / `SELECT ALL`.
    if toks
        .get(i)
        .is_some_and(|t| t.is_word("DISTINCT") || t.is_word("ALL"))
    {
        i += 1;
    }
    let mut depth = select_depth;
    // Each projection: its tokens and the comma that followed it.
    let mut projections: Vec<(Vec<Tok>, Option<Tok>)> = Vec::new();
    let mut current: Vec<Tok> = Vec::new();
    while i < toks.len() {
        let t = &toks[i];
        match t.kind {
            Kind::Punct('(') | Kind::Punct('[') => depth += 1,
            Kind::Punct(')') | Kind::Punct(']') => {
                depth -= 1;
                if depth < select_depth {
                    break;
                }
            }
            _ => {}
        }
        if depth == select_depth {
            // `* EXCEPT (…)` / `* REPLACE (…)` are star modifiers, not set
            // operations: a set-op `EXCEPT` is followed by `SELECT`/`ALL`/
            // `DISTINCT`, never by `(`.
            let star_modifier = (t.is_word("EXCEPT") || t.is_word("REPLACE"))
                && toks.get(i + 1).is_some_and(|n| n.kind == Kind::Punct('('));
            if !star_modifier
                && (t.kind == Kind::Punct(';') || TERMINATORS.iter().any(|w| t.is_word(w)))
            {
                break;
            }
            if t.kind == Kind::Punct(',') {
                projections.push((std::mem::take(&mut current), Some(t.clone())));
                i += 1;
                continue;
            }
        }
        current.push(t.clone());
        i += 1;
    }
    if !current.is_empty() {
        projections.push((current, None));
    }

    for (p, comma) in projections {
        let sh = shape(&p);
        let Some(name) = projection_name(&sh, dialect) else {
            continue;
        };
        let mut comments: Vec<&String> = Vec::new();
        // Anchors that reach the projection root, in source order.
        if let Some(first) = p.first() {
            comments.extend(first.comments.iter());
        }
        if p.len() > 1 && closes_this(sh.this) {
            if let Some(last) = sh.this.last() {
                comments.extend(last.comments.iter());
            }
        }
        if let Some(a) = sh.alias {
            comments.extend(a.comments.iter());
        }
        if let Some(c) = &comma {
            comments.extend(c.comments.iter());
        }
        if let Some(last) = comments.last() {
            out.insert(name, last.trim().to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_comment_form_matches_sqlmesh_on_the_fixture_model() {
        let sql = r#"WITH base AS (
  SELECT
    item_id, -- cte comment: should NOT count
    num_orders
  FROM sqlmesh_example.full_model
)
SELECT
  item_id, -- trailing: the item
  -- leading: orders before the projection
  num_orders,
  /* block trailing */ num_orders * 2 AS doubled, /* block after */
  CAST(num_orders AS DOUBLE) AS as_double,
  base.item_id AS "Quoted, Alias", -- quoted alias
  'a -- not a comment' AS lit -- string with dashes
  -- final line comment after last projection
FROM base
UNION ALL
SELECT item_id, num_orders, 0, 0.0, 'x', 'y' -- right side: should NOT count
FROM base"#;
        let got = column_descriptions(sql, "duckdb");
        let want: IndexMap<String, String> = [
            ("item_id", "trailing: the item"),
            ("num_orders", "leading: orders before the projection"),
            ("doubled", "block after"),
            ("quoted, alias", "quoted alias"),
            ("lit", "string with dashes"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn comment_placement_follows_sqlglot_exactly() {
        // Verified against sqlglot: a comment after the comma belongs to
        // the projection BEFORE it; a comment nested inside a call's
        // arguments reaches nothing; one on the closing paren reaches the
        // alias; one on the SELECT line is the SELECT's, not a column's.
        let sql =
            "SELECT -- not a column\n  a, /* after comma */ COUNT(b /* inner */) AS n\nFROM t";
        let got = column_descriptions(sql, "duckdb");
        assert_eq!(got.get("a").map(String::as_str), Some("after comma"));
        assert_eq!(got.get("n"), None);

        let sql = "SELECT\n  SUM(x) -- trailing after paren\n  AS y,\n  CASE WHEN a THEN 1 END -- case\n  , CASE WHEN a THEN 1 END c -- case alias\nFROM t";
        let got = column_descriptions(sql, "duckdb");
        assert_eq!(
            got.get("y").map(String::as_str),
            Some("trailing after paren")
        );
        assert_eq!(got.get("c").map(String::as_str), Some("case alias"));
        assert_eq!(
            got.len(),
            2,
            "the nameless CASE contributes nothing: {got:?}"
        );
    }

    #[test]
    fn implicit_aliases_dotted_columns_and_nameless_projections() {
        let sql = "SELECT t.x -- x\n, SUM(y) total -- total\n, z + 1 -- nameless\n, * -- star\n, @macro -- macro\nFROM t";
        let got = column_descriptions(sql, "postgres");
        assert_eq!(got.get("x").map(String::as_str), Some("x"));
        assert_eq!(got.get("total").map(String::as_str), Some("total"));
        assert_eq!(got.len(), 2, "{got:?}");
    }

    /// Shapes from mountain's models, each checked against sqlglot.
    #[test]
    fn mountain_shapes_star_modifiers_subscripts_keyword_aliases_and_interior_comments() {
        let sql = "SELECT\n  *\n  EXCEPT (t_z)\n  , 2 * exp(z) * (\n    t_z * (1 + t_z)\n  ) AS p_value /* two-sided */\n  , split(v.ip, '/')[OFFSET(0)] AS impression_ip /* revert */\n  , CASE WHEN a = 6 /* Friday */ THEN 1 END AS rev_weekly_flag\n  , (\n    x\n  ) /* rate-form */ * n AS incremental_visits\n  , end_ts AS time /* UTC */\nFROM t";
        let got = column_descriptions(sql, "bigquery");
        assert_eq!(got.get("p_value").map(String::as_str), Some("two-sided"));
        assert_eq!(got.get("impression_ip").map(String::as_str), Some("revert"));
        assert_eq!(got.get("time").map(String::as_str), Some("UTC"));
        assert_eq!(
            got.get("rev_weekly_flag"),
            None,
            "a WHEN-branch comment is interior"
        );
        assert_eq!(
            got.get("incremental_visits"),
            None,
            "a paren operand's comment is interior"
        );
        assert_eq!(got.len(), 3, "{got:?}");

        // Leading-comma style: a comment on its own line before `, b`
        // anchors on the comma, which belongs to the projection BEFORE it
        // (`a`) — sqlglot's rule, however it reads. A set-op EXCEPT still
        // terminates the list.
        let sql = "SELECT\n  a\n  -- lead\n  , b\n  , SUM(c) d -- implicit\nEXCEPT DISTINCT\nSELECT 1, 2, 3 -- no\nFROM t";
        let got = column_descriptions(sql, "duckdb");
        assert_eq!(got.get("a").map(String::as_str), Some("lead"));
        assert_eq!(got.get("b"), None);
        assert_eq!(got.get("d").map(String::as_str), Some("implicit"));
        assert_eq!(got.len(), 2, "{got:?}");
    }

    #[test]
    fn dialect_normalization_of_names() {
        let sql = "SELECT Foo AS \"MixedCase\" -- c\n, Bar -- d\nFROM t";
        let pg = column_descriptions(sql, "postgres");
        assert!(
            pg.contains_key("MixedCase") && pg.contains_key("bar"),
            "{pg:?}"
        );
        let bq = column_descriptions(
            "SELECT Foo AS `MixedCase` -- c\n, Bar -- d\nFROM t",
            "bigquery",
        );
        assert!(
            bq.contains_key("mixedcase") && bq.contains_key("bar"),
            "{bq:?}"
        );
        let sf = column_descriptions(sql, "snowflake");
        assert!(
            sf.contains_key("MixedCase") && sf.contains_key("BAR"),
            "{sf:?}"
        );
    }

    #[test]
    fn bigquery_double_quoted_strings_do_not_open_identifiers() {
        let sql = "SELECT \"a -- b\" AS s, -- the string\n  c -- c\nFROM t";
        let got = column_descriptions(sql, "bigquery");
        assert_eq!(got.get("s").map(String::as_str), Some("the string"));
        assert_eq!(got.get("c").map(String::as_str), Some("c"));
    }

    /// The differential fixture: SQLMesh's own `column_descriptions` for
    /// every model of `test/fixtures/sqlmesh/project`, against what this
    /// extractor reads off the SQL the state store holds for the same
    /// models. Declared models are skipped (SQLMesh doesn't derive there).
    #[test]
    fn matches_sqlmesh_on_every_fixture_model() {
        let expected: serde_json::Value = serde_json::from_str(include_str!(
            "../../../test/fixtures/sqlmesh/project_column_descriptions.json"
        ))
        .unwrap();
        let mut checked = 0;
        for line in include_str!("../../../test/fixtures/sqlmesh/state/_snapshots.jsonl").lines() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            let snapshot: serde_json::Value =
                serde_json::from_str(row["snapshot"].as_str().unwrap()).unwrap();
            let node = &snapshot["node"];
            let name = node["name"].as_str().unwrap();
            let Some(sql) = node["query"]["sql"].as_str() else {
                continue; // seeds have no query
            };
            let want = &expected[name];
            if want["declared"].as_bool() == Some(true) {
                continue;
            }
            let dialect = node["dialect"].as_str().unwrap_or("duckdb");
            let got = column_descriptions(sql, dialect);
            let got_json: serde_json::Value = serde_json::to_value(&got).unwrap();
            assert_eq!(got_json, want["column_descriptions"], "model {name}");
            checked += 1;
        }
        assert!(
            checked >= 4,
            "the fixture must exercise the extractor: {checked}"
        );
    }
}
