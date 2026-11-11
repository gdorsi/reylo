// The run time on ClickHouse, over @clickhouse/client's client (and @clickhouse/client-web's):
// `query()` runs a statement that returns rows and `command()` one that returns none. The rows are
// asked for as JSONCompact, with the settings that make each value's JSON exact, and each value is
// given its type's one representation (src/values.ts) by the result's own column types, which the
// format names beside the rows: an Int64 or wider integer is a BigInt, a Decimal its exact text,
// a Date `YYYY-MM-DD`, a DateTime its ISO 8601 instant in UTC, an Array an array, a Tuple of named
// elements an object of them, a Map an object of its keys, and a JSON its value.
//
// JSON has one number, and JavaScript reads it as a double, which holds no integer past 2^53
// exactly. ClickHouse can write an integer of 64 bits or more as a string of its digits, which is
// exact, but it writes the integers inside a JSON column that way too, where a number is what the
// document holds. So a result that may hold a document (a JSON, Dynamic or Variant column, alone
// or inside a Tuple or a Map) is asked for with its integers as numbers and read from its text,
// each integer a double cannot hold from its digits (`JSON.parse`'s source text access); a result
// of plain columns, or a runtime without that access, has them quoted and is read as it is.
//
// ClickHouse binds a parameter by its text: the HTTP interface's `param_<name>` beside the
// statement, read as the type the substitution states (`{p1:Array(String)}`). The client's own
// writer of that text takes a tuple only as a value of its TupleParam class and a number at
// JavaScript's precision, so the run time writes each text itself, by the parameter's type, and
// hands it to the client as the setting the HTTP interface reads it from. The text is measured on
// the server (ClickHouse 26.8): a value that stands alone is escaped as a tab-separated value is,
// NULL is `\N`, and a value inside an array, a tuple or a map is written as a literal, a string
// quoted and a number bare.
import { daysFromCivil, DecodeError, describe, ParameterError } from "./values.ts";
import { bareName, type EngineType } from "./engine-type.ts";
import type { Columns } from "./values.ts";
import { namesInResult, rowOf } from "./row.ts";

/** @clickhouse/client's and @clickhouse/client-web's ClickHouseClient: `run` takes it. */
export type ClickhouseClient = {
  query(params: { query: string; format: "JSONCompact"; clickhouse_settings?: any }): Promise<{ json(): Promise<any>; text(): Promise<string> }>;
  command(params: { query: string; clickhouse_settings?: any }): Promise<unknown>;
  insert(params: any): Promise<unknown>;
};

/** Whether a client is ClickHouse's: pg's and mariadb's have a query() too, and neither a command() nor an insert(). */
export function isClickhouse(c: { [key: string]: unknown } | null | undefined): boolean {
  return c != null && typeof c.query === "function" && typeof c.command === "function" && typeof c.insert === "function";
}

/**
 * What makes each value's JSON exact: a decimal as its digits, every digit of its scale among
 * them, a NaN or an infinity as its name, a DateTime as its ISO 8601 instant in UTC, a tuple of
 * named elements as an object of them, and a map as an object.
 */
const OUTPUT = {
  output_format_json_quote_decimals: 1,
  output_format_json_quote_denormals: 1,
  output_format_decimal_trailing_zeros: 1,
  output_format_json_named_tuples_as_objects: 1,
  output_format_json_map_as_array_of_tuples: 0,
  date_time_output_format: "iso",
} as const;

/** Whether this runtime's `JSON.parse` hands its reviver each value's source text. */
let sourceAccess: boolean | undefined;
function readsSource(): boolean {
  if (sourceAccess === undefined) {
    sourceAccess = false;
    try {
      JSON.parse("[1]", ((_key: string, v: unknown, context?: { source?: unknown }) => {
        if (typeof context?.source === "string") sourceAccess = true;
        return v;
      }) as (key: string, v: unknown) => unknown);
    } catch {}
  }
  return sourceAccess;
}

/** An integer a double does not hold exactly, from its digits. */
const exactIntegers = ((_key: string, v: unknown, context?: { source?: string }) => (typeof v === "number" && !Number.isSafeInteger(v) && context?.source !== undefined && /^-?\d+$/.test(context.source) ? BigInt(context.source) : v)) as (key: string, v: unknown) => unknown;

