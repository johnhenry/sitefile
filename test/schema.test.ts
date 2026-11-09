import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { build, type BuildOptions } from '../src/executor.js';
import { isSitefileError, SitefileError, type ErrorCode } from '../src/errors.js';
import { parseProgram } from '../src/language/validate.js';
import { plan } from '../src/planner.js';
import { checkSchema, compileSchema } from '../src/schema.js';
import { planGraph, graphText } from '../src/explain.js';

const temps: string[] = [];
after(async () => {
  for (const d of temps) await rm(d, { recursive: true, force: true });
});

async function project(files: Record<string, string>): Promise<string> {
  const dir = await mkdtemp(path.join(os.tmpdir(), 'sitefile-schema-'));
  temps.push(dir);
  for (const [rel, content] of Object.entries(files)) {
    const p = path.join(dir, rel);
    await mkdir(path.dirname(p), { recursive: true });
    await writeFile(p, content);
  }
  return dir;
}
async function run(files: Record<string, string>, extra: Partial<BuildOptions> = {}) {
  return build({ contextDir: await project(files), lockfile: false, ...extra });
}
async function rejects(files: Record<string, string>, code: ErrorCode, message: string, line?: number): Promise<void> {
  await assert.rejects(run(files), (e: unknown) => {
    assert.ok(isSitefileError(e), String(e));
    assert.equal(e.code, code, e.message);
    assert.equal(e.detail, message);
    if (line !== undefined) assert.equal(e.span?.line, line);
    return true;
  });
}

const SCHEMA = 'required: [title, date]\nproperties:\n  title: string\n  date: date\n  tags: {type: list, items: string}\n  status: {enum: [draft, published]}\n';
const post = (front: string): string => `---\n${front}---\nBody.\n`;
const site = (sitefile: string, posts: Record<string, string>, schema = SCHEMA): Record<string, string> => ({
  'post.schema.yaml': schema,
  ...Object.fromEntries(Object.entries(posts).map(([k, v]) => [`posts/${k}`, v])),
  Sitefile: sitefile,
});
const MD = 'IMPORT posts FROM ./posts AS markdown SCHEMA ./post.schema.yaml\nRENDER <<T TO /i.html\n${#each posts as p}${p.title};${/each}\nT\nEXPORT /';

test('SCHEMA: matching markdown documents build; the file-name date satisfies a required date', async () => {
  const r = await run(site(MD, { '2026-01-05-a.md': post('title: A\ntags: [x, y]\nstatus: draft\n'), 'b.md': post('title: B\ndate: 2026-02-01T10:00:00Z\n') }));
  assert.equal(new TextDecoder().decode(r.output.get('/i.html')?.bytes), 'A;B;\n');
});

test('SCHEMA: mismatches name the document, the key, the expected type and the value', async () => {
  await rejects(site(MD, { 'a.md': post('date: 2026-01-01\n') }), 'E_SCHEMA', "IMPORT posts: document '/a.md' has no 'title' (required by schema './post.schema.yaml')", 1);
  await rejects(
    site(MD, { 'a.md': post('title: A\ndate: soon\n') }),
    'E_SCHEMA',
    "IMPORT posts: document '/a.md' has 'date' string 'soon'; it should be a date (YYYY-MM-DD or an RFC 3339 date-time)",
  );
  await rejects(site(MD, { 'a.md': post('title: 3\ndate: 2026-01-01\n') }), 'E_SCHEMA', "IMPORT posts: document '/a.md' has 'title' number 3; it should be a string");
  await rejects(
    site(MD, { 'a.md': post('title: A\ndate: 2026-01-01\ntags: [x, 2]\n') }),
    'E_SCHEMA',
    "IMPORT posts: document '/a.md' has 'tags' item 2 number 2; it should be a string",
  );
  await rejects(
    site(MD, { 'a.md': post('title: A\ndate: 2026-01-01\nstatus: live\n') }),
    'E_SCHEMA',
    "IMPORT posts: document '/a.md' has 'status' string 'live'; it should be one of draft, published",
  );
  // A null value counts as missing.
  await rejects(site(MD, { 'a.md': post('title:\ndate: 2026-01-01\n') }), 'E_SCHEMA', "IMPORT posts: document '/a.md' has no 'title' (required by schema './post.schema.yaml')");
  // Documents are checked in path order, before SORT BY.
  await rejects(
    site(MD.replace('SCHEMA', 'SORT BY title DESC SCHEMA'), { 'a.md': post('title: A\n'), 'b.md': post('title: B\n') }),
    'E_SCHEMA',
    "IMPORT posts: document '/a.md' has no 'date' (required by schema './post.schema.yaml')",
  );
});

