// Thin owner bridge. Browser, CDP and dedicated-profile lifetime belong upstream.
import { request } from 'node:https';
import { spawn } from 'node:child_process';
import { parseArgs } from 'node:util';
import { pathToFileURL } from 'node:url';
import { chmod, mkdir, readFile } from 'node:fs/promises';
import path from 'node:path';

export const ORIGIN = 'https://www.overleaf.com';
export const OPERATIONS = ['list_comments', 'read_comment_thread', 'add_comment', 'reply_comment',
  'read_file', 'list_tracked_changes', 'find_and_replace'];
class Expired extends Error {}
function require(ok, message) { if (!ok) throw new Error(message); }

export function selectCookie(header) {
  const selected = new Map();
  for (const pair of header.split(';')) {
    const at = pair.indexOf('=');
    const name = pair.slice(0, at).trim();
    if (!['overleaf_session2', 'GCLB'].includes(name)) continue;
    const value = pair.slice(at + 1).trim();
    require(at > 0 && !selected.has(name) && value.length <= 4096 &&
      /^[\x21-\x7e]+$/.test(value) && !/[;"\\,]/.test(value), 'Invalid Overleaf session cookie');
    selected.set(name, value);
  }
  require(selected.has('overleaf_session2'), 'Overleaf session cookie missing');
  return [...selected].map(([k, v]) => `${k}=${v}`).join('; ');
}

function decode(value) {
  return value.replace(/&(#x[0-9a-f]+|#\d+|quot|apos|amp|lt|gt);/gi, (_, entity) => {
    if (entity[0] === '#') return String.fromCodePoint(entity[1].toLowerCase() === 'x' ?
      parseInt(entity.slice(2), 16) : Number(entity.slice(1)));
    return { quot: '"', apos: "'", amp: '&', lt: '<', gt: '>' }[entity.toLowerCase()];
  });
}

export function envelope(body, cookie, expectedEmail, project, expiresAt) {
  const metadata = new Map();
  for (const tag of body.matchAll(/<meta\b(?:[^>"']|"[^"]*"|'[^']*')*>/gi)) {
    const attrs = Object.fromEntries([...tag[0].matchAll(/([\w-]+)\s*=\s*(["'])(.*?)\2/gs)]
      .map(m => [m[1].toLowerCase(), decode(m[3])]));
    if (!['ol-user', 'ol-project_id'].includes(attrs.name)) continue;
    require(!metadata.has(attrs.name), 'Ambiguous account/project metadata');
    metadata.set(attrs.name, attrs.content);
  }
  require(metadata.get('ol-project_id') === project, 'Project verification failed');
  const user = JSON.parse(metadata.get('ol-user') || '{}');
  const account = user._id ?? user.id;
  require(/^[0-9a-f]{24}$/.test(account || '') && user.email?.toLowerCase() === expectedEmail,
    'Expected Overleaf account verification failed; no session imported');
  return { schema: 2, cookie, account_id: account, expected_email: expectedEmail,
    projects: [project], operations: OPERATIONS, ...(expiresAt === undefined ? {} : { expires_at: expiresAt }) };
}

function fetchProject(cookie, project) {
  // Fixed HTTPS GET, system certificate verification, no proxy or redirect handling.
  return new Promise((resolve, reject) => {
    const req = request(`${ORIGIN}/project/${project}`, {
      method: 'GET', headers: { Cookie: cookie, Accept: 'text/html' }, timeout: 15000,
    }, res => {
      if (res.statusCode === 401 || res.statusCode === 302 && /^\/login(?:[/?]|$)/.test(res.headers.location || '')) {
        res.resume(); reject(new Expired('Saved Overleaf session expired')); return;
      }
      if (res.statusCode !== 200 || !/^text\/html(?:;|$)/i.test(res.headers['content-type'] || '')) {
        res.resume(); reject(new Error('Overleaf project check failed; no automatic login on access/server errors')); return;
      }
      const chunks = []; let size = 0;
      res.on('data', chunk => {
        size += chunk.length;
        if (size > 1024 * 1024) req.destroy(new Error('Project metadata response too large'));
        else chunks.push(chunk);
      });
      res.on('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
      res.on('error', reject);
    });
    req.on('timeout', () => req.destroy(new Error('Project check timed out')));
    req.on('error', reject);
    req.end();
  });
}

export async function acquire(expectedEmail, project, expiresAt, upstream, readProject = fetchProject) {
  const saved = await upstream.loadStored(ORIGIN);
  if (saved?.cookie) {
    const cookie = selectCookie(saved.cookie);
    try { return envelope(await readProject(cookie, project), cookie, expectedEmail, project, expiresAt); }
    catch (error) { if (!(error instanceof Expired)) throw error; }
  }
  // Upstream owns the ordinary dedicated Chrome login; human enters credentials/MFA.
  const cookie = selectCookie(await upstream.captureCookie(ORIGIN));
  const verified = envelope(await readProject(cookie, project), cookie, expectedEmail, project, expiresAt);
  await upstream.saveStored(ORIGIN, cookie);
  return verified;
}

async function control(executable, args, input = '') {
  await new Promise((resolve, reject) => {
    const child = spawn(executable, args, { stdio: ['pipe', 'ignore', 'ignore'] });
    child.on('error', () => reject(new Error('Fridica owner control unavailable')));
    child.on('close', code => code === 0 ? resolve() : reject(new Error('Fridica owner control failed; verify feature build, store and configuration')));
    child.stdin.on('error', () => reject(new Error('Fridica owner control input failed')));
    child.stdin.end(input);
  });
}

export async function checkResources() {
  const pkg = JSON.parse(await readFile(new URL('./node_modules/@netique/overleaf-mcp/package.json', import.meta.url)));
  const lock = JSON.parse(await readFile(new URL('./node_modules/.package-lock.json', import.meta.url)));
  require(pkg.version === '0.4.0' && lock.packages['node_modules/@netique/overleaf-mcp'].resolved ===
    'git+https://github.com/happysky19/overleaf-mcp-native.git#e4ad4f97d5730a448e5a2c3aa60838ef2d1f8981', 'Pinned Overleaf dependency missing or mismatched');
  await import('./node_modules/@netique/overleaf-mcp/dist/auth/cookieStore.js');
  await import('./node_modules/@netique/overleaf-mcp/dist/auth/browserLogin.js');
}

async function main() {
  const { values: o } = parseArgs({ options: {
    'expected-email': { type: 'string' }, fridica: { type: 'string' }, config: { type: 'string' },
    project: { type: 'string' }, browser: { type: 'string' }, check: { type: 'boolean' }, 'expires-at': { type: 'string' }, help: { type: 'boolean' },
  } });
  if (o.help) {
    console.log('node owner-login.mjs --expected-email EMAIL --fridica /absolute/fridica --config /absolute/config.toml --project PROJECT_ID [--browser /absolute/chromium] [--expires-at UNIX_SECONDS] [--check]');
    return;
  }
  require(['darwin', 'linux'].includes(process.platform), 'Owner login requires desktop macOS or Linux');
  require(Number(process.versions.node.split('.')[0]) >= 20, 'Node >=20 required');
  const email = o['expected-email']?.toLowerCase();
  require(email && email.length <= 254 && /^[\x21-\x7e]+$/.test(email) && /^[^\s@]+@[^\s@]+$/.test(email), 'Expected existing account email required');
  require(path.isAbsolute(o.fridica || '') && path.isAbsolute(o.config || ''), 'Absolute Fridica executable and config paths required');
  require(/^[0-9a-f]{24}$/.test(o.project || ''), 'Explicit project ID required');
  require(!o.browser || path.isAbsolute(o.browser), 'Browser override must be an absolute executable path');
  const expiry = o['expires-at'] === undefined ? undefined : Number(o['expires-at']);
  require(expiry === undefined || Number.isSafeInteger(expiry) && expiry > Date.now() / 1000, 'Explicit expiry must be in the future');
  process.umask(0o077);
  // Prevent ambient upstream overrides or proxy/TLS weakening. No credential values logged.
  for (const key of Object.keys(process.env)) {
    if (/^(OL_|NODE_|OVERLEAF_)|proxy$/i.test(key) || key === 'SSL_CERT_FILE' || key === 'SSL_CERT_DIR') delete process.env[key];
  }
  process.env.OL_BASE_URL = ORIGIN;
  if (o.browser) process.env.OL_BROWSER = o.browser;
  process.env.OL_MCP_LOG_LEVEL = 'error';
  await checkResources();
  if (o.check) { console.log('Owner login resources ready; no backend, browser, credential store or network opened.'); return; }
  await control(o.fridica, ['overleaf-auth', 'import-verified-session', '--help']);
  const store = await import('./node_modules/@netique/overleaf-mcp/dist/auth/cookieStore.js');
  const browser = await import('./node_modules/@netique/overleaf-mcp/dist/auth/browserLogin.js');
  await mkdir(path.dirname(store.cookieFilePath()), { recursive: true, mode: 0o700 });
  await chmod(path.dirname(store.cookieFilePath()), 0o700);
  await mkdir(browser.profileDir(), { recursive: true, mode: 0o700 });
  await chmod(browser.profileDir(), 0o700);
  console.log('Reusing the dedicated Overleaf session; Chrome opens only if login is required.');
  const verified = await acquire(email, o.project, expiry, { ...store, ...browser });
  await control(o.fridica, ['overleaf-auth', 'import-verified-session', '--config', o.config], JSON.stringify(verified));
  console.log('Verified session saved. Forwarding requires separate explicit recipient/channel binding. No listener reloaded or manuscript changed.');
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(() => { console.error('Owner login/setup failed; no credentials printed. Check the expected account, network, feature build and owner configuration.'); process.exitCode = 1; });
}
