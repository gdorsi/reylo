//! A ClickHouse type as the format's type (`spec/index.ts`), read from the text ClickHouse writes
//! it as (`UInt64`, `Nullable(String)`, `DateTime64(3, 'UTC')`), under ClickHouse's own name for
//! it. ClickHouse says in the type itself whether a value may be NULL: only a `Nullable(…)` holds
//! one, so a column's type settles its nullability, and so does each element's, field's and map
//! value's inside a nested type.
//!
//! A `LowCardinality(T)` and a `SimpleAggregateFunction(f, T)` hold what `T` holds; an `Array` is
//! an array, a `Tuple` whose elements have names a record, a `Map` a map, a `Nested` an array of
//! records, an `Enum8` or `Enum16` text that is one of its labels, and a `JSON` any JSON. A
//! `DateTime` or `DateTime64` is an instant: ClickHouse stores the moment, and the type's time
//! zone only says how it is printed. A `String` is bytes meant as UTF-8 and never validated. A type
//! the format does not structure (a `Tuple` whose elements have no names, a `Variant`, a `Dynamic`,
//! an `Interval`, a geometric type, an aggregate function's state) is opaque.

use reylo_engine_core::types::{Bounds, DateType, DecimalType, Field, FloatType, IllFormed, InstantType, IntegerType, Kind, MapType, Position, Size, TextType, Type};

/// A type as ClickHouse writes it: its name and what stands in its parentheses.
#[derive(Debug, Clone, PartialEq)]
pub struct Written {
    pub name: String,
    pub args: Vec<Arg>,
}

/// One argument of a written type.
#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    /// A type: `Array(String)`'s `String`.
    Type(Written),
    /// A named element: `Tuple(a UInt64)`'s `a UInt64`.
    Field(String, Written),
    /// A string: `DateTime('UTC')`'s zone.
    Text(String),
    /// A number: `FixedString(16)`'s length.
    Number(String),
    /// An enum's label and its number: `'info' = 1`.
    Entry(String, String),
    /// A setting, as `Dynamic(max_types = 8)` writes one.
    Setting,
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl Reader<'_> {
    fn space(&mut self) {
        while self.b.get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.space();
        self.b.get(self.i).copied()
    }

    fn word(&mut self) -> Option<String> {
        self.space();
        let start = self.i;
        while self.b.get(self.i).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') {
            self.i += 1;
        }
        (self.i > start).then(|| String::from_utf8_lossy(&self.b[start..self.i]).into_owned())
    }

    /// A quoted string or name from its opening quote: a backslash escapes, and a quote doubled is one.
    fn quoted(&mut self) -> Option<String> {
        let quote = *self.b.get(self.i)?;
        self.i += 1;
        let mut out = Vec::new();
        loop {
            match *self.b.get(self.i)? {
                b'\\' => {
                    out.push(match *self.b.get(self.i + 1)? {
                        b'n' => b'\n',
                        b't' => b'\t',
                        b'r' => b'\r',
                        b'0' => 0,
                        other => other,
                    });
                    self.i += 2;
                }
                c if c == quote && self.b.get(self.i + 1) == Some(&quote) => {
                    out.push(quote);
                    self.i += 2;
                }
                c if c == quote => {
                    self.i += 1;
                    return Some(String::from_utf8_lossy(&out).into_owned());
                }
                c => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
    }

    fn number(&mut self) -> Option<String> {
        self.space();
        let start = self.i;
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        while self.b.get(self.i).is_some_and(|c| c.is_ascii_digit() || *c == b'.') {
            self.i += 1;
        }
        (self.i > start).then(|| String::from_utf8_lossy(&self.b[start..self.i]).into_owned())
    }

    /// Past the parentheses that open here, to the one that closes them, and what they hold unread.
    fn skip_parentheses(&mut self) -> Option<()> {
        let mut depth = 0usize;
        loop {
            match *self.b.get(self.i)? {
                b'\'' | b'"' | b'`' => {
                    self.quoted()?;
                    continue;
                }
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        self.i += 1;
                        return Some(());
                    }
                }
                _ => {}
            }
            self.i += 1;
        }
    }

    fn ty(&mut self) -> Option<Written> {
        let name = self.word()?;
        let mut args = Vec::new();
        // A JSON type's parentheses hold paths and settings, in a syntax of their own.
        if matches!(name.as_str(), "JSON" | "Object") {
            if self.peek() == Some(b'(') {
                self.skip_parentheses()?;
            }
            return Some(Written { name, args });
        }
        if self.peek() == Some(b'(') {
            self.i += 1;
            if self.peek() == Some(b')') {
                self.i += 1;
                return Some(Written { name, args });
            }
            loop {
                args.push(self.arg()?);
                match self.peek()? {
                    b',' => self.i += 1,
                    b')' => {
                        self.i += 1;
                        break;
                    }
                    _ => return None,
                }
            }
        }
        Some(Written { name, args })
    }

    fn arg(&mut self) -> Option<Arg> {
        match self.peek()? {
            b'\'' => {
                let text = self.quoted()?;
                if self.peek() == Some(b'=') {
                    self.i += 1;
                    return Some(Arg::Entry(text, self.number()?));
                }
                Some(Arg::Text(text))
            }
            c if c.is_ascii_digit() || c == b'-' => self.number().map(Arg::Number),
            b'`' | b'"' => {
                let name = self.quoted()?;
                Some(Arg::Field(name, self.ty()?))
            }
            _ => {
                let start = self.i;
                let first = self.word()?;
                match self.peek() {
                    // `max_types = 8`: a setting, read to the end of its value.
                    Some(b'=') => {
                        self.i += 1;
                        self.number().or_else(|| self.word())?;
                        Some(Arg::Setting)
                    }
                    Some(c) if c.is_ascii_alphabetic() || c == b'_' || c == b'`' => Some(Arg::Field(first, self.ty()?)),
                    _ => {
                        self.i = start;
                        self.ty().map(Arg::Type)
                    }
                }
            }
        }
    }
}

