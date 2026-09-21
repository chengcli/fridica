'use strict';
const $ = id => document.getElementById(id);
let data, selected = null;
const attention = new Set(['awaiting_approval', 'failed', 'ambiguous', 'interrupted', 'blocked']);
const active = new Set(['running', 'ready', 'sending', 'delivery_pending']);
const label = value => (value || 'unknown').replaceAll('_', ' ');
function node(tag, text, className) {
  const el = document.createElement(tag);
  if (text !== undefined) el.textContent = text;
  if (className) el.className = className;
  return el;
}
function badge(value) {
  return node('span', label(value), 'badge ' + (attention.has(value) || value === 'offline' ? 'bad' : ['pending', 'waiting', 'reconnecting', 'unknown'].includes(value) ? 'warn' : ['connected', 'complete', 'sent'].includes(value) ? 'good' : ''));
}
function when(timestamp) { return timestamp ? new Date(timestamp * 1000).toLocaleString() : 'No messages yet'; }
function ago(timestamp) {
  if (!timestamp) return '—';
  const seconds = Math.max(0, Math.floor(Date.now() / 1000 - timestamp));
  return seconds < 60 ? `${seconds}s ago` : seconds < 3600 ? `${Math.floor(seconds / 60)}m ago` : seconds < 86400 ? `${Math.floor(seconds / 3600)}h ago` : `${Math.floor(seconds / 86400)}d ago`;
}
function stateFor(event) { return event.status || event.state; }
function renderActivity() {
  const query = $('search').value.toLowerCase();
  const filter = $('filter').value;
  const rows = data.events.filter(e => {
    if (filter === 'tasks' && (!e.task_id || e.outgoing)) return false;
    if (filter === 'attention' && !attention.has(stateFor(e))) return false;
    if (filter === 'outgoing' && !e.outgoing) return false;
    return [e.text, e.result, e.sender, e.task_id, e.channel].join(' ').toLowerCase().includes(query);
  });
  const fragment = document.createDocumentFragment();
  for (const event of rows) {
    const button = node('button', undefined, 'row' + (selected === event.thread + ':' + event.channel ? ' selected' : ''));
    button.type = 'button';
    const top = node('div', undefined, 'row-top');
    top.append(badge(stateFor(event)), node('span', event.outgoing ? '↑ Fridica reply' : event.sender), node('time', ago(event.timestamp)));
    button.append(top, node('div', event.text, 'row-title'), node('div', `${event.channel} · ${event.task_id || (event.decision ? label(event.decision) : 'Recorded message')}`, 'row-meta'));
    button.onclick = () => { selected = event.thread + ':' + event.channel; renderActivity(); renderThread(); $('details').scrollIntoView({behavior: 'smooth', block: 'start'}); };
    fragment.append(button);
  }
  if (!rows.length) fragment.append(node('p', 'No matching activity. New recorded messages will appear here.', 'empty'));
  $('activity').replaceChildren(fragment);
}
function renderThread() {
  $('details').hidden = !selected;
  if (!selected) return;
  const events = data.events.filter(e => e.thread + ':' + e.channel === selected).reverse();
  $('thread-label').textContent = `${selected} · Within the latest 200 recorded messages`;
  const items = events.map(e => {
    const item = node('article', undefined, 'timeline-item');
    item.append(badge(e.state), node('p', `${e.sender} · ${when(e.timestamp)}`, 'row-meta'), node('pre', e.text));
    if (e.result && !e.outgoing) {
      const result = node('div', undefined, 'result');
      result.append(node('small', e.state === 'sent' ? 'Result · delivery confirmed' : 'Result · delivery not confirmed'), node('pre', e.result));
      item.append(result);
    }
    if (e.attempts) item.append(node('small', `Delivery retries: ${e.attempts}${e.retry_at ? ' · Next retry: ' + when(e.retry_at) : ''}`));
    const link = node('a', 'Open Slack channel');
    link.href = `https://slack.com/app_redirect?team=${encodeURIComponent(data.config.workspace)}&channel=${encodeURIComponent(e.channel)}`;
    link.target = '_blank'; link.rel = 'noopener noreferrer';
    const links = node('p'); links.append(link); item.append(links);
    return item;
  });
  if (!items.length) items.push(node('p', 'This thread is outside the recent message window.'));
  $('timeline').replaceChildren(...items);
}
function renderPermissions() {
  const parts = [node('p', data.config.file_access ? 'Scoped file access enabled. A directory boundary alone does not grant writes.' : 'Managed file access is disabled.')];
  const requests = data.requests.filter(r => ['pending', 'approved', 'applying', 'failed', 'interrupted'].includes(r.status));
  if (!requests.length) parts.push(node('p', 'No pending file requests.', 'quiet'));
  for (const r of requests) {
    const item = node('div', undefined, 'request');
    item.append(badge(r.status), node('p', `${r.operation} · ${r.path}`), node('p', `Request ${r.id}`), node('p', r.error || `From ${r.sender}`));
    parts.push(item);
  }
  parts.push(node('p', `${data.grants.length} active grants (up to 100 shown)`));
  for (const grant of data.grants) parts.push(node('p', `${grant.sender} · ${grant.path} · ${grant.expires ? 'until ' + when(grant.expires) : 'until revoked'}`));
  $('permissions').replaceChildren(...parts);
  const roots = [];
  for (const [kind, paths] of [['Project roots · writes require authorization', data.config.write_roots], ['Read-only roots', data.config.read_roots]]) {
    for (const path of paths) {
      const scope = node('div', undefined, 'scope');
      scope.append(node('p', kind), node('code', path)); roots.push(scope);
    }
  }
  $('roots').replaceChildren(...roots);
}
function render() {
  const h = data.health;
  $('connection').replaceWith(Object.assign(badge(h.status), {id: 'connection'}));
  $('identity').textContent = `${data.config.backend} · ${data.config.owner} · ${data.config.channels.join(', ')}`;
  $('listener').textContent = label(h.status);
  $('heartbeat').textContent = h.heartbeat_at ? `Heartbeat ${ago(h.heartbeat_at)} · PID ${h.pid}${h.observe_only ? ' · observe only' : ''}` : 'Restart listener to record its heartbeat';
  $('active').textContent = data.tasks.filter(t => active.has(t.status)).length;
  $('attention').textContent = data.attention_count;
  $('last-message').textContent = ago(data.last_message);
  $('messages').textContent = when(data.last_message);
  $('refreshed').textContent = `Updated ${new Date().toLocaleTimeString()}`;
  renderActivity(); renderPermissions(); renderThread();
}
async function refresh() {
  try {
    const response = await fetch('/api/state', {cache: 'no-store', signal: AbortSignal.timeout(5000)});
    if (!response.ok) throw new Error('State unavailable');
    data = await response.json();
    render(); $('error').hidden = true;
  } catch (error) {
    $('error').textContent = 'Live updates unavailable. Displayed data may be stale. Check the local dashboard process and database.';
    $('error').hidden = false;
    $('connection').replaceWith(Object.assign(badge('offline'), {id: 'connection'}));
    $('refreshed').textContent = 'Updates disconnected';
  } finally { setTimeout(refresh, 2000); }
}
$('search').oninput = () => data && renderActivity();
$('filter').onchange = () => data && renderActivity();
$('close').onclick = () => { selected = null; renderThread(); renderActivity(); };
refresh();
