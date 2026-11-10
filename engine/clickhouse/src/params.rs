//! A ClickHouse statement's parameters. ClickHouse binds a parameter as `{name:Type}`, a
//! substitution that states its type, and types none from where it stands, so each parameter of a
//! statement states its type too: as ClickHouse's own `{id:UInt64}`, or as `:id` with a cast right
//! after it, `:id::UInt64`. The one parameter that states none is a whole value of an
//! `INSERT … VALUES`, whose type is its column's ([`crate::catalog`]).
//!
//! The statement the engine hands back numbers each parameter, `{p1:UInt64}`, which is what the
//! driver binds: an author's name may hold a dot, and a name of ClickHouse's own may not. The
//! statement it has described holds a value of each parameter's type in its place,
//! `defaultValueOfTypeName('UInt64')`, since ClickHouse describes no statement whose
//! substitutions are not set.

use reylo_engine_core::lex::{self, Lexed, Placeholder, Rewritten, Target};

/// The type a parameter states where it is written: a substitution's own, or the cast right
/// after a `:name`. `None` where it states none.
pub fn stated(sql: &str, p: &Placeholder) -> Option<String> {
    if sql.as_bytes().get(p.start) == Some(&b'{') {
        return lex::substitution_type(sql, p.start, p.end).map(str::to_string);
    }
    cast_after(sql, p.end)
}

/// The type of the `::Type` cast written right after the offset `end`: a name, and what stands in
/// its parentheses, to the one that closes them.
fn cast_after(sql: &str, end: usize) -> Option<String> {
    let b = sql.as_bytes();
    let space = |mut i: usize| {
        while b.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        i
    };
    let mut i = space(end);
    if !(b.get(i) == Some(&b':') && b.get(i + 1) == Some(&b':')) {
        return None;
    }
    i = space(i + 2);
    let start = i;
    while b.get(i).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') {
        i += 1;
    }
    if i == start {
        return None;
    }
    if b.get(i) == Some(&b'(') {
        let mut depth = 0usize;
        loop {
            match b.get(i)? {
                quote @ (b'\'' | b'"' | b'`') => {
                    i += 1;
                    loop {
                        match b.get(i)? {
                            b'\\' => i += 2,
                            c if c == quote => {
                                i += 1;
                                break;
                            }
                            _ => i += 1,
                        }
                    }
                }
                b'(' => {
                    depth += 1;
                    i += 1;
                }
                b')' => {
                    depth -= 1;
                    i += 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => i += 1,
            }
        }
    }
    Some(sql[start..i].to_string())
}

/// A type's text as a ClickHouse string literal: a backslash and a quote escaped.
fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The parameter's number, as the statement binds it: its place in the binding order, from 1.
fn number(lexed: &Lexed, name: &str) -> usize {
    lexed.params.iter().position(|p| p == name).map_or(0, |i| i + 1)
}

/// The statement as the driver runs it: each parameter as `{pN:Type}`, by its number and the type
/// `types` gives its name; one with no type keeps the `$N` no statement runs with.
pub fn rewritten(sql: &str, lexed: &Lexed, types: &dyn Fn(&str) -> Option<String>) -> Rewritten {
    lex::rewrite_with(sql, lexed, Target::Dollar, &|name, rendered| match types(name) {
        Some(ty) => format!("{{p{}:{ty}}}", number(lexed, name)),
        None => rendered.to_string(),
    })
}

/// The statement as the engine describes it: a value of each parameter's type in its place.
pub fn probe(sql: &str, lexed: &Lexed, types: &dyn Fn(&str) -> Option<String>) -> Rewritten {
    lex::rewrite_with(sql, lexed, Target::Dollar, &|name, rendered| match types(name) {
        Some(ty) => format!("defaultValueOfTypeName({})", literal(&ty)),
        None => rendered.to_string(),
    })
}

/// The word that stands for parameter `n` in the text the tree is read from, which reads a
/// substitution's braces as nothing of ClickHouse's.
pub fn marker(n: usize) -> String {
    format!("reylo_parameter_{n}")
}

/// The number of the parameter a word of the tree stands for.
pub fn marked(word: &str) -> Option<usize> {
    word.strip_prefix("reylo_parameter_")?.parse().ok()
}

/// The statement as the tree is read from it: each parameter as a word that names its number.
pub fn parsable(sql: &str, lexed: &Lexed) -> Rewritten {
    lex::rewrite_with(sql, lexed, Target::Dollar, &|name, _| marker(number(lexed, name)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reylo_engine_core::dialect::Dialect;

    fn types(sql: &str) -> Vec<Option<String>> {
        let lexed = lex::lex(sql, Dialect::Clickhouse).unwrap();
        lexed.placeholders.iter().map(|p| stated(sql, p)).collect()
    }

    #[test]
    fn a_parameter_states_its_type() {
        assert_eq!(types("select {id:UInt64}, :name::String, :n"), [Some("UInt64".into()), Some("String".into()), None]);
        assert_eq!(types("select :at::DateTime64(3, 'UTC'), { tags : Array(LowCardinality(String)) }"), [Some("DateTime64(3, 'UTC')".into()), Some("Array(LowCardinality(String))".into())]);
        assert_eq!(types("select :q :: Nullable(String) from t"), [Some("Nullable(String)".into())]);
        // A cast further on is the expression's, not the parameter's.
        assert_eq!(types("select (:a + 1)::UInt8"), [None]);
    }

    #[test]
    fn the_statement_is_written_for_the_driver_and_for_the_describe() {
        let sql = "select id from events where id = :id::UInt64 and tag = {tag:Nullable(String)} and at > :auth.since::DateTime('UTC')";
        let lexed = lex::lex(sql, Dialect::Clickhouse).unwrap();
        let stated: Vec<(String, Option<String>)> = lexed.placeholders.iter().map(|p| (p.name.clone(), super::stated(sql, p))).collect();
        let types = |name: &str| stated.iter().find(|(n, _)| n == name).and_then(|(_, t)| t.clone());
        let run = rewritten(sql, &lexed, &types);
        assert_eq!(run.sql, "select id from events where id = {p1:UInt64}::UInt64 and tag = {p2:Nullable(String)} and at > {p3:DateTime('UTC')}::DateTime('UTC')");
        assert_eq!(run.order, ["id", "tag", "auth.since"]);
        let described = probe(sql, &lexed, &types);
        assert_eq!(described.sql, "select id from events where id = defaultValueOfTypeName('UInt64')::UInt64 and tag = defaultValueOfTypeName('Nullable(String)') and at > defaultValueOfTypeName('DateTime(\\'UTC\\')')::DateTime('UTC')");
        // An offset of the described text is one of the author's.
        let at = described.sql.find("tag =").unwrap();
        assert_eq!(&sql[described.to_original(at)..][..5], "tag =");
        assert_eq!(parsable(sql, &lexed).sql, "select id from events where id = reylo_parameter_1::UInt64 and tag = reylo_parameter_2 and at > reylo_parameter_3::DateTime('UTC')");
        assert_eq!(marked("reylo_parameter_12"), Some(12));
    }
}