/// The type `text` writes, where it is one type and nothing else.
pub fn parse(text: &str) -> Option<Written> {
    let mut reader = Reader { b: text.as_bytes(), i: 0 };
    let ty = reader.ty()?;
    reader.peek().is_none().then_some(ty)
}

fn integer(bits: u32, unsigned: bool) -> Type {
    Type::new(Kind::Integer(IntegerType { bits, unsigned, ..IntegerType::default() }))
}

/// ClickHouse's floats hold infinity, -infinity and NaN.
fn float(exponent_bits: u32, mantissa_bits: u32) -> Type {
    Type::new(Kind::Float(FloatType { exponent_bits, mantissa_bits, infinity: true, nan: true, ..FloatType::default() }))
}

fn decimal(precision: u32, scale: Option<i32>) -> Type {
    Type::decimal(DecimalType { precision: Some(precision), min_scale: scale, max_scale: scale, ..DecimalType::default() })
}

/// A Tuple's or a Nested's elements as a record's fields, where every one is named and there is one.
fn fields(args: &[Arg]) -> Option<Vec<Field>> {
    let fields: Vec<Field> = args.iter().map(|a| match a {
        Arg::Field(name, ty) => {
            let position = of_written(ty);
            Some(Field::new(name.clone(), position.ty, position.nullable))
        }
        _ => None,
    }).collect::<Option<_>>()?;
    (!fields.is_empty()).then_some(fields)
}

/// ClickHouse's `String`, and a `FixedString` of `size` bytes: bytes meant as UTF-8, never validated.
fn string(size: Option<u64>) -> Type {
    Type::new(Kind::Text(TextType { ill_formed: Some(IllFormed::Utf8), size: size.map(Size::exact), ..TextType::default() }))
}

fn instant(tick: String, min: &str, max: &str) -> Type {
    Type::new(Kind::Instant(InstantType { tick, zone: None, bounds: Bounds::between(min, max), infinity: false }))
}

fn count(args: &[Arg], at: usize) -> Option<u32> {
    match args.get(at)? {
        Arg::Number(n) => n.parse().ok(),
        _ => None,
    }
}

fn inner(args: &[Arg], at: usize) -> Option<&Written> {
    match args.get(at)? {
        Arg::Type(t) => Some(t),
        _ => None,
    }
}

/// The format's type of what `text` writes, and whether the value may be NULL: only where the
/// type is a `Nullable`, or a `Variant`, a `Dynamic` or a `JSON` path, which hold NULL themselves.
pub fn of(text: &str) -> Position {
    match parse(text) {
        Some(written) => of_written(&written),
        None => Position::nullable(Type::opaque(head(text))),
    }
}

/// The name a type's text starts with, for a type the reader does not read whole.
fn head(text: &str) -> String {
    text.trim().chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect()
}

