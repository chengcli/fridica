from __future__ import annotations

import asyncio
from collections import Counter
from contextlib import closing
from importlib.resources import files
import json
import re
import sqlite3
import time

from aiohttp import web

from .config import Config


SECRET = re.compile(r"\b(?:xox[baprs]-[A-Za-z0-9-]+|xapp-[A-Za-z0-9-]+|sk-[A-Za-z0-9_-]+)")


def snapshot(config: Config) -> dict:
    now = time.time()
    data = {
        'time': now, 'health': {'status': 'unknown'}, 'events': [], 'tasks': [],
        'requests': [], 'grants': [], 'counts': {}, 'last_message': None, 'attention_count': 0,
        'config': {'owner': config.owner_id, 'workspace': config.workspace_id,
                   'channels': list(config.channels), 'backend': config.backend,
                   'file_access': config.file_access,
                   'write_roots': [str(p) for p in (config.workspace, *config.additional_workspaces)],
                   'read_roots': [str(p) for p in config.read_only_workspaces]},
    }
    if not config.state_path.exists():
        return data
    with closing(sqlite3.connect(config.state_path.as_uri() + '?mode=ro', uri=True, timeout=1)) as db:
        db.row_factory = sqlite3.Row
        db.execute('PRAGMA query_only=ON')
        db.execute('BEGIN')
        identity = db.execute('SELECT owner,workspace FROM identity').fetchone()
        if identity is not None and tuple(identity) != (config.owner_id, config.workspace_id):
            raise ValueError('State database identity does not match configuration')
        if db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime'").fetchone():
            health = db.execute('SELECT pid,started_at,heartbeat_at,status,observe_only FROM runtime WHERE id=1').fetchone()
            if health:
                data['health'] = dict(health)
                if now - health['heartbeat_at'] > 15 and health['status'] != 'stopped':
                    data['health']['status'] = 'offline'
        placeholders = ','.join('?' for _ in config.channels)
        scope = f'workspace=? AND channel IN ({placeholders})'
        params = (config.workspace_id, *config.channels)
        data['counts'] = {r['state']: r['n'] for r in db.execute(
            f'SELECT state,count(*) n FROM events WHERE {scope} GROUP BY state', params)}
        data['attention_count'] = db.execute(
            f"SELECT count(*) FROM events WHERE {scope} AND (state IN ('failed','ambiguous','interrupted','blocked') "
            "OR json_extract(result,'$.status')='blocked' OR EXISTS (SELECT 1 FROM file_requests r "
            "WHERE r.event_id=events.event_id AND r.status IN ('pending','failed','interrupted')))", params).fetchone()[0]
        outgoing = "(event_id LIKE 'outgoing:%' OR EXISTS (SELECT 1 FROM events sent WHERE " \
                   "sent.workspace=events.workspace AND sent.channel=events.channel " \
                   "AND sent.sent_ts=json_extract(events.payload,'$.timestamp')))"
        data['last_message'] = db.execute(
            f"SELECT max(timestamp) FROM events WHERE {scope} AND NOT {outgoing}", params).fetchone()[0]
        for row in db.execute(f'SELECT *,{outgoing} AS outgoing FROM events WHERE {scope} ORDER BY timestamp DESC LIMIT 200', params):
            payload = json.loads(row['payload'])
            result = json.loads(row['result']) if row['result'] else {}
            state = 'sent' if row['outgoing'] else row['state']
            status = result.get('status', state) if state == 'sent' else state
            request = db.execute('SELECT status FROM file_requests WHERE event_id=?', (row['event_id'],)).fetchone()
            if request and request['status'] == 'pending':
                status = 'awaiting_approval'
            data['events'].append({
                'id': row['event_id'], 'channel': row['channel'], 'thread': row['thread'],
                'timestamp': row['timestamp'], 'state': state, 'status': status, 'decision': row['decision'],
                'sender': payload.get('sender_id'), 'text': payload.get('text', '')[:40000],
                'outgoing': bool(row['outgoing']),
                'result': result.get('text', '')[:40000], 'task_id': row['task_id'] or (payload.get('task_id') if row['outgoing'] else None),
                'attempts': row['attempts'], 'retry_at': row['retry_at'],
            })
        data['requests'] = [dict(r) for r in db.execute(
            f'SELECT r.id,r.event_id,r.sender,r.channel,r.operation,r.path,r.status,r.created,r.error '
            f'FROM file_requests r JOIN events e ON e.event_id=r.event_id '
            f'WHERE e.workspace=? AND e.channel IN ({placeholders}) ORDER BY r.created DESC LIMIT 100', params)]
        data['grants'] = [dict(r) for r in db.execute(
            f'SELECT id,sender,channel,path,expires FROM grants WHERE channel IN ({placeholders}) '
            'AND revoked=0 AND (expires IS NULL OR expires>?) ORDER BY id LIMIT 100', (*config.channels, now))]
        for row in db.execute(f'SELECT channel,thread,task_id,status,turns,updated FROM tasks WHERE {scope} ORDER BY updated DESC LIMIT 100', params):
            task = dict(row)
            # Delivery errors do not update the task row. Use the latest request's state.
            latest = db.execute("SELECT event_id,state FROM events WHERE workspace=? AND channel=? AND thread=? "
                                "AND task_id=? AND reply_only=0 AND event_id NOT LIKE 'outgoing:%' ORDER BY timestamp DESC LIMIT 1",
                                (config.workspace_id, task['channel'], task['thread'], task['task_id'])).fetchone()
            if latest:
                if latest['state'] in {'failed', 'ambiguous', 'interrupted', 'running', 'sending', 'ready', 'blocked'}:
                    task['status'] = latest['state']
                request = db.execute('SELECT status FROM file_requests WHERE event_id=?', (latest['event_id'],)).fetchone()
                if request and request['status'] == 'pending':
                    task['status'] = 'awaiting_approval'
            data['tasks'].append(task)
    data['task_counts'] = dict(Counter(t['status'] for t in data['tasks']))
    # Slack text can contain pasted credentials. Do not return recognized token formats.
    return json.loads(SECRET.sub('[redacted]', json.dumps(data)))