test('SCHEMA: JSON schemas, yaml/json lists and CSV rows', async () => {
  const schema = JSON.stringify({ required: ['name'], properties: { age: 'number', admin: 'boolean', role: { enum: ['a', 2] } } });
  const ok = await run({
    's.json': schema,
    'p.csv': 'name,age,admin,role\nAl, 10 ,true,2\nBo,,false,a\n',
    'p.yaml': '- name: Al\n  age: 10\n- name: Bo\n  role: 2\n',
    Sitefile: 'IMPORT c FROM ./p.csv AS csv SCHEMA ./s.json\nIMPORT y FROM ./p.yaml AS yaml SCHEMA ./s.json\nRENDER <<T TO /i.html\n${#each c as p}${p.name}${/each}${#each y as p}${p.name}${/each}\nT\nEXPORT /',
  });
  assert.equal(new TextDecoder().decode(ok.output.get('/i.html')?.bytes), 'AlBoAlBo\n');
  await rejects({ 's.json': schema, 'p.csv': 'name,age\nAl,ten\n', Sitefile: 'IMPORT c FROM ./p.csv AS csv SCHEMA ./s.json\nEXPORT /' }, 'E_SCHEMA', "IMPORT c: document '#1' has 'age' string 'ten'; it should be a number");
  await rejects({ 's.json': schema, 'p.csv': 'name,age\n,1\n', Sitefile: 'IMPORT c FROM ./p.csv AS csv SCHEMA ./s.json\nEXPORT /' }, 'E_SCHEMA', "IMPORT c: document '#1' has no 'name' (required by schema './s.json')");
  await rejects({ 's.json': schema, 'p.yaml': '- name: A\n  age: "10"\n', Sitefile: 'IMPORT y FROM ./p.yaml AS yaml SCHEMA ./s.json\nEXPORT /' }, 'E_SCHEMA', "IMPORT y: document '#1' has 'age' string '10'; it should be a number");
  await rejects({ 's.json': schema, 'p.json': '[{"name": "A"}, 3]', Sitefile: 'IMPORT y FROM ./p.json AS json SCHEMA ./s.json\nEXPORT /' }, 'E_TYPE', 'IMPORT y: SCHEMA checks documents (objects), but element 2 is number 3');
  // A directory of data files: documents are named by their path.
  await rejects({ 's.json': schema, 'd/x.yaml': 'age: 1\n', Sitefile: 'IMPORT d FROM ./d AS yaml SCHEMA ./s.json\nEXPORT /' }, 'E_SCHEMA', "IMPORT d: document '/x.yaml' has no 'name' (required by schema './s.json')");
});