/// The format's type of a written type, and whether its value may be NULL.
pub fn of_written(t: &Written) -> Position {
    let named = |ty: Type| Position::non_null(ty.named(t.name.clone()));
    let unread = || Position::nullable(Type::opaque(t.name.clone()));
    match t.name.as_str() {
        "Nullable" => match inner(&t.args, 0) {
            Some(inner) => Position::nullable(of_written(inner).ty),
            None => unread(),
        },
        // A dictionary encoding of the same values.
        "LowCardinality" => inner(&t.args, 0).map_or_else(unread, of_written),
        // The function's result, kept as a plain value.
        "SimpleAggregateFunction" => inner(&t.args, 1).map_or_else(unread, of_written),
        // A JSON value, alone or inside another: an older server's `Object('json')` too.
        "JSON" | "Object" => Position::non_null(Type::json().named("JSON")),
        "Bool" => named(Type::boolean()),
        "Int8" => named(integer(8, false)),
        "Int16" => named(integer(16, false)),
        "Int32" => named(integer(32, false)),
        "Int64" => named(integer(64, false)),
        "Int128" => named(integer(128, false)),
        "Int256" => named(integer(256, false)),
        "UInt8" => named(integer(8, true)),
        "UInt16" => named(integer(16, true)),
        "UInt32" => named(integer(32, true)),
        "UInt64" => named(integer(64, true)),
        "UInt128" => named(integer(128, true)),
        "UInt256" => named(integer(256, true)),
        "BFloat16" => named(float(8, 7)),
        "Float32" => named(float(8, 23)),
        "Float64" => named(float(11, 52)),
        "Decimal" => match count(&t.args, 0) {
            Some(precision) => named(decimal(precision, Some(count(&t.args, 1).unwrap_or(0) as i32))),
            None => unread(),
        },
        "Decimal32" => named(decimal(9, count(&t.args, 0).map(|s| s as i32))),
        "Decimal64" => named(decimal(18, count(&t.args, 0).map(|s| s as i32))),
        "Decimal128" => named(decimal(38, count(&t.args, 0).map(|s| s as i32))),
        "Decimal256" => named(decimal(76, count(&t.args, 0).map(|s| s as i32))),
        "String" => named(string(None)),
        "FixedString" => match count(&t.args, 0) {
            Some(size) => named(string(Some(size.into()))),
            None => unread(),
        },
        "UUID" => named(Type::uuid()),
        // An address, as the text ClickHouse prints it.
        "IPv4" | "IPv6" => named(Type::text()),
        "Date" => named(Type::new(Kind::Date(DateType { bounds: Bounds::between("1970-01-01", "2149-06-06"), infinity: false }))),
        "Date32" => named(Type::new(Kind::Date(DateType { bounds: Bounds::between("1900-01-01", "2299-12-31"), infinity: false }))),
        "DateTime" | "DateTime32" => named(instant("1".into(), "1970-01-01T00:00:00Z", "2106-02-07T06:28:15Z")),
        "DateTime64" => match count(&t.args, 0) {
            Some(0) => named(instant("1".into(), "1900-01-01T00:00:00Z", "2299-12-31T23:59:59Z")),
            // Nanoseconds from 1970 in a 64-bit integer reach 2262.
            Some(9) => named(instant("1/1000000000".into(), "1900-01-01T00:00:00Z", "2262-04-11T23:47:16.854775807Z")),
            Some(precision) if precision < 9 => named(instant(format!("1/{}", 10u64.pow(precision)), "1900-01-01T00:00:00Z", &format!("2299-12-31T23:59:59.{}Z", "9".repeat(precision as usize)))),
            _ => unread(),
        },
        "Enum8" | "Enum16" => {
            let labels: Option<Vec<String>> = t.args.iter().map(|a| match a {
                Arg::Entry(label, _) | Arg::Text(label) => Some(label.clone()),
                _ => None,
            }).collect();
            match labels {
                Some(labels) if !labels.is_empty() => named(Type::text_of(labels)),
                _ => unread(),
            }
        }
        "Array" => match inner(&t.args, 0) {
            Some(element) => Position::non_null(Type::array(of_written(element))),
            None => unread(),
        },
        "Tuple" => match fields(&t.args) {
            Some(fields) => Position::non_null(Type::record(fields)),
            // Elements with no names: no type of the format describes one.
            None => Position::non_null(Type::opaque("Tuple")),
        },
        // Each row of a nested table, as a record of its columns.
        "Nested" => match fields(&t.args) {
            Some(fields) => Position::non_null(Type::array(Position::non_null(Type::record(fields)))),
            None => unread(),
        },
        "Map" => match (inner(&t.args, 0), inner(&t.args, 1)) {
            (Some(key), Some(value)) => Position::non_null(Type::new(Kind::Map(MapType { key: Box::new(of_written(key).ty), value: Box::new(of_written(value)) }))),
            _ => unread(),
        },
        // The type of a bare NULL, `Nullable(Nothing)`: no value at all.
        "Nothing" => Position::non_null(Type::unknown()),
        // A value of any of several types, or none.
        "Variant" | "Dynamic" => unread(),
        // The format structures none of the rest: its name is all it says, and the value is what
        // ClickHouse writes for it.
        _ => Position::non_null(Type::opaque(t.name.clone())),
    }
}