def create_app(config: Config) -> web.Application:
    @web.middleware
    async def local_only(request, handler):
        host = request.host
        if (not re.fullmatch(r'(?:127\.0\.0\.1|localhost)(?::\d+)?', host)
                or request.headers.get('Origin', f'http://{host}') != f'http://{host}'
                or request.headers.get('Sec-Fetch-Site') == 'cross-site'):
            raise web.HTTPForbidden()
        try:
            response = await handler(request)
        except web.HTTPException as error:
            response = web.Response(status=error.status, text=error.reason, headers=error.headers)
        response.headers.update({
            'Cache-Control': 'no-store', 'X-Content-Type-Options': 'nosniff',
            'Content-Security-Policy': "default-src 'self'; object-src 'none'; frame-ancestors 'none'; base-uri 'none'",
            'Referrer-Policy': 'no-referrer',
        })
        return response

    async def state(request):
        try:
            data = await asyncio.to_thread(snapshot, config)
        except (ValueError, sqlite3.Error, OSError):
            return web.json_response({'error': 'Cannot read state. Check the configured database and identity.'}, status=503)
        return web.json_response(data)

    async def resource(request):
        name = request.match_info.get('name', 'dashboard.html')
        content_type = {'dashboard.html': 'text/html', 'dashboard.js': 'text/javascript', 'dashboard.css': 'text/css'}
        if name not in content_type:
            raise web.HTTPNotFound()
        return web.Response(body=files('fridica').joinpath(name).read_bytes(), content_type=content_type[name])

    app = web.Application(middlewares=[local_only])
    app.router.add_get('/api/state', state)
    app.router.add_get('/', resource)
    app.router.add_get('/{name}', resource)
    return app


def serve_dashboard(config: Config, port: int) -> None:
    if not 1 <= port <= 65535:
        raise ValueError('port must be between 1 and 65535')
    web.run_app(create_app(config), host='127.0.0.1', port=port, access_log=None)