/** An integer written in sixteen digits or more, the fewest of one a double does not hold exactly: with none in a result's text, plain `JSON.parse` reads it as the reviver would. */
const WIDE = /(?<![\d.])\d{16,}(?![\d.eE])/;

/** Whether a column of this kind may hold a JSON document: a JSON column's, and one the module has no kind for (a Tuple, a Map, a Variant). */
const mayHoldJson = (kind: string) => /^(?:json|unknown)(?:\[\])*$/.test(kind);

/** The error ClickHouse writes into a result it had started: its number and its name, as the client's own error keeps them. */
function inResult(text: string): Error {
  const m = /Code: (\d+)\. DB::Exception: ([\s\S]*?)\s*(?:\(([A-Z][A-Z0-9_]+)\))?\s*(?:\(version [^)]*\))?\s*$/.exec(text);
  return Object.assign(new Error(m?.[2] ?? text), { code: m?.[1] ?? "0", type: m?.[3] });
}

/** Whether a statement with no columns in its module returns rows, by the word it opens with. */
const RETURNS_ROWS = /^(?:\s|--[^\n]*(?:\n|$)|#[ !][^\n]*(?:\n|$)|\/\*[\s\S]*?\*\/|\()*(?:select|with|from|show|describe|desc|explain|exists|check)\b/i;

/**
 * Runs `text` with `params`, the text of each parameter in the statement's order (`{p1:…}` takes
 * the first), and returns its rows, each value given its kind. `columns` is empty for a statement
 * that returns no rows, which runs as a command.
 */
export async function runClickhouse(c: ClickhouseClient, text: string, params: readonly string[], columns: Columns | undefined): Promise<unknown[]> {
  const bound: Record<string, string> = {};
  params.forEach((value, i) => (bound[`param_p${i + 1}`] = value));
  if (columns === undefined ? !RETURNS_ROWS.test(text) : columns.length === 0) {
    await c.command({ query: text, clickhouse_settings: bound });
    return [];
  }
  const fromText = readsSource() && (columns === undefined || columns.some(([, kind]) => mayHoldJson(kind)));
  const result = await c.query({ query: text, format: "JSONCompact", clickhouse_settings: { ...OUTPUT, output_format_json_quote_64bit_integers: fromText ? 0 : 1, ...bound } });
  const json = fromText ? await result.text() : undefined;
  // Whether the reviver may have read an integer as a BigInt, inside a document too.
  const exact = json !== undefined && WIDE.test(json);
  const body = (json === undefined ? await result.json() : JSON.parse(json, exact ? exactIntegers : undefined)) as { meta?: { name: string; type: string }[]; data?: unknown[][]; exception?: unknown } | null;
  if (typeof body?.exception === "string") throw inResult(body.exception);
  if (body === null || typeof body !== "object" || !Array.isArray(body.meta) || !Array.isArray(body.data)) {
    throw new DecodeError(`ClickHouse answered with ${describe(body)}, which is no JSONCompact result: a result has its columns under "meta" and its rows under "data".`);
  }
  const names = namesInResult(columns?.map(([name]) => name) ?? body.meta.map((m) => m.name), body.meta.map((m) => m.name));
  const decoders = body.meta.map((m) => decoderOf(m.type, exact));
  // Each row by Reylo's row builder (src/row.ts): a column named `__proto__` is an own property.
  const build = rowOf(names);
  const decoded = new Array<unknown>(names.length);
  return body.data.map((row) => {
    for (let i = 0; i < names.length; i++) decoded[i] = decoders[i] === same ? row[i] : decoders[i]!(row[i], names[i]!);
    return build(decoded);
  });
}

// --- Binding ---------------------------------------------------------------------------------

/** A value that stands alone, escaped as a tab-separated value is. */
const escaped = (s: string) => s.replace(/[\\\t\n\r\0]/g, (c) => ESCAPES[c]!);
/** A string inside an array, a tuple or a map: a quoted literal. */
const quoted = (s: string) => `'${s.replace(/[\\'\t\n\r\0]/g, (c) => ESCAPES[c]!)}'`;
const ESCAPES: Record<string, string> = { "\\": "\\\\", "'": "\\'", "\t": "\\t", "\n": "\\n", "\r": "\\r", "\0": "\\0" };

/**
 * The text ClickHouse reads for a parameter of the engine's type `type` given `v`, checked
 * against the type first: NULL is `\N`. `jsonNull` is JSON's null for a JSON parameter the
 * statement takes no SQL NULL for. A parameter with no type, of a query defined by hand, is
 * written by the value it is given.
 */
export function clickhouseParam(type: EngineType | undefined, v: unknown, name: string, jsonNull = false): string {
  if (jsonNull) return "null";
  if (v === null || v === undefined) return "\\N";
  return write(type, v, name, false, "");
}

/** `v`, never null, as the text of a value of type `t`: alone, or `inside` an array, a tuple or a map. `at` says where in the parameter it stands. */
function write(t: EngineType | undefined, v: unknown, name: string, inside: boolean, at: string): string {
  const refuse = (what: string): never => {
    throw new ParameterError(`Parameter "${name}"${at && ` at ${at}`} is ${what}; got ${describe(v)}.`);
  };
  const text = (s: string) => (inside ? quoted(s) : escaped(s));
  switch (t?.kind) {
    case "boolean":
      return typeof v === "boolean" ? String(v) : refuse("a Bool, which takes true or false");
    case "integer": {
      if (typeof v === "number" && !Number.isInteger(v)) return refuse(`${label(t)}, an integer, which takes no fraction, NaN or infinity`);
      if (typeof v !== "bigint" && typeof v !== "number" && !(typeof v === "string" && /^\s*[+-]?\d+\s*$/.test(v))) return refuse(`${label(t)}, an integer, which takes a number, a BigInt or a string of its digits`);
      // ClickHouse reads an integer past its type's range as the one its low bits make (300 as a UInt8 is 44).
      const n = BigInt(typeof v === "string" ? v.trim() : v);
      const [min, max] = t.unsigned ? [0n, (1n << BigInt(t.bits)) - 1n] : [-(1n << BigInt(t.bits - 1)), (1n << BigInt(t.bits - 1)) - 1n];
      return n >= min && n <= max ? n.toString() : refuse(`${label(t)}, which holds ${min} to ${max}`);
    }
    case "float":
      return typeof v === "number" ? float(v) : typeof v === "bigint" ? String(v) : refuse(`${label(t)}, which takes a number`);
    case "decimal":
      if (typeof v === "bigint" || (typeof v === "number" && Number.isFinite(v))) return String(v);
      return typeof v === "string" && /^\s*[+-]?(?:\d+\.?\d*|\.\d+)(?:e[+-]?\d+)?\s*$/i.test(v) ? v.trim() : refuse(`${label(t)}, which takes its digits as a string, or a number`);
    case "date":
      // ClickHouse reads a date past its type's range as the nearest one it holds.
      if (typeof v === "string" && /^\d{4}-\d\d-\d\d$/.test(v) && ((t.min !== undefined && v < t.min) || (t.max !== undefined && v > t.max))) return refuse(`${label(t)}, which holds ${t.min} to ${t.max}`);
      return typeof v === "string" ? text(v) : refuse(`${label(t)}, a date, which takes a string ("2026-10-12")`);
    case "text":
    case "uuid":
      return typeof v === "string" ? text(v) : refuse(`${label(t)}, which takes a string`);
    case "instant":
      return typeof v === "string" ? instant(t, v, inside, refuse) : refuse(`${label(t)}, an instant, which takes a string that names its zone ("2026-10-12T19:06:48.282Z"), not a Date: pass date.toISOString()`);
    case "json":
      // A JSON value is its JSON text, which stands quoted inside another value.
      return text(JSON.stringify(v));
    case "array": {
      if (!Array.isArray(v)) return refuse("an Array, which takes an array");
      return `[${v.map((e, i) => element(t.element.type, t.element.nullable === true, e, name, `${at}[${i}]`)).join(",")}]`;
    }
    case "record": {
      if (v === null || typeof v !== "object" || Array.isArray(v)) return refuse("a Tuple of named elements, which takes an object of them");
      return `(${t.fields.map((f) => element(f.type, f.nullable === true, (v as Record<string, unknown>)[f.name], name, `${at}.${f.name}`)).join(",")})`;
    }
    case "map": {
      if (v === null || typeof v !== "object" || Array.isArray(v)) return refuse("a Map, which takes an object of its keys");
      const entries = v instanceof Map ? [...v] : Object.entries(v);
      return `{${entries.map(([k, e]) => `${key(t.key, k, name, at)}:${element(t.value.type, t.value.nullable === true, e, name, `${at}[${JSON.stringify(k)}]`)}`).join(",")}}`;
    }
    default:
      return plain(v, inside, refuse);
  }
}

/** A value inside an array, a tuple or a map: NULL where its type takes one. */
function element(t: EngineType, nullable: boolean, v: unknown, name: string, at: string): string {
  if (v !== null && v !== undefined) return write(t, v, name, true, at);
  if (nullable || t.kind === "unknown" || t.kind === "opaque") return "NULL";
  throw new ParameterError(`Parameter "${name}" at ${at} is ${v === null ? "null" : "missing"}, and its type, ${label(t)}, takes no NULL there.`);
}

/** A map's key, which an object holds as a string: a number's or a truth value's digits stand bare, as its type reads them. */
function key(t: EngineType, k: unknown, name: string, at: string): string {
  const where = `${at}[${JSON.stringify(k)}]`;
  if (typeof k !== "string") return write(t, k, name, true, where);
  if (t.kind === "float") return write(t, k === "nan" ? NaN : k === "inf" ? Infinity : k === "-inf" ? -Infinity : Number(k), name, true, where);
  if (t.kind === "boolean" && (k === "true" || k === "false")) return k;
  return write(t, k, name, true, where);
}

/** A value of a type the format does not structure (a Tuple of unnamed elements, a Variant, an Interval), written by what it is. */
function plain(v: unknown, inside: boolean, refuse: (what: string) => never): string {
  if (typeof v === "string") return inside ? quoted(v) : escaped(v);
  if (typeof v === "number") return float(v);
  if (typeof v === "bigint" || typeof v === "boolean") return String(v);
  if (Array.isArray(v)) return `[${v.map((e) => (e === null || e === undefined ? "NULL" : plain(e, true, refuse))).join(",")}]`;
  return refuse("of a type Reylo writes a string, a number, a BigInt, a boolean or an array of them for");
}

const label = (t: EngineType) => bareName(t) || t.kind;

/** A number as ClickHouse reads a float: NaN and the infinities by their names. */
const float = (v: number) => (Number.isNaN(v) ? "nan" : v === Infinity ? "inf" : v === -Infinity ? "-inf" : String(v));

/** The floats JSON has no number for, by the name a result writes each with. */
export const NON_FINITE: ReadonlyMap<unknown, number> = new Map([["nan", NaN], ["-nan", NaN], ["inf", Infinity], ["+inf", Infinity], ["-inf", -Infinity]]);

const INSTANT = /^\s*(\d{4})-(\d\d)-(\d\d)[T ](\d\d):(\d\d)(?::(\d\d)(?:[.,](\d{1,9}))?)?\s*(?:(Z)|([+-])(\d\d)(?::?(\d\d))?)\s*$/i;

/**
 * An instant's text, which names its zone, as the count of seconds since 1970 that ClickHouse
 * reads for a DateTime or a DateTime64 in any time zone: a date and a time alone it would read
 * in the type's own zone. A DateTime holds whole seconds. Where it stands alone, ClickHouse reads
 * a count of fewer than five digits as a year, so it is written ten digits wide, and reads no
 * instant within the second before 1970, whose sign it drops.
 */
function instant(t: EngineType & { kind: "instant" }, v: string, inside: boolean, refuse: (what: string) => never): string {
  const m = INSTANT.exec(v);
  if (m === null) return refuse(`${label(t)}, an instant, so a string for it names its zone as Z or an offset ("2026-10-12T19:06:48.282Z", "2026-10-12 19:06:48+09:00")`);
  const [year, month, day, hour, minute, second] = [m[1], m[2], m[3], m[4], m[5], m[6] ?? "0"].map(Number) as [number, number, number, number, number, number];
  if (month < 1 || month > 12 || day < 1 || day > 31 || hour > 23 || minute > 59 || second > 59) return refuse(`${label(t)}, an instant, and this is no date and time`);
  const offset = m[8] === undefined ? (m[9] === "-" ? -1 : 1) * (Number(m[10]) * 3600 + Number(m[11] ?? "0") * 60) : 0;
  const seconds = daysFromCivil(year, month, day) * 86400 + hour * 3600 + minute * 60 + second - offset;
  const fraction = m[7] ?? "";
  const whole = bareName(t) !== "DateTime64";
  if (whole) {
    if (/[1-9]/.test(fraction)) return refuse("a DateTime, which holds whole seconds");
    return inside || seconds < 0 ? String(seconds) : String(seconds).padStart(10, "0");
  }
  const nanos = BigInt(seconds) * 1000000000n + BigInt(fraction.padEnd(9, "0"));
  const abs = nanos < 0n ? -nanos : nanos;
  const digits = (abs % 1000000000n).toString().padStart(9, "0").replace(/0+$/, "") || "0";
  const count = (abs / 1000000000n).toString();
  if (nanos >= 0n) return `${inside ? count : count.padStart(10, "0")}.${digits}`;
  if (!inside && count === "0") return refuse("a DateTime64 that stands alone, for which ClickHouse reads no instant within the second before 1970-01-01T00:00:00Z");
  return `-${count}.${digits}`;
}

// --- Decoding --------------------------------------------------------------------------------

type Decoder = (v: unknown, column: string) => unknown;

/** A value whose JSON is its representation already: a string, a number of 32 bits or fewer, a truth value, and a type the format does not structure. */
const same: Decoder = (v) => v;

/** A document's value, each number as JSON reads it: an integer past 2^53 as the double nearest it, as `JSON.parse` gives one. */
const document: Decoder = (v) => {
  if (typeof v === "bigint") return Number(v);
  if (v === null || typeof v !== "object") return v;
  if (Array.isArray(v)) {
    for (let i = 0; i < v.length; i++) if (typeof v[i] === "bigint" || (v[i] !== null && typeof v[i] === "object")) v[i] = document(v[i], "");
    return v;
  }
  const o = v as Record<string, unknown>;
  for (const k of Object.keys(o)) if (typeof o[k] === "bigint" || (o[k] !== null && typeof o[k] === "object")) o[k] = document(o[k], "");
  return v;
};

/** Each type's decoder, for a result read as it is and for one whose integers the reviver read (`exact`). */
const decoders = { plain: new Map<string, Decoder>(), exact: new Map<string, Decoder>() };

/**
 * The decoder of a ClickHouse type, by its name as a result writes it (`Array(Nullable(Int64))`).
 * `exact` says the result's integers past 2^53 are BigInts, which a document's are turned back from.
 */
function decoderOf(type: string, exact: boolean): Decoder {
  const cache = exact ? decoders.exact : decoders.plain;
  let found = cache.get(type);
  if (found === undefined) {
    found = build(type, exact);
    // A statement's types are few; a type written with a value in it (an Enum's labels) is one more each.
    if (cache.size < 4096) cache.set(type, found);
  }
  return found;
}

/** A type's name and what its parentheses hold, split at the commas that stand in no inner parentheses or string. */
function parts(type: string): { name: string; args: string[] } {
  const open = type.indexOf("(");
  if (open === -1 || !type.endsWith(")")) return { name: type.trim(), args: [] };
  const args: string[] = [];
  let depth = 0;
  let start = open + 1;
  for (let i = open + 1; i < type.length - 1; i++) {
    const c = type[i]!;
    if (c === "'" || c === '"' || c === "`") {
      for (i++; i < type.length - 1 && type[i] !== c; i++) if (type[i] === "\\") i++;
    } else if (c === "(") depth++;
    else if (c === ")") depth--;
    else if (c === "," && depth === 0) {
      args.push(type.slice(start, i).trim());
      start = i + 1;
    }
  }
  const last = type.slice(start, -1).trim();
  if (last !== "" || args.length) args.push(last);
  return { name: type.slice(0, open).trim(), args };
}

/** A Tuple's element as it is written: `name Type` for a named one, the type alone otherwise. */
function tupleElement(text: string): { name?: string; type: string } {
  const m = /^(?:`((?:[^`\\]|\\[\s\S])*)`|([A-Za-z_][A-Za-z0-9_]*))\s+(\S[\s\S]*)$/.exec(text);
  if (m === null) return { type: text };
  return { name: m[1] !== undefined ? m[1].replace(/\\([\s\S])/g, "$1") : m[2]!, type: m[3]! };
}

function build(type: string, exact: boolean): Decoder {
  const { name, args } = parts(type);
  switch (name) {
    case "Nullable":
    case "LowCardinality":
      // NULL is JSON's null, which every decoder below leaves as it is.
      return args.length === 1 ? decoderOf(args[0]!, exact) : same;
    case "SimpleAggregateFunction":
      return args.length ? decoderOf(args[args.length - 1]!, exact) : same;
    case "Int64": case "Int128": case "Int256": case "UInt64": case "UInt128": case "UInt256":
      return (v, column) => {
        if (v === null) return null;
        if (typeof v === "bigint") return v;
        if (typeof v === "string" && /^-?\d+$/.test(v)) return BigInt(v);
        if (typeof v === "number" && Number.isSafeInteger(v)) return BigInt(v);
        throw new DecodeError(`Column "${column}" is ${type}, and holds ${describe(v)}, which is no integer a result writes exactly.`);
      };
    case "Float32": case "Float64": case "BFloat16":
      // A float with no fraction is written as an integer, which may have been read as one.
      return (v) => (typeof v === "bigint" ? Number(v) : typeof v !== "string" ? v : (NON_FINITE.get(v) ?? Number(v)));
    case "JSON": case "Object": case "Dynamic": case "Variant":
      return exact ? document : same;
    case "Decimal": case "Decimal32": case "Decimal64": case "Decimal128": case "Decimal256":
      return (v) => (typeof v === "number" ? String(v) : v);
    case "Nothing":
      return () => null;
    case "Array": {
      if (args.length !== 1) return same;
      const inner = decoderOf(args[0]!, exact);
      if (inner === same) return same;
      return (v, column) => {
        if (Array.isArray(v)) for (let i = 0; i < v.length; i++) v[i] = inner(v[i], column);
        return v;
      };
    }
    case "Tuple":
    case "Nested": {
      const elements = args.map(tupleElement);
      const inner = elements.map((e) => decoderOf(e.type, exact));
      const tuple: Decoder = inner.every((d) => d === same)
        ? same
        : (v, column) => {
            if (Array.isArray(v)) for (let i = 0; i < v.length && i < inner.length; i++) v[i] = inner[i]!(v[i], column);
            else if (v !== null && typeof v === "object") {
              // The value JSON.parse made holds each name as its own property, `__proto__` too.
              const o = v as Record<string, unknown>;
              elements.forEach((e, i) => {
                if (e.name !== undefined && inner[i] !== same && Object.hasOwn(o, e.name)) o[e.name] = inner[i]!(o[e.name], column);
              });
            }
            return v;
          };
      if (name === "Tuple" || tuple === same) return tuple;
      // A Nested column that is not flattened is an array of its rows.
      return (v, column) => {
        if (Array.isArray(v)) for (let i = 0; i < v.length; i++) v[i] = tuple(v[i], column);
        return v;
      };
    }
    case "Map": {
      if (args.length !== 2) return same;
      const inner = decoderOf(args[1]!, exact);
      if (inner === same) return same;
      return (v, column) => {
        if (v !== null && typeof v === "object" && !Array.isArray(v)) {
          const o = v as Record<string, unknown>;
          for (const k of Object.keys(o)) o[k] = inner(o[k], column);
        }
        return v;
      };
    }
    default:
      return same;
  }
}
