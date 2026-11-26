// Thin owner bridge. Browser, CDP and dedicated-profile lifetime belong upstream.
import { spawn } from 'node:child_process';
import { parseArgs } from 'node:util';
import { pathToFileURL } from 'node:url';
import { chmod, mkdir, readFile } from 'node:fs/promises';
import path from 'node:path';

export const ORIGIN = 'https://www.overleaf.com';
export const OPERATIONS = ['list_comments', 'read_comment_thread', 'add_comment', 'reply_comment',
  'read_file', 'list_tracked_changes', 'find_and_replace'];
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

export function envelope(identity, cookie, expectedEmail, project, expiresAt) {
  require(/^[0-9a-f]{24}$/.test(identity.userId || '') && identity.userEmail?.toLowerCase() === expectedEmail,
    'Expected Overleaf account verification failed; no session imported');
  return { schema: 3, cookie, account_id: identity.userId, expected_email: expectedEmail,
    ...(project === undefined ? {} : { projects: [project] }), operations: OPERATIONS,
    ...(expiresAt === undefined ? {} : { expires_at: expiresAt }) };
}

export async function acquire(expectedEmail, project, expiresAt, upstream) {
  const verify = async cookie => envelope(await upstream.validateCookie(cookie), cookie, expectedEmail, project, expiresAt);
  const saved = await upstream.loadStored(ORIGIN);
  if (saved?.cookie) {
    const cookie = selectCookie(saved.cookie);
    try { return await verify(cookie); }
    catch (error) {
      // Upstream uses the same error class for expiry and server errors. Only an
      // explicit login redirect/401 permits browser renewal; never retry mismatch/5xx.
      const expired = upstream.OverleafAuthError && error instanceof upstream.OverleafAuthError &&
        (/^Session cookie rejected \(redirected to (?:\/login(?:[/?][^)]*)?|https:\/\/www\.overleaf\.com\/login(?:[/?][^)]*)?|login)\)/.test(error.message) ||
          error.message === 'GET /project returned HTTP 401');
      if (!expired) throw error;
    }
  }
  // Upstream owns the ordinary dedicated Chrome login; human enters credentials/MFA.
  const cookie = selectCookie(await upstream.captureCookie(ORIGIN));
  const verified = await verify(cookie);
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

export const SOURCE = '42135403af5e76b80348303d742a5e1fe4da6d30';
export function pinnedSource(resolved) {
  return ['https://', 'git+https://', 'ssh://git@', 'git+ssh://git@'].some(prefix =>
    ['', '.git'].some(suffix => resolved ===
      `${prefix}github.com/happysky19/overleaf-mcp-native${suffix}#${SOURCE}`));
}

export async function checkResources() {
  const pkg = JSON.parse(await readFile(new URL('./node_modules/@netique/overleaf-mcp/package.json', import.meta.url)));
  const lock = JSON.parse(await readFile(new URL('./node_modules/.package-lock.json', import.meta.url)));
  require(pkg.version === '0.4.0' && pinnedSource(lock.packages?.['node_modules/@netique/overleaf-mcp']?.resolved),
    'Pinned Overleaf dependency missing or mismatched');
  await import('./node_modules/@netique/overleaf-mcp/dist/auth/cookieStore.js');
  await import('./node_modules/@netique/overleaf-mcp/dist/auth/browserLogin.js');
  await import('./node_modules/@netique/overleaf-mcp/dist/auth/sessionFd.js');
  await import('./node_modules/@netique/overleaf-mcp/dist/session/identity.js');
}

async function main() {
  const { values: o } = parseArgs({ options: {
    'expected-email': { type: 'string' }, fridica: { type: 'string' }, config: { type: 'string' },
    project: { type: 'string' }, browser: { type: 'string' }, check: { type: 'boolean' }, 'expires-at': { type: 'string' }, help: { type: 'boolean' },
  } });
  if (o.help) {
    console.log('node owner-login.mjs --expected-email EMAIL --fridica /absolute/fridica --config /absolute/config.toml [--project PROJECT_ID] [--browser /absolute/chromium] [--expires-at UNIX_SECONDS] [--check]');
    return;
  }
  require(['darwin', 'linux'].includes(process.platform), 'Owner login requires desktop macOS or Linux');
  require(Number(process.versions.node.split('.')[0]) >= 20, 'Node >=20 required');
  const email = o['expected-email']?.toLowerCase();
  require(email && email.length <= 254 && /^[\x21-\x7e]+$/.test(email) && /^[^\s@]+@[^\s@]+$/.test(email), 'Expected existing account email required');
  require(path.isAbsolute(o.fridica || '') && path.isAbsolute(o.config || ''), 'Absolute Fridica executable and config paths required');
  require(o.project === undefined || /^[0-9a-f]{24}$/.test(o.project), 'Invalid optional project restriction');
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
  const identity = await import('./node_modules/@netique/overleaf-mcp/dist/session/identity.js');
  const errors = await import('./node_modules/@netique/overleaf-mcp/dist/api/errors.js');
  await mkdir(path.dirname(store.cookieFilePath()), { recursive: true, mode: 0o700 });
  await chmod(path.dirname(store.cookieFilePath()), 0o700);
  await mkdir(browser.profileDir(), { recursive: true, mode: 0o700 });
  await chmod(browser.profileDir(), 0o700);
  console.log('Reusing the dedicated Overleaf session; Chrome opens only if login is required.');
  const verified = await acquire(email, o.project, expiry, { ...store, ...browser, ...identity, ...errors });
  await control(o.fridica, ['overleaf-auth', 'import-verified-session', '--config', o.config], JSON.stringify(verified));
  console.log('Verified session saved. Eligible workers receive native tools through Fridica admission; existing restrictions are retained. No listener reloaded or manuscript changed.');
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(() => { console.error('Owner login/setup failed; no credentials printed. Check the expected account, network, feature build and owner configuration.'); process.exitCode = 1; });
}
