//! The text SQLite keeps in `sqlite_schema.sql`.
//!
//! SQLite stores `CREATE TABLE|INDEX|TRIGGER|VIEW ` followed by the statement
//! as written from the object name on (build.c `sqlite3EndTable`,
//! `sqlite3CreateIndex`, trigger.c `sqlite3FinishTrigger`), so `TEMP`,
//! `IF NOT EXISTS` and a schema qualifier drop out while keyword case,
//! whitespace and comments stay. `ALTER TABLE` then edits that text in place
//! (alter.c). Turso renders the text from its AST instead; the functions here
//! produce SQLite's text, and fall back to the rendered text unless the
//! result parses to the same definition, so the stored text can never
//! describe a different object than the one Turso created.

use turso_parser::ast::{Cmd, Stmt};
use turso_parser::parser::Parser;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Identifier or keyword.
    Word,
    /// `"x"`, `` `x` `` or `[x]`.
    QuotedId,
    /// `'x'`.
    Str,
    /// Anything else, one token per operator or literal.
    Other,
}

#[derive(Debug, Clone, Copy)]
struct Tok {
    kind: Kind,
    start: usize,
    end: usize,
}

fn is_id_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// SQLite's tokens, without whitespace and comments.
fn tokenize(sql: &str) -> Vec<Tok> {
    let bytes = sql.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let b = bytes[i];
        let kind = match b {
            b if b.is_ascii_whitespace() => {
                i += 1;
                continue;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                continue;
            }
            b'"' | b'`' | b'\'' => {
                i += 1;
                loop {
                    match bytes.get(i) {
                        None => break,
                        Some(&c) if c == b => {
                            if bytes.get(i + 1) == Some(&b) {
                                i += 2;
                            } else {
                                i += 1;
                                break;
                            }
                        }
                        Some(_) => i += 1,
                    }
                }
                if b == b'\'' {
                    Kind::Str
                } else {
                    Kind::QuotedId
                }
            }
            b'[' => {
                while i < bytes.len() && bytes[i] != b']' {
                    i += 1;
                }
                i = (i + 1).min(bytes.len());
                Kind::QuotedId
            }
            b if is_id_char(b) && !b.is_ascii_digit() => {
                while i < bytes.len() && is_id_char(bytes[i]) {
                    i += 1;
                }
                Kind::Word
            }
            b if b.is_ascii_digit() => {
                while i < bytes.len() && (is_id_char(bytes[i]) || bytes[i] == b'.') {
                    i += 1;
                }
                Kind::Other
            }
            _ => {
                // Keep UTF-8 boundaries: a non-ASCII byte starts a Word above.
                i += 1;
                Kind::Other
            }
        };
        toks.push(Tok {
            kind,
            start,
            end: i,
        });
    }
    toks
}

fn text<'a>(sql: &'a str, tok: &Tok) -> &'a str {
    &sql[tok.start..tok.end]
}

fn is_word(sql: &str, tok: Option<&Tok>, word: &str) -> bool {
    tok.is_some_and(|tok| tok.kind == Kind::Word && text(sql, tok).eq_ignore_ascii_case(word))
}

fn is_punct(sql: &str, tok: Option<&Tok>, punct: &str) -> bool {
    tok.is_some_and(|tok| tok.kind == Kind::Other && text(sql, tok) == punct)
}

/// The identifier a name token stands for.
fn dequote(raw: &str) -> String {
    let bytes = raw.as_bytes();
    match bytes.first() {
        Some(b'[') => raw[1..raw.len().saturating_sub(1).max(1)].to_string(),
        Some(&q @ (b'"' | b'`' | b'\'')) if raw.len() >= 2 => {
            let quote = q as char;
            raw[1..raw.len() - 1].replace(&format!("{quote}{quote}"), &quote.to_string())
        }
        _ => raw.to_string(),
    }
}

