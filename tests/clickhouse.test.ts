import { after, before, test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { Session, type Outcome } from "../src/analyze.ts";
import { sqlModule } from "../src/loader.ts";
import { analyze as describe, defineQuery } from "../src/index.ts";
import { leadingComment } from "../src/doc-comment.ts";
import { nativeBinary } from "../src/native.ts";
import { clickhouseParam } from "../src/decode-clickhouse.ts";
import { dialectOf } from "../src/project.ts";
import type { EngineType } from "../src/engine-type.ts";
import { clickhouseDatabase, clickhouseLive as skip, closeSessions, declarations, INDEX, plain } from "./helpers.ts";

// ClickHouse is typed over its HTTP interface, which the engine speaks itself, and run through
// @clickhouse/client, the labs' (lab/drivers).

const SCHEMA = [
  `create table events (
    id UInt64, name String, tag Nullable(String), level Enum8('info' = 1, 'warn' = 2),
    at DateTime64(3, 'UTC'), seen DateTime('Europe/Rome'), day Date, tags Array(String), scores Array(Nullable(Int32)),
    props Map(String, Nullable(Int64)), lc LowCardinality(String), amount Decimal(10, 2),
    pair Tuple(a Int32, b Nullable(String)), uid UUID, ip IPv4, ok Bool, ratio Float64, big Int128,
    doc JSON, fixed FixedString(3) comment 'Three bytes'
  ) engine = MergeTree order by id`,
  `create table teams (id UInt32, name String) engine = MergeTree order by id`,
];

let db: Awaited<ReturnType<typeof clickhouseDatabase>>;
let root: string;

before(async () => {
  if (skip) return;
  db = await clickhouseDatabase("reylo_clickhouse", SCHEMA);
  root = mkdtempSync(join(tmpdir(), "reylo-clickhouse-"));
  writeFileSync(join(root, "package.json"), "{}");
  writeFileSync(join(root, "tsconfig.json"), "{}");
  writeFileSync(join(root, ".env"), `DATABASE_URL=${db.url}\n`);
});

after(async () => {
  if (skip) return;
  await closeSessions();
  await db.drop();
});

type Analysis = Extract<Outcome, { ok: true }>["analysis"];

async function analyze(sql: string): Promise<Analysis> {
  const session = await Session.for(root);
  const outcome = await session.analyze(sql, { store: false });
  assert.ok(outcome.ok, !outcome.ok ? outcome.message : "");
  return outcome.analysis;
}

/** A type as ClickHouse names it: an array its element's in brackets, a Tuple its elements, a Map its key and value, an Enum its labels. */
function shown(t: EngineType): string {
  if (t.kind === "array") return `Array(${shown(t.element.type)}${t.element.nullable ? "?" : ""})`;
  if (t.kind === "record") return `Tuple(${t.fields.map((f) => `${f.name} ${shown(f.type)}${f.nullable ? "?" : ""}`).join(", ")})`;
  if (t.kind === "map") return `Map(${shown(t.key)}, ${shown(t.value.type)}${t.value.nullable ? "?" : ""})`;
  if (t.kind === "text" && t.enum !== undefined) return `Enum(${t.enum.join(", ")})`;
  return "label" in t && t.label !== undefined ? t.label : t.kind;
}

/** Each column, or parameter, as `name: type`, with `?` where it may be NULL. */
const listed = (items: readonly { name: string; type: EngineType; nullable?: boolean }[]) => items.map((c) => `${c.name}: ${shown(c.type)}${c.nullable ? "?" : ""}`);

/** The text a diagnostic stands at. */
const at = (sql: string, d: { start: number; length: number }) => sql.slice(d.start, d.start + d.length);

/** The module a build step writes for `raw`, imported. */
async function built(name: string, raw: string) {
  const file = join(root, `${name}.sql`);
  writeFileSync(file, raw);
  const { code } = await sqlModule(file, raw);
  const out = join(root, `${name}.mjs`);
  writeFileSync(out, code.replace(`from "reylo"`, `from ${JSON.stringify(INDEX)}`));
  return (await import(pathToFileURL(out).href)).default as { run(client: unknown, params?: unknown): Promise<any>; try(client: unknown, params?: unknown): Promise<any> };
}

test("ClickHouse: the URL's scheme names the database, over HTTP or HTTPS", () => {
  for (const url of ["clickhouse://localhost:8123/app", "clickhouses://user:secret@host.example/app", "clickhouse+http://localhost/app", "clickhouse+https://host.example:8443/app"]) assert.equal(dialectOf(url), "clickhouse");
});

test("ClickHouse: each column is typed by the server's describe, and is NULL only where its type says so", { skip }, async () => {
  const a = await analyze("select * from events");
  assert.deepEqual(listed(a.columns), [
    "id: UInt64", "name: String", "tag: String?", "level: Enum(info, warn)", "at: DateTime64", "seen: DateTime", "day: Date", "tags: Array(String)", "scores: Array(Int32?)",
    "props: Map(String, Int64?)", "lc: String", "amount: Decimal", "pair: Tuple(a Int32, b String?)", "uid: UUID", "ip: IPv4", "ok: Bool", "ratio: Float64", "big: Int128", "doc: JSON", "fixed: FixedString",
  ]);
  assert.equal(a.kind, "select");
  assert.equal(a.cardinality, "many");
  assert.deepEqual(a.diagnostics, []);
  // An expression is typed as the server types it: a left join's columns are not NULL unless join_use_nulls says so.
  const joined = await analyze("select e.id as id, t.name as team, toNullable(e.id) as maybe, e.tags[1] as first, count() over () as n from events e left join teams t on t.id = e.id");
  assert.deepEqual(listed(joined.columns), ["id: UInt64", "team: String", "maybe: UInt64?", "first: String", "n: UInt64"]);
});

test("ClickHouse: a parameter states its type, as ClickHouse's own substitution or a cast after :name", { skip }, async () => {
  const a = await analyze("select id from events where id = :id::UInt64 and tag = {tag:Nullable(String)} and at > :since::DateTime64(3, 'UTC') and has({tags:Array(String)}, name)");
  assert.deepEqual(listed(a.params), ["id: UInt64", "tag: String?", "since: DateTime64", "tags: Array(String)"]);
  assert.equal(a.rewritten.sql, "select id from events where id = {p1:UInt64}::UInt64 and tag = {p2:Nullable(String)} and at > {p3:DateTime64(3, 'UTC')}::DateTime64(3, 'UTC') and has({p4:Array(String)}, name)");
  assert.deepEqual(a.rewritten.order, ["id", "tag", "since", "tags"]);
  // One that states none is refused where it stands, with both ways to state it.
  const sql = "select id from events where id = :id";
  const untyped = await analyze(sql);
  assert.equal(untyped.diagnostics.length, 1);
  assert.match(untyped.diagnostics[0]!.message, /each states its own type: cast :id, as in :id::UInt64, or write it as ClickHouse's own \{name:UInt64\}/);
  assert.equal(at(sql, untyped.diagnostics[0]!), ":id");
});

test("ClickHouse: a whole value of INSERT … VALUES takes its column's type", { skip }, async () => {
  const a = await analyze("insert into events (id, name, tag, tags, fixed) values (:id, :name, :tag, :tags, :fixed)");
  assert.deepEqual(listed(a.params), ["id: UInt64", "name: String", "tag: String?", "tags: Array(String)", "fixed: FixedString"]);
  assert.equal(a.rewritten.sql, "insert into events (id, name, tag, tags, fixed) values ({p1:UInt64}, {p2:String}, {p3:Nullable(String)}, {p4:Array(String)}, {p5:FixedString(3)})");
  assert.equal(a.kind, "insert");
  assert.equal(a.cardinality, "none");
  assert.deepEqual(a.columns, []);
  // The column's comment is the parameter's.
  assert.equal(a.params.at(-1)!.comment, "Three bytes");
  const missing = await analyze("insert into events (id, nope) values (:id, :x)");
  assert.match(missing.diagnostics[0]!.message, /The table events has no column nope/);
});

test("ClickHouse: how many rows a statement returns", { skip }, async () => {
  const cardinality = async (sql: string) => (await analyze(sql)).cardinality;
  assert.equal(await cardinality("select id from events"), "many");
  assert.equal(await cardinality("select id from events order by id limit 1"), "at_most_one");
  assert.equal(await cardinality("select count() as n, max(id) as top from events"), "one");
  assert.equal(await cardinality("select countIf(ok) as n from events where id > 3"), "one");
  assert.equal(await cardinality("select count() as n from events group by lc"), "many");
  assert.equal(await cardinality("select 1 as one"), "one");
  assert.equal(await cardinality("select count() as n from events limit 1 union all select 1"), "many");
  assert.equal(await cardinality("alter table events delete where id = :id::UInt64"), "none");
});

test("ClickHouse: an error the server raises is placed where it names", { skip }, async () => {
  const unknown = "select id, nope from events";
  const a = await analyze(unknown);
  assert.equal(a.diagnostics.length, 1);
  assert.equal(a.diagnostics[0]!.code, 6);
  assert.equal(at(unknown, a.diagnostics[0]!), "nope");
  const syntax = "select id frm events where id = {id:UInt64}";
  const b = await analyze(syntax);
  assert.match(b.diagnostics[0]!.message, /^Syntax error/);
  assert.equal(at(syntax, b.diagnostics[0]!), "events");
  // A statement whose rows the server does not describe is refused, and so is a FORMAT of its own.
  assert.match((await analyze("show tables")).diagnostics[0]!.message, /SHOW/i);
  assert.match((await analyze("select 1 as one format JSON")).diagnostics[0]!.message, /FORMAT/);
});

test("ClickHouse: the module declares the types .run() returns and takes", { skip }, async () => {
  const d = await declarations(root, {
    "all.sql": "select * from events where id = :id::UInt64",
    "params.sql": "select 1 as one where {a:Array(Nullable(String))} = [] and {m:Map(String, UInt8)} = map() and {p:Tuple(a Int32, b Nullable(String))}.a = 1 and {d:Decimal(10, 2)} > 0 and {big:Int128} > 0 and {at:Nullable(DateTime)} is null and {j:JSON}.a = 1 and {n:UInt8} = 1",
  });
  const all = d["all.sql"]!;
  for (const field of [
    "id: bigint", "name: string", "tag: string | null", 'level: "info" | "warn"', "at: string", "seen: string", "day: string", "tags: string[]", "scores: (number | null)[]",
    "props: Record<string, bigint | null>", "lc: string", "amount: string", "pair: { a: number; b: string | null }", "uid: string", "ip: string", "ok: boolean", "ratio: number", "big: bigint", "doc: Json", "fixed: string",
  ]) assert.ok(all.includes(field), `${field} in\n${all}`);
  assert.ok(all.includes("id: number | bigint | string"), all);
  const params = d["params.sql"]!;
  for (const field of [
    "a: readonly (string | null)[]", "m: Readonly<Record<string, number>>", "p: { a: number; b: string | null }", "d: string | number", "big: number | bigint | string", "at?: string | null", "j: JsonInput", "n: number",
  ]) assert.ok(params.includes(field), `${field} in\n${params}`);
});

const ROW = {
  id: 18446744073709551615n,
  name: "tab\there, 'quoted', back\\slash, new\nline, nul\0, ünïcode ✓",
  tag: null,
  level: "warn",
  at: "2026-10-20T10:00:00.123Z",
  seen: "2026-10-20T10:00:00Z",
  day: "2026-10-20",
  tags: ["a", "it's", "b\\c", "", "tab\t"],
  scores: [1, null, -2147483648],
  props: { a: 1n, "it's": null, big: -9223372036854775808n },
  lc: "low",
  amount: "-12345678.90",
  pair: { a: -7, b: null },
  uid: "0a1b2c3d-0000-4000-8000-00000000000f",
  ip: "10.1.2.3",
  ok: true,
  ratio: 0.1,
  big: -170141183460469231731687303715884105728n,
  doc: { a: { b: [1, "two", null] }, s: "it's \"q\" \\ \n" },
  fixed: "ab\0",
};

test("ClickHouse: .run() returns each type's one representation, and a row goes back in as it came out", { skip }, async () => {
  const columns = Object.keys(ROW);
  const insert = await built("insert", `insert into events (${columns.join(", ")}) values (${columns.map((c) => `:${c}`).join(", ")})`);
  const get = await built("get", "select * from events where id = :id::UInt64");
  // A statement that returns no rows runs as a command, and gives none.
  assert.equal(await insert.run(db.client, ROW), undefined);
  const [row] = await get.run(db.client, { id: ROW.id });
  assert.deepEqual({ ...row }, ROW);
  // What came out goes back in: the same row under another key.
  await insert.run(db.client, { ...row, id: 2 });
  const [again] = await get.run(db.client, { id: "2" });
  assert.deepEqual({ ...again }, { ...ROW, id: 2n });
  const remove = await built("remove", "alter table events delete where id = :id::UInt64 settings mutations_sync = 1");
  assert.equal(await remove.run(db.client, { id: 2n }), undefined);
  assert.deepEqual(await get.run(db.client, { id: 2 }), []);
});

test("ClickHouse: every type's edge values go in as text ClickHouse reads back exactly", { skip }, async () => {
  const echo = await built(
    "echo",
    `select {f:Array(Float64)} as f, {i:Array(Nullable(Int64))} as i, {u:UInt256} as u, {d:Array(Decimal(38, 10))} as d,
      {at:Array(DateTime64(9, 'UTC'))} as at, {early:DateTime64(3, 'UTC')} as early, {first:DateTime('UTC')} as first, {seen:Array(Nullable(DateTime('UTC')))} as seen,
      {m:Map(UInt64, Array(String))} as m, {n:Array(Tuple(a UInt8, b Nullable(String)))} as n, {s:Nullable(String)} as s, {e:Array(String)} as e,
      {t:Tuple(d Date, j JSON, u Nullable(UUID))} as t, {dates:Map(Date, Bool)} as dates, {j:Array(JSON)} as j`,
  );
  const input = {
    f: [NaN, Infinity, -Infinity, -0.5, 1e300, 5e-324],
    i: [9223372036854775807n, null, -9223372036854775808n],
    u: 115792089237316195423570985008687907853269984665640564039457584007913129639935n,
    d: ["-9999999999999999999999999999.9999999999", "0.0000000001"],
    at: ["2026-10-20T10:00:00.123456789Z", "1969-12-31T23:59:59.250000000Z", "1900-01-01T00:00:00.000000000Z"],
    early: "1969-12-31T23:59:58.500Z",
    first: "1970-01-01T00:00:07Z",
    seen: ["1970-01-01T00:00:07Z", null, "2106-02-07T06:28:15Z"],
    m: { "0": [], "18446744073709551615": ["x"] },
    n: [{ a: 1, b: "x" }, { a: 2, b: null }],
    s: "\\N",
    e: [],
    t: { d: "2026-10-20", j: { k: "it's" }, u: null },
    dates: { "2026-10-20": true },
    j: [{ a: 1 }, { b: "it's\n" }],
  };
  const row = await echo.run(db.client, input);
  assert.deepEqual({ ...row }, input);
  // An instant goes in from any offset, and a DateTime64 that stands alone from one before 1970.
  const offset = await echo.run(db.client, { ...input, early: "1970-01-01 01:59:58.5+02:00", first: "1970-01-01T09:00:07+09:00" });
  assert.equal(offset.early, "1969-12-31T23:59:58.500Z");
  assert.equal(offset.first, "1970-01-01T00:00:07Z");
});

test("ClickHouse: .run() refuses a value its parameter's type does not read, before the statement runs", { skip }, async () => {
  const q = await built("refuse", "select {n:UInt8} as n, {at:DateTime('UTC')} as at, {a:Array(String)} as a, {p:Tuple(a Int32, b String)} as p, {early:DateTime64(3)} as early, {day:Date} as day, {m:Map(Int8, Bool)} as m");
  const good = { n: 1, at: "2026-10-20T10:00:00Z", a: [], p: { a: 1, b: "" }, early: "2026-10-20T10:00:00Z", day: "2026-10-20", m: { "-128": true } };
  assert.deepEqual({ ...(await q.run(db.client, good)) }, { ...good, at: "2026-10-20T10:00:00Z", early: "2026-10-20T10:00:00.000Z" });
  const refused = async (change: Record<string, unknown>, message: RegExp) => assert.rejects(async () => q.run(db.client, { ...good, ...change }), (e: Error) => e instanceof TypeError && message.test(e.message), JSON.stringify(change));
  await refused({ n: 1.5 }, /"n" is UInt8, an integer, which takes no fraction/);
  // ClickHouse would read an integer past its type's range as another, and a date past its as the last it holds.
  await refused({ n: 300 }, /"n" is UInt8, which holds 0 to 255; got 300/);
  await refused({ n: "-1" }, /"n" is UInt8, which holds 0 to 255/);
  await refused({ m: { 128: true } }, /"m" at \["128"\] is Int8, which holds -128 to 127/);
  await refused({ day: "2200-01-01" }, /"day" is Date, which holds 1970-01-01 to 2149-06-06/);
  await refused({ n: null }, /"n" is null, and the statement takes no null for it/);
  await refused({ at: new Date() }, /not a Date/);
  await refused({ at: "2026-10-20 10:00:00" }, /names its zone/);
  await refused({ at: "2026-10-20T10:00:00.5Z" }, /a DateTime, which holds whole seconds/);
  await refused({ a: ["x", null] }, /"a" at \[1\] is null, and its type, String, takes no NULL there/);
  await refused({ p: { a: 1 } }, /"p" at \.b is missing/);
  await refused({ early: "1969-12-31T23:59:59.5Z" }, /no instant within the second before 1970/);
  assert.equal(clickhouseParam(undefined, "a\tb", "x"), "a\\tb");
  assert.equal(clickhouseParam(undefined, null, "x"), "\\N");
  assert.equal(clickhouseParam(undefined, ["a'b", 1, null, [2n, true]], "x"), "['a\\'b',1,NULL,[2,true]]");
});

test("ClickHouse: .try() reads the server's error, its name and its number", { skip }, async () => {
  const q = await built("fails", "select throwIf({n:UInt8} = 1, 'no ones') as x");
  assert.deepEqual(plain([(await q.try(db.client, { n: 0 })).value]), [{ x: 0 }]);
  const result = await q.try(db.client, { n: 1 });
  assert.equal(result.error.name, "QueryError");
  assert.equal(result.error.code, "FUNCTION_THROW_IF_VALUE_IS_NON_ZERO");
  assert.equal(result.error.errno, 395);
  assert.match(result.error.message, /no ones/);
});

test("ClickHouse: a client is run only on its own database's statements", { skip }, async () => {
  const pg = defineQuery<Record<never, never>, { one: number }, "one", "postgres">({ dialect: "postgres", sql: "select 1 as one", params: [], cardinality: "one" });
  await assert.rejects(async () => pg.run(db.client as never), /It is a ClickHouse client, and the statement is typed for postgres/);
  const q = await built("one", "select 1 as one");
  await assert.rejects(async () => q.run({ query: async () => ({ rows: [] }) }), /reylo cannot run a clickhouse statement on an object with query\. It takes a client with query\(\{ query, format \}\) and command\(\{ query \}\)/);
});

test("ClickHouse: a column named __proto__ is an own property of the row, and a Map's key of that name of its object", { skip }, async () => {
  const q = await built("proto", `select 1 as "__proto__", map('__proto__', toInt64(2)) as m, tuple(toInt64(3)) as t`);
  const row = await q.run(db.client);
  assert.equal(Object.getPrototypeOf(row), Object.prototype);
  assert.deepEqual(Object.entries(row), [["__proto__", 1], ["m", row.m], ["t", [3n]]]);
  assert.deepEqual(Object.entries(row.m), [["__proto__", 2n]]);
});

test("ClickHouse: a Standard Query's decode of a JSON format's values is .run()'s result", { skip }, async () => {
  const q = await built("standard", "select id, ratio, tags, scores, amount, at, big, doc from events where id = :id::UInt64");
  const [row] = await q.run(db.client, { id: ROW.id });
  const raw = { id: "18446744073709551615", ratio: 0.1, tags: ROW.tags, scores: [1, null, -2147483648], amount: "-12345678.90", at: ROW.at, big: "-170141183460469231731687303715884105728", doc: ROW.doc };
  const { columns } = (q as unknown as { "~standard": { query: { columns: { name: string; decode(v: unknown): unknown }[] } } })["~standard"].query;
  assert.deepEqual(columns.map((c) => [c.name, c.decode(raw[c.name as keyof typeof raw])]), Object.entries(row));
});

test("ClickHouse: the schema's fingerprint moves with a column's type", { skip }, async () => {
  const session = await Session.for(root);
  await db.client.command({ query: "create table moving (id UInt8, a UInt8) engine = MergeTree order by id" });
  assert.equal(await session.recheck(), true);
  assert.equal(await session.recheck(), false);
  await db.client.command({ query: "alter table moving modify column a UInt16" });
  assert.equal(await session.recheck(), true);
  const a = await analyze("select a from moving");
  assert.deepEqual(listed(a.columns), ["a: UInt16"]);
});

test("ClickHouse: a query's JSON Schema describes a Map as an object and an array's elements as its type says", { skip }, async () => {
  const q = await built("schema", "select props, scores, pair, amount, big from events where has({ids:Array(Nullable(UInt64))}, id) and amount > {least:Decimal(10, 2)} and props = {props:Map(String, Nullable(Int64))}");
  const { jsonSchema } = (q as unknown as { "~standard": { jsonSchema: { input(): { properties: Record<string, unknown> }; output(): { items: { properties: Record<string, unknown> } } } } })["~standard"];
  const wide = { anyOf: [{ type: ["integer", "string"], format: "int64" }, { type: "null" }] };
  assert.deepEqual(jsonSchema.input().properties, {
    ids: { type: "array", items: wide },
    least: { type: ["string", "number"], format: "decimal" },
    props: { type: "object", additionalProperties: wide },
  });
  assert.deepEqual(jsonSchema.output().items.properties, {
    props: { type: "object", additionalProperties: { anyOf: [{ type: "integer", format: "int64" }, { type: "null" }] } },
    scores: { type: "array", items: { anyOf: [{ type: "integer", format: "int32" }, { type: "null" }] } },
    pair: { type: "object", properties: { a: { type: "integer", format: "int32" }, b: { type: ["string", "null"] } }, required: ["a", "b"] },
    amount: { type: "string", format: "decimal" },
    big: { type: "integer", format: "int64" },
  });
});

test("ClickHouse: analyze() gives a statement's descriptor, as reylo analyze prints it", { skip }, async () => {
  const sql = "-- An event's tags.\n# Its second line.\nselect id, tags from events where id = {id:UInt64} limit 1";
  const descriptor = await describe({ url: db.url, sql });
  assert.equal(descriptor.dialect.name, "clickhouse");
  assert.match(descriptor.dialect.version ?? "", /^\d+\.\d+/);
  assert.equal(descriptor.sql, "-- An event's tags.\n# Its second line.\nselect id, tags from events where id = {p1:UInt64} limit 1");
  assert.deepEqual(descriptor.params, [{ name: "id", type: { kind: "integer", bits: 64, unsigned: true, label: "UInt64" } }]);
  assert.equal(descriptor.result.kind === "rows" && descriptor.result.cardinality, "at_most_one");
  const run = spawnSync(nativeBinary(), ["analyze", "--url", db.url, "--sql", sql], { cwd: root, encoding: "utf8" });
  const { file: _, ...printed } = JSON.parse(run.stdout.trim().split("\n")[0]!);
  assert.deepEqual(descriptor, printed);
  // The comment that opens the statement is its documentation, a `# ` line of it too.
  assert.deepEqual(leadingComment(sql, "clickhouse").prose, "An event's tags.\nIts second line.");
});
