import { test } from 'node:test';
import assert from 'node:assert/strict';
import { acquire, envelope, selectCookie, checkResources, pinnedSource, SOURCE } from './owner-login.mjs';

const PROJECT = '333333333333333333333333';

test('owner helper reuses the saved session and enforces account/project before capture storage', async () => {
  // npm may record the exact locked Git revision as git+ssh, not git+https.
  await checkResources();
  const source = `git+https://github.com/happysky19/overleaf-mcp-native.git#${SOURCE}`;
  assert.ok(pinnedSource(source));
  assert.ok(pinnedSource(source.replace('git+https://', 'git+ssh://git@')));
  for (const bad of [source.replace('happysky19/', 'other/'), source.replace('github.com/', 'github.com.evil/'),
    source.replace(SOURCE, SOURCE.slice(0, 12)), source.replace(SOURCE, 'main'), `${source}extra`, undefined]) {
    assert.equal(pinnedSource(bad), false);
  }
  const body = `<meta name="ol-project_id" content="${PROJECT}"><meta content='{&quot;_id&quot;:&quot;111111111111111111111111&quot;,&quot;email&quot;:&quot;owner@example.invalid&quot;}' name="ol-user">`;
  let captures = 0, saves = 0;
  const upstream = { loadStored: async () => ({ cookie: 'overleaf_session2=fixture; analytics=ignored' }),
    captureCookie: async () => { captures++; return 'overleaf_session2=fixture; GCLB=route; other=ignored'; },
    saveStored: async (_, cookie) => { saves++; assert.equal(cookie, 'overleaf_session2=fixture; GCLB=route'); } };
  const grant = await acquire('owner@example.invalid', PROJECT, undefined, upstream, async () => body);
  assert.equal(grant.cookie, 'overleaf_session2=fixture');
  assert.equal(grant.expires_at, undefined);
  assert.equal(captures, 0); assert.equal(saves, 0);
  await assert.rejects(acquire('other@example.invalid', PROJECT, undefined, upstream, async () => body));
  assert.equal(captures, 0);
  upstream.loadStored = async () => null;
  await assert.rejects(acquire('owner@example.invalid', PROJECT, undefined, upstream, async () => body.replace(PROJECT, '2'.repeat(24))));
  assert.equal(saves, 0);
  const fresh = await acquire('owner@example.invalid', PROJECT, 4000000000, upstream, async () => body);
  assert.equal(fresh.expires_at, 4000000000); assert.equal(saves, 1);
  assert.throws(() => envelope(body.replace('owner@', 'other@'), grant.cookie, 'owner@example.invalid', PROJECT));
  assert.throws(() => selectCookie('overleaf_session2=a; overleaf_session2=b'));
});