/// The text SQLite stores for a `CREATE TABLE|INDEX|TRIGGER|VIEW` statement,
/// or `None` for other statements (`CREATE TABLE ... AS SELECT`, virtual
/// tables) and text that does not have the expected shape.
fn create_statement_text(input: &str) -> Option<String> {
    let toks = tokenize(input);
    let mut i = 0;
    if !is_word(input, toks.get(i), "CREATE") {
        return None;
    }
    i += 1;
    if is_word(input, toks.get(i), "TEMP") || is_word(input, toks.get(i), "TEMPORARY") {
        i += 1;
    }
    let unique = is_word(input, toks.get(i), "UNIQUE");
    if unique {
        i += 1;
    }
    let kind = ["TABLE", "INDEX", "TRIGGER", "VIEW"]
        .into_iter()
        .find(|kind| is_word(input, toks.get(i), kind))?;
    if unique && kind != "INDEX" {
        return None;
    }
    i += 1;
    if is_word(input, toks.get(i), "IF") {
        if !is_word(input, toks.get(i + 1), "NOT") || !is_word(input, toks.get(i + 2), "EXISTS") {
            return None;
        }
        i += 3;
    }
    if !matches!(toks.get(i)?.kind, Kind::Word | Kind::QuotedId) {
        return None;
    }
    if is_punct(input, toks.get(i + 1), ".") {
        i += 2;
    }
    let name = *toks.get(i)?;
    // The statement ends at a `;` outside the trigger body, or with the text.
    let semi = |from: usize| {
        toks[from..]
            .iter()
            .position(|tok| tok.kind == Kind::Other && text(input, tok) == ";")
            .map(|at| from + at)
    };
    let end = match kind {
        "TABLE" => {
            let lp = i + 1;
            if !is_punct(input, toks.get(lp), "(") {
                return None;
            }
            let mut depth = 0usize;
            let mut rp = None;
            for (at, tok) in toks.iter().enumerate().skip(lp) {
                match (tok.kind, text(input, tok)) {
                    (Kind::Other, "(") => depth += 1,
                    (Kind::Other, ")") => {
                        depth -= 1;
                        if depth == 0 {
                            rp = Some(at);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let rp = rp?;
            match semi(rp + 1) {
                // Without table options the text ends at `)`; with them it
                // runs to the `;` (sqlite3EndTable's pEnd2).
                Some(at) if at == rp + 1 => toks[rp].end,
                None if rp + 1 == toks.len() => toks[rp].end,
                Some(at) => toks[at].start,
                None => input.len(),
            }
        }
        "INDEX" => semi(i).map_or(input.len(), |at| toks[at].start),
        "TRIGGER" => {
            let last = match toks.last() {
                Some(tok) if tok.kind == Kind::Other && text(input, tok) == ";" => {
                    toks.len().checked_sub(2)?
                }
                _ => toks.len() - 1,
            };
            if !is_word(input, toks.get(last), "END") {
                return None;
            }
            toks[last].end
        }
        "VIEW" => {
            let end = semi(i).map_or(input.len(), |at| toks[at].start);
            input[..end].trim_end().len()
        }
        _ => unreachable!(),
    };
    if end <= name.start {
        return None;
    }
    let prefix = if unique { "UNIQUE INDEX" } else { kind };
    Some(format!("CREATE {prefix} {}", &input[name.start..end]))
}

/// A parsed `CREATE` statement in a form that ignores what SQLite drops
/// from the stored text, and how names are quoted.
fn definition(sql: &str) -> Option<Vec<String>> {
    let Ok(Some(Cmd::Stmt(mut stmt))) = Parser::new(sql.as_bytes()).next_cmd() else {
        return None;
    };
    match &mut stmt {
        Stmt::CreateTable {
            temporary,
            if_not_exists,
            tbl_name,
            ..
        } => {
            *temporary = false;
            *if_not_exists = false;
            tbl_name.namespace = None;
        }
        Stmt::CreateIndex {
            if_not_exists,
            idx_name,
            ..
        } => {
            *if_not_exists = false;
            idx_name.namespace = None;
        }
        Stmt::CreateTrigger {
            temporary,
            if_not_exists,
            trigger_name,
            ..
        } => {
            *temporary = false;
            *if_not_exists = false;
            trigger_name.namespace = None;
        }
        Stmt::CreateView {
            temporary,
            if_not_exists,
            view_name,
            ..
        } => {
            *temporary = false;
            *if_not_exists = false;
            view_name.namespace = None;
        }
        _ => return None,
    }
    let rendered = stmt.to_string();
    Some(
        tokenize(&rendered)
            .iter()
            .map(|tok| key(&rendered, tok))
            .collect(),
    )
}

/// `candidate` when it defines the same object as `rendered`, else
/// `rendered`.
fn verified(candidate: Option<String>, rendered: String) -> String {
    match candidate {
        Some(candidate)
            if definition(&candidate).is_some_and(|d| Some(d) == definition(&rendered)) =>
        {
            candidate
        }
        _ => rendered,
    }
}

/// `candidate` when it defines the same table as `rendered`, compared as
/// the tables Turso builds from them, else `rendered`. `ALTER TABLE ...
/// ADD|DROP COLUMN` renders the table from that model, which writes some
/// constraints differently than the stored text does.
fn verified_table(candidate: Option<String>, rendered: String) -> String {
    let table = |sql: &str| {
        crate::schema::BTreeTable::from_sql(sql, 0)
            .ok()
            .map(|table| table.to_sql())
    };
    match candidate {
        Some(candidate) if table(&candidate).is_some_and(|t| Some(t) == table(&rendered)) => {
            candidate
        }
        _ => rendered,
    }
}

/// The `sqlite_schema.sql` text for a `CREATE` statement: SQLite's text
/// taken from `input` when it defines what Turso rendered, else `rendered`.
pub(crate) fn create_sql(input: &str, rendered: String) -> String {
    verified(create_statement_text(input), rendered)
}

/// Where SQLite inserts an added column in a stored `CREATE TABLE`: the
/// comma before the first table constraint, or the closing parenthesis
/// (`Table.u.tab.addColOffset`). Also returns the column name tokens.
fn column_list(sql: &str, toks: &[Tok]) -> Option<(usize, Vec<Tok>)> {
    let lp = toks
        .iter()
        .position(|tok| tok.kind == Kind::Other && text(sql, tok) == "(")?;
    let mut depth = 0usize;
    let mut columns = Vec::new();
    let mut expect_column = true;
    for (at, tok) in toks.iter().enumerate().skip(lp) {
        match (tok.kind, text(sql, tok)) {
            (Kind::Other, "(") => {
                depth += 1;
                continue;
            }
            (Kind::Other, ")") => {
                depth -= 1;
                if depth == 0 {
                    return Some((tok.start, columns));
                }
                continue;
            }
            (Kind::Other, ",") if depth == 1 => {
                let next = toks.get(at + 1);
                if ["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN"]
                    .iter()
                    .any(|word| is_word(sql, next, word))
                {
                    return Some((tok.start, columns));
                }
                expect_column = true;
                continue;
            }
            _ => {}
        }
        if depth == 1 && expect_column {
            columns.push(*tok);
            expect_column = false;
        }
    }
    None
}

/// The column definition of `ALTER TABLE ... ADD [COLUMN] <def>` as
/// written, without a trailing `;`.
pub(crate) fn added_column_text(alter_input: &str) -> Option<String> {
    let toks = tokenize(alter_input);
    let add = toks
        .iter()
        .position(|tok| is_word(alter_input, Some(tok), "ADD"))?;
    let mut start = add + 1;
    if is_word(alter_input, toks.get(start), "COLUMN") {
        start += 1;
    }
    let def = &alter_input[toks.get(start)?.start..];
    Some(
        def.trim_end_matches(|c: char| c == ';' || c.is_ascii_whitespace())
            .to_string(),
    )
}

/// SQLite's text after `ALTER TABLE ... ADD COLUMN`: the column definition
/// as written, inserted before the table constraints.
pub(crate) fn add_column_sql(old: &str, column_def: &str, rendered: String) -> String {
    let toks = tokenize(old);
    let candidate = column_list(old, &toks)
        .map(|(offset, _)| format!("{}, {column_def}{}", &old[..offset], &old[offset..]));
    verified_table(candidate, rendered)
}

/// SQLite's text after `ALTER TABLE ... DROP COLUMN` (alter.c
/// `dropColumnFunc`).
pub(crate) fn drop_column_sql(old: &str, column: &str, rendered: String) -> String {
    let toks = tokenize(old);
    let candidate = column_list(old, &toks).and_then(|(offset, columns)| {
        let index = columns
            .iter()
            .position(|tok| dequote(text(old, tok)).eq_ignore_ascii_case(column))?;
        let (from, to) = match columns.get(index + 1) {
            Some(next) => (columns[index].start, next.start),
            None => (old[..columns[index].start].rfind(',')?, offset),
        };
        Some(format!("{}{}", &old[..from], &old[to..]))
    });
    verified_table(candidate, rendered)
}

/// The comparison key of a token: names compare by the identifier they
/// stand for, case-insensitively.
fn key(sql: &str, tok: &Tok) -> String {
    match tok.kind {
        Kind::Word | Kind::QuotedId => dequote(text(sql, tok)).to_lowercase(),
        Kind::Str | Kind::Other => text(sql, tok).to_string(),
    }
}

/// Carries the renames that turned `old` into `rendered` (Turso's text after
/// `ALTER TABLE ... RENAME`) back to `old`, which keeps everything else as
/// written, as SQLite's `sqlite_rename_table`/`sqlite_rename_column` do. A
/// renamed name is written as Turso rendered it, and in double quotes where
/// `old` quoted it. `written` is the new name as the ALTER statement wrote
/// it: as in SQLite (alter.c `renameEditSql`), it replaces an unquoted name
/// as written there, and goes in double quotes where the ALTER statement
/// quoted it.
pub(crate) fn renamed_sql(old: &str, rendered: String, written: Option<&str>) -> String {
    let written = written.map(|written| {
        let quoted = matches!(written.as_bytes().first(), Some(b'"' | b'`' | b'[' | b'\''))
            || !written.bytes().all(is_id_char)
            || written.as_bytes().first().is_some_and(u8::is_ascii_digit);
        (dequote(written), quoted)
    });
    let old_toks = tokenize(old);
    let new_toks = tokenize(&rendered);
    if old_toks.len() * new_toks.len() > 4_000_000 {
        return rendered;
    }
    let old_keys: Vec<String> = old_toks.iter().map(|tok| key(old, tok)).collect();
    let new_keys: Vec<String> = new_toks.iter().map(|tok| key(&rendered, tok)).collect();
    // Longest common subsequence of the two token lists.
    let (n, m) = (old_keys.len(), new_keys.len());
    let mut lcs = vec![0u32; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i * (m + 1) + j] = if old_keys[i] == new_keys[j] {
                lcs[(i + 1) * (m + 1) + j + 1] + 1
            } else {
                lcs[(i + 1) * (m + 1) + j].max(lcs[i * (m + 1) + j + 1])
            };
        }
    }
    let at = |i: usize, j: usize| lcs[i * (m + 1) + j];
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && old_keys[i] == new_keys[j] {
            i += 1;
            j += 1;
            continue;
        }
        // A block that differs: one name replaced by another is a rename.
        let (block_i, block_j) = (i, j);
        while i < n || j < m {
            if i < n && j < m && old_keys[i] == new_keys[j] {
                break;
            }
            if j < m && (i == n || at(i, j + 1) >= at(i + 1, j)) {
                j += 1;
            } else {
                i += 1;
            }
        }
        if i - block_i == 1 && j - block_j == 1 {
            let (old_tok, new_tok) = (old_toks[block_i], new_toks[block_j]);
            if matches!(old_tok.kind, Kind::Word | Kind::QuotedId)
                && matches!(new_tok.kind, Kind::Word | Kind::QuotedId)
            {
                let new_raw = text(&rendered, &new_tok);
                let (new_name, quote) = match &written {
                    Some((name, quoted)) if dequote(new_raw).eq_ignore_ascii_case(name) => {
                        (name.clone(), *quoted)
                    }
                    _ => (dequote(new_raw), new_tok.kind == Kind::QuotedId),
                };
                let replacement = if old_tok.kind == Kind::QuotedId || quote {
                    let mut quoted = format!("\"{}\"", new_name.replace('"', "\"\""));
                    if old.as_bytes().get(old_tok.end) == Some(&b'"') {
                        quoted.push(' ');
                    }
                    quoted
                } else {
                    new_name
                };
                edits.push((old_tok.start, old_tok.end, replacement));
            }
        }
    }
    if edits.is_empty() {
        return verified(Some(old.to_string()), rendered);
    }
    let mut candidate = old.to_string();
    for (start, end, replacement) in edits.into_iter().rev() {
        candidate.replace_range(start..end, &replacement);
    }
    verified(Some(candidate), rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create(input: &str) -> Option<String> {
        create_statement_text(input)
    }

    #[test]
    fn create_text_follows_sqlite() {
        assert_eq!(
            create("CREATE INDEX IF NOT EXISTS IX on T (C asc);").as_deref(),
            Some("CREATE INDEX IX on T (C asc)")
        );
        assert_eq!(
            create("create unique index main.ix ON t(a) ;").as_deref(),
            Some("CREATE UNIQUE INDEX ix ON t(a) ")
        );
        assert_eq!(
            create("CREATE TEMP TABLE IF NOT EXISTS t (\n  a, -- note\n  b\n);").as_deref(),
            Some("CREATE TABLE t (\n  a, -- note\n  b\n)")
        );
        assert_eq!(
            create("CREATE TABLE t(a) STRICT ;").as_deref(),
            Some("CREATE TABLE t(a) STRICT ")
        );
        assert_eq!(
            create(
                "CREATE TRIGGER IF NOT EXISTS tr DELETE ON t\nBEGIN\n  -- v1\n  SELECT 1;\nEND;"
            )
            .as_deref(),
            Some("CREATE TRIGGER tr DELETE ON t\nBEGIN\n  -- v1\n  SELECT 1;\nEND")
        );
        assert_eq!(
            create("CREATE VIEW v AS\n  SELECT 1  ;").as_deref(),
            Some("CREATE VIEW v AS\n  SELECT 1")
        );
        assert_eq!(create("CREATE TABLE t AS SELECT 1"), None);
        assert_eq!(create("CREATE VIRTUAL TABLE t USING fts5(a)"), None);
    }

    #[test]
    fn create_sql_falls_back_when_the_definition_differs() {
        let rendered = "CREATE TABLE t (a, b)".to_string();
        assert_eq!(create_sql("CREATE TABLE t(a)", rendered.clone()), rendered);
        assert_eq!(
            create_sql("CREATE TABLE IF NOT EXISTS t( a ,b )", rendered),
            "CREATE TABLE t( a ,b )"
        );
    }

    #[test]
    fn alter_edits_follow_sqlite() {
        let old = "CREATE TABLE t(a INT, b TEXT, CONSTRAINT pk PRIMARY KEY (a))";
        assert_eq!(
            add_column_sql(
                old,
                "c int default 0",
                "CREATE TABLE t (a INT, b TEXT, c int DEFAULT 0, CONSTRAINT pk PRIMARY KEY (a))"
                    .into()
            ),
            "CREATE TABLE t(a INT, b TEXT, c int default 0, CONSTRAINT pk PRIMARY KEY (a))"
        );
        assert_eq!(
            drop_column_sql(
                old,
                "b",
                "CREATE TABLE t (a INT, CONSTRAINT pk PRIMARY KEY (a))".into()
            ),
            "CREATE TABLE t(a INT, CONSTRAINT pk PRIMARY KEY (a))"
        );
        assert_eq!(
            drop_column_sql(
                "CREATE TABLE t(a INT,  b TEXT , c)",
                "a",
                "CREATE TABLE t (b TEXT, c)".into()
            ),
            "CREATE TABLE t(b TEXT , c)"
        );
        assert_eq!(
            added_column_text("ALTER TABLE t ADD COLUMN c int default 0 ;").as_deref(),
            Some("c int default 0")
        );
    }

    #[test]
    fn renames_keep_the_rest_of_the_text() {
        assert_eq!(
            renamed_sql(
                "CREATE TABLE t(\n  a int default 0, -- note\n  [b] text\n)",
                "CREATE TABLE t (x int DEFAULT 0, b text)".into(),
                Some("x")
            ),
            "CREATE TABLE t(\n  x int default 0, -- note\n  [b] text\n)"
        );
        assert_eq!(
            renamed_sql(
                "CREATE INDEX i on t(a asc, [b])",
                "CREATE INDEX i ON t (x ASC, b)".into(),
                None
            ),
            "CREATE INDEX i on t(x asc, [b])"
        );
        assert_eq!(
            renamed_sql(
                "CREATE INDEX i on t(\"a\")",
                "CREATE INDEX i ON t (\"new name\")".into(),
                Some("\"New name\"")
            ),
            "CREATE INDEX i on t(\"New name\")"
        );
    }
}