test('SCHEMA: invalid schema files and clauses', async () => {
  const one = (schema: string): Record<string, string> => site(MD, { 'a.md': post('title: A\ndate: 2026-01-01\n') }, schema);
  await rejects(one('- a\n'), 'E_SCHEMA', "IMPORT posts: schema './post.schema.yaml' must be a mapping (with required and/or properties)", 1);
  await rejects(one('require: [a]\n'), 'E_SCHEMA', "IMPORT posts: schema './post.schema.yaml' has an unknown key 'require' (expected required or properties)");
  await rejects(one('properties:\n  a: str\n'), 'E_SCHEMA', "IMPORT posts: schema './post.schema.yaml' property 'a': unknown type 'str' (expected string, number, boolean, date or list)");
  await rejects(one('properties:\n  a: {items: string}\n'), 'E_SCHEMA', "IMPORT posts: schema './post.schema.yaml' property 'a': items needs type list");
  await rejects(one('properties:\n  a: {enum: [[1]]}\n'), 'E_SCHEMA', "IMPORT posts: schema './post.schema.yaml' property 'a': enum must be a non-empty list of strings, numbers or booleans");
  await rejects(one('a: [unclosed\n'), 'E_DATA', "unterminated list: missing ']'");
  await rejects({ Sitefile: 'IMPORT p FROM ./p.json AS json SCHEMA ./missing.yaml\nEXPORT /', 'p.json': '[]' }, 'E_NOT_FOUND', "'./missing.yaml' does not exist in the build context");
  assert.throws(() => parseProgram('IMPORT p FROM ./p AS tree SCHEMA ./s.yaml\nEXPORT /'), (e: unknown) => isSitefileError(e, 'E_VALIDATION') && e.detail === 'IMPORT p: SCHEMA applies to documents and lists: AS markdown, json, yaml or csv' && e.span?.column === 27);
  assert.throws(() => parseProgram('IMPORT p FROM ./p AS json SCHEMA ./a.yaml SCHEMA ./b.yaml\nEXPORT /'), (e: unknown) => isSitefileError(e, 'E_SYNTAX') && e.detail === 'SCHEMA given twice');
  assert.throws(() => plan(parseProgram('IMPORT p FROM ./p AS json SCHEMA ./s.txt\nEXPORT /')), (e: unknown) => isSitefileError(e, 'E_VALIDATION') && e.detail === "IMPORT p: SCHEMA './s.txt' must be a .json, .yaml or .yml file");
  assert.throws(() => plan(parseProgram('IMPORT p FROM ./p AS json SCHEMA https://e.x/s.json\nEXPORT /')), (e: unknown) => isSitefileError(e, 'E_PROTOCOL'));
  assert.throws(() => plan(parseProgram('IMPORT p FROM ./p AS json SCHEMA ../s.json\nEXPORT /')), (e: unknown) => isSitefileError(e, 'E_PATH_ESCAPE'));
  assert.throws(() => parseProgram('IMPORT p FROM ./p AS json SCHEMA ./${S}.json\nEXPORT /'), (e: unknown) => isSitefileError(e, 'E_UNDEFINED'));
  const p = plan(parseProgram('ARG S = "post"\nIMPORT p FROM ./p AS json SCHEMA ./schemas/${S}.yaml\nEXPORT /'));
  assert.deepEqual(p.imports[0]!.schema, { uri: './schemas/post.yaml', path: 'schemas/post.yaml', span: { file: 'Sitefile', line: 2, column: 27 } });
  // graph: the schema file is an input of the import.
  assert.match(graphText(planGraph(p)), /local:\.\/schemas\/post\.yaml {2}\.\/schemas\/post\.yaml\n/);
  assert.ok(planGraph(p).edges.some((e) => e.from === 'local:./schemas/post.yaml' && e.to === 'import:p' && e.kind === 'input'));
});

test('compileSchema and checkSchema directly', () => {
  const span = { file: 'Sitefile', line: 1, column: 1 };
  const s = compileSchema({ properties: { n: 'number', tags: { type: 'list', enum: ['a'] } } }, 's.yaml', 'IMPORT x', span);
  assert.doesNotThrow(() => checkSchema([{ n: 1, tags: ['a', 'a'] }, { n: null }], s, 'IMPORT x', false, span));
  assert.throws(() => checkSchema({ tags: ['b'] }, s, 'IMPORT x', false, span), (e: unknown) => e instanceof SitefileError && e.detail === "IMPORT x: document '#1' has 'tags' item 1 string 'b'; it should be one of a");
  assert.throws(() => checkSchema([null], s, 'IMPORT x', false, span), (e: unknown) => e instanceof SitefileError && e.detail === 'IMPORT x: SCHEMA checks documents (objects), but element 1 is null');
});

