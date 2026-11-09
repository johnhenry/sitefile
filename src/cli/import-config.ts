import { readFile } from 'node:fs/promises';
import { SitefileError } from '../errors.js';
import { formatOf, headersText, HOST_FORMATS, importHostConfig, redirectsText, type HostFormat } from '../host-config.js';
import { requireArgs, str, usage, type Command } from './common.js';

const importConfig: Command = {
  name: 'import-config',
  summary: "translate a host's config file into _redirects and _headers",
  usage: 'import-config <file> [--format f] [--redirects | --headers] [--json]',
  description:
    "Read netlify.toml, vercel.json, firebase.json, staticwebapp.config.json, wrangler.toml / wrangler.json\n([assets]) or .htaccess, and print the _redirects and _headers that sitefile serve understands today.\nEvery rule printed is one serve applies as written; anything else (placeholders, conditions, proxying,\nregular expressions, settings with no _redirects form) is listed on stderr as a warning, never half\ntranslated. The format comes from the file name unless --format is given.",
  flags: [
    `--format <f>    ${HOST_FORMATS.join(', ')}`,
    '--redirects     print only the _redirects text',
    '--headers       print only the _headers text',
    '--json          machine-readable output (rules and unsupported entries)',
  ],
  options: { format: { type: 'string' }, redirects: { type: 'boolean' }, headers: { type: 'boolean' }, json: { type: 'boolean' } },
  async run(v, pos) {
    requireArgs(pos, 1, 1, 'a host config file');
    const file = pos[0] as string;
    const given = str(v.format);
    if (given !== undefined && !(HOST_FORMATS as readonly string[]).includes(given)) usage(`--format expects one of ${HOST_FORMATS.join(', ')}, got '${given}'`);
    const format = (given as HostFormat | undefined) ?? formatOf(file);
    if (format === undefined) usage(`cannot tell the format of '${file}' from its name; pass --format (${HOST_FORMATS.join(', ')})`);
    if (v.redirects && v.headers) usage('--redirects and --headers conflict');
    if (v.json && (v.redirects || v.headers)) usage('--json prints both; drop --redirects/--headers');
    let text: string;
    try {
      text = await readFile(file, 'utf8');
    } catch (e) {
      throw new SitefileError('E_NOT_FOUND', `cannot read '${file}'`, undefined, { cause: e });
    }
    const c = importHostConfig(text, file, format);
    if (v.json) {
      process.stdout.write(
        JSON.stringify(
          { format: c.format, redirects: c.redirects, headers: c.headers.map((b) => ({ pattern: b.pattern, headers: b.headers })), unsupported: c.unsupported },
          null,
          2,
        ) + '\n',
      );
      return 0;
    }
    for (const u of c.unsupported) process.stderr.write(`warning: ${file}: ${u.entry}: ${u.reason}\n`);
    const r = redirectsText(c);
    const h = headersText(c);
    if (v.redirects) process.stdout.write(r);
    else if (v.headers) process.stdout.write(h);
    else process.stdout.write(`${r ? `# _redirects\n${r}` : ''}${r && h ? '\n' : ''}${h ? `# _headers\n${h}` : ''}`);
    return 0;
  },
};

export const importConfigCommands: ReadonlyMap<string, Command> = new Map([['import-config', importConfig]]);