/// Whether ClickHouse reads `text` as a type a parameter can state: one the reader reads whole.
/// `Identifier`, which a `{name:Identifier}` substitutes a name with, is no type of a value.
pub fn readable(text: &str) -> bool {
    parse(text).is_some_and(|t| t.name != "Identifier")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short(text: &str) -> String {
        let position = of(text);
        format!("{}{}", position.ty.short(), if position.nullable { "?" } else { "" })
    }

    #[test]
    fn each_kind_of_type() {
        assert_eq!(short("UInt64"), "UInt64");
        assert_eq!(serde_json::to_value(of("UInt32").ty).unwrap(), serde_json::json!({"kind": "integer", "bits": 32, "unsigned": true, "label": "UInt32"}));
        assert_eq!(short("Nullable(String)"), "String(utf8)?");
        assert_eq!(short("LowCardinality(Nullable(String))"), "String(utf8)?");
        assert_eq!(short("LowCardinality(String)"), "String(utf8)");
        assert_eq!(short("Decimal(10, 2)"), "Decimal(10,2)");
        assert_eq!(short("Array(Nullable(Int32))"), "Int32?[]");
        assert_eq!(short("Array(Array(String))"), "String(utf8)[][]");
        assert_eq!(short("Tuple(\n    a Int32,\n    b Nullable(String))"), "{ a: Int32, b: String(utf8)? }");
        assert_eq!(short("Tuple(`first name` String)"), "{ first name: String(utf8) }");
        assert_eq!(of("Tuple(UInt64, String)").ty, Type::opaque("Tuple"));
        assert_eq!(short("Map(String, Nullable(Int32))"), "map(String(utf8) => Int32?)");
        assert_eq!(short("Enum8('info' = 1, 'it\\'s' = 2)"), "Enum8('info' | 'it's')");
        assert_eq!(short("SimpleAggregateFunction(sum, UInt64)"), "UInt64");
        assert_eq!(short("Nested(a UInt8, b String)"), "({ a: UInt8, b: String(utf8) })[]");
        assert_eq!(of("JSON(max_dynamic_paths = 8, a.b UInt32, SKIP a.c)").ty, Type::json().named("JSON"));
        assert_eq!(of("Object('json')").ty, Type::json().named("JSON"));
        // A JSON value inside another type is one too, whatever its parentheses hold.
        assert_eq!(short("Array(JSON(a.b UInt32, SKIP REGEXP 'x)'))"), short("Array(JSON)"));
        assert_eq!(short("Tuple(d Date, j JSON, n Nullable(JSON(max_dynamic_types = 4)))"), short("Tuple(d Date, j JSON, n Nullable(JSON))"));
        assert_eq!(of("AggregateFunction(quantile(0.5), Float64)").ty, Type::opaque("AggregateFunction"));
        assert_eq!(of("Variant(String, UInt64)"), Position::nullable(Type::opaque("Variant")));
        assert_eq!(of("Dynamic(max_types = 8)"), Position::nullable(Type::opaque("Dynamic")));
        assert_eq!(of("Nullable(Nothing)"), Position::nullable(Type::unknown()));
        assert_eq!(of("IntervalDay").ty, Type::opaque("IntervalDay"));
        assert_eq!(of("Point").ty, Type::opaque("Point"));
        // An instant by its precision, whatever zone prints it.
        assert_eq!(serde_json::to_value(of("DateTime64(3, 'Europe/Rome')").ty).unwrap()["tick"], "1/1000");
        assert_eq!(serde_json::to_value(of("DateTime('UTC')").ty).unwrap()["tick"], "1");
        assert_eq!(serde_json::to_value(of("FixedString(4)").ty).unwrap(), serde_json::json!({"kind": "text", "ill_formed": "utf8", "size": {"exact": 4}, "label": "FixedString"}));
        for text in ["Bool", "Int8", "Int256", "UInt128", "Float32", "Float64", "BFloat16", "Decimal64(4)", "String", "UUID", "IPv4", "IPv6", "Date", "Date32", "DateTime", "DateTime64(0)", "DateTime64(6)", "DateTime64(9, 'UTC')", "Enum16('a' = 1)"] {
            of(text).ty.check().unwrap_or_else(|e| panic!("{text}: {e}"));
        }
    }

    #[test]
    fn a_parameter_states_a_type_of_a_value() {
        assert!(readable("Array(Tuple(a UInt64, b String))"));
        assert!(readable("DateTime64(3, 'UTC')"));
        assert!(readable("JSON"));
        assert!(!readable("Identifier"));
        assert!(!readable("UInt64) or (1"));
        assert!(!readable(""));
    }
}
