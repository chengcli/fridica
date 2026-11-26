import { test } from 'node:test';
import assert from 'node:assert/strict';
import { acquire, selectCookie, checkResources, pinnedSource, SOURCE } from './owner-login.mjs';

const PROJECT = '333333333333333333333333';

test('pinned owner helper reuses upstream account validation and saved login without a project', async () => {
  await checkResources();
  const source = `git+https://github.com/happysky19/overleaf-mcp-native.git#${SOURCE}`;
  assert.ok(pinnedSource(source));
  assert.ok(pinnedSource(source.replace('git+https://', 'git+ssh://git@')));
  for (const bad of [source.replace('happysky19/', 'other/'), source.replace('github.com/', 'github.com.evil/'),
    source.replace(SOURCE, SOURCE.slice(0, 12)), source.replace(SOURCE, 'main'), `${source}extra`, undefined]) {
    assert.equal(pinnedSource(bad), false);
  }
  let captures = 0, saves = 0, validations = 0;
  const upstream = { loadStored: async () => ({ cookie: 'overleaf_session2=fixture; analytics=ignored' }),
    captureCookie: async () => { captures++; return 'overleaf_session2=fixture; GCLB=route; other=ignored'; },
    validateCookie: async cookie => {
      validations++; assert.ok(cookie.startsWith('overleaf_session2=fixture'));
      return { userId: '1'.repeat(24), userEmail: 'owner@example.invalid' };
    },
    saveStored: async (_, cookie) => { saves++; assert.equal(cookie, 'overleaf_session2=fixture; GCLB=route'); } };
  const grant = await acquire('owner@example.invalid', undefined, undefined, upstream);
  assert.equal(grant.schema, 3);
  assert.equal(grant.projects, undefined);
  assert.equal(grant.cookie, 'overleaf_session2=fixture');
  assert.equal(grant.expires_at, undefined);
  assert.equal(captures, 0); assert.equal(saves, 0); assert.equal(validations, 1);
  await assert.rejects(acquire('other@example.invalid', undefined, undefined, upstream), /account/);
  assert.equal(captures, 0);
  upstream.loadStored = async () => null;
  await assert.rejects(acquire('other@example.invalid', undefined, undefined, upstream), /account/);
  assert.equal(saves, 0);
  const restricted = await acquire('owner@example.invalid', PROJECT, 4000000000, upstream);
  assert.deepEqual(restricted.projects, [PROJECT]);
  assert.equal(restricted.expires_at, 4000000000); assert.equal(saves, 1);
  assert.throws(() => selectCookie('overleaf_session2=a; overleaf_session2=b'));
});

test('only actual expiry renews upstream login, never account/server failures', async () => {
  const { OverleafAuthError } = await import('./node_modules/@netique/overleaf-mcp/dist/api/errors.js');
  let captures = 0;
  const upstream = { OverleafAuthError,
    loadStored: async () => ({ cookie: 'overleaf_session2=expired' }),
    captureCookie: async () => { captures++; return 'overleaf_session2=renewed'; },
    saveStored: async () => {},
    validateCookie: async cookie => {
      if (cookie.includes('expired')) throw new OverleafAuthError('Session cookie rejected (redirected to /login). The cookie is likely expired');
      return { userId: '1'.repeat(24), userEmail: 'owner@example.invalid' };
    } };
  await acquire('owner@example.invalid', undefined, undefined, upstream);
  assert.equal(captures, 1);
  upstream.validateCookie = async () => { throw new OverleafAuthError('GET /project returned HTTP 503'); };
  await assert.rejects(acquire('owner@example.invalid', undefined, undefined, upstream));
  assert.equal(captures, 1);
});