test('SCHEMA in an imported Sitefile (AS sitefile) is read from its own context', async () => {
  const files = {
    'post.schema.yaml': 'required: [missing]\n',
    'blog/post.schema.yaml': 'required: [title]\n',
    'blog/posts/a.md': post('title: A\n'),
    'blog/Sitefile': 'STAGE pages\nIMPORT posts FROM ./posts AS markdown SCHEMA ./post.schema.yaml\nRENDER <<T TO /i.html\n${#each posts as p}${p.title}${/each}\nT\nEXPORT /',
    Sitefile: 'IMPORT blog FROM ./blog AS sitefile\nFROM scratch\nCOPY --from=blog+pages / /\nEXPORT /',
  };
  const r = await run(files);
  assert.equal(new TextDecoder().decode(r.output.get('/i.html')?.bytes), 'A\n');
  await rejects({ ...files, 'blog/posts/a.md': post('date: 2026-01-01\n') }, 'E_SCHEMA', "IMPORT posts: document '/a.md' has no 'title' (required by schema './post.schema.yaml')");
  assert.throws(() => parseProgram('IMPORT t FROM ./t AS sitefile SCHEMA ./s.yaml\nEXPORT /'), (e: unknown) => isSitefileError(e, 'E_VALIDATION'));
});

test('SCHEMA pattern: strings and list items must match; invalid patterns are E_SCHEMA', async () => {
  const schema = "properties:\n  handle: {pattern: '^[a-z0-9-]+$'}\n  tags: {type: list, items: string, pattern: '^\\w+$'}\n  code: {type: string, enum: [ab1, cd], pattern: '\\d'}\n";
  const ok = await run(site(MD, { 'a.md': post('title: A\nhandle: hello-world\ntags: [x, y_2]\ncode: ab1\n'), 'b.md': post('title: B\n') }, schema));
  assert.equal(new TextDecoder().decode(ok.output.get('/i.html')?.bytes), 'A;B;\n');
  await rejects(site(MD, { 'a.md': post('handle: Hello World\n') }, schema), 'E_SCHEMA', "IMPORT posts: document '/a.md' has 'handle' string 'Hello World'; it should match the pattern '^[a-z0-9-]+$'");
  await rejects(site(MD, { 'a.md': post('handle: 12\n') }, schema), 'E_SCHEMA', "IMPORT posts: document '/a.md' has 'handle' number 12; it should be a string matching the pattern '^[a-z0-9-]+$'");
  await rejects(site(MD, { 'a.md': post('tags: [ok, not ok]\n') }, schema), 'E_SCHEMA', "IMPORT posts: document '/a.md' has 'tags' item 2 string 'not ok'; it should match the pattern '^\\w+$'");
  // enum is checked before pattern.
  await rejects(site(MD, { 'a.md': post('code: cd\n') }, schema), 'E_SCHEMA', "IMPORT posts: document '/a.md' has 'code' string 'cd'; it should match the pattern '\\d'");
  await rejects(site(MD, { 'a.md': post('code: x1\n') }, schema), 'E_SCHEMA', "IMPORT posts: document '/a.md' has 'code' string 'x1'; it should be one of ab1, cd");
  // CSV cells are strings.
  await rejects({ 's.json': JSON.stringify({ properties: { zip: { pattern: '^\\d{5}$' } } }), 'p.csv': 'zip\n12345\n1234\n', Sitefile: 'IMPORT c FROM ./p.csv AS csv SCHEMA ./s.json\nEXPORT /' }, 'E_SCHEMA', "IMPORT c: document '#2' has 'zip' string '1234'; it should match the pattern '^\\d{5}$'");
  const one = (s: string): Record<string, string> => site(MD, { 'a.md': post('title: A\n') }, s);
  await rejects(one("properties:\n  a: {pattern: '[a-'}\n"), 'E_SCHEMA', "IMPORT posts: schema './post.schema.yaml' property 'a': pattern '[a-' is invalid at character 1: unterminated character class");
  await rejects(one("properties:\n  a: {pattern: 'a(?=b)'}\n"), 'E_SCHEMA', "IMPORT posts: schema './post.schema.yaml' property 'a': pattern 'a(?=b)' is invalid at character 2: unsupported group '(?' (only ( … ) and (?: … ) are supported)");
  await rejects(one('properties:\n  a: {pattern: [x]}\n'), 'E_SCHEMA', "IMPORT posts: schema './post.schema.yaml' property 'a': pattern must be a string (a regular expression)");
  await rejects(one('properties:\n  a: {min: 1}\n'), 'E_SCHEMA', "IMPORT posts: schema './post.schema.yaml' property 'a': unknown key 'min' (expected type, items, enum or pattern)");
});
