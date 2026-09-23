from __future__ import annotations

import asyncio
from collections import Counter
from importlib.resources import files
import json
import re
import os
import secrets
import signal
import sqlite3
import time

from aiohttp import web

from .config import Config
from .dashboard_control import database


SECRET = re.compile(r"\b(?:xox[baprs]-[A-Za-z0-9-]+|xapp-[A-Za-z0-9-]+|sk-[A-Za-z0-9_-]+)")


def snapshot(config: Config, *, compact=False) -> dict:
    now = time.time()
    data = {
        'time': now, 'health': {'status': 'unknown'}, 'events': [], 'tasks': [],
        'requests': [], 'grants': [], 'counts': {}, 'last_message': None, 'attention_count': 0,
        'config': {'owner': config.owner_id, 'workspace': config.workspace_id,
                   'channels': list(config.channels), 'backend': config.backend, 'model': config.model, 'reasoning_effort': config.reasoning_effort, 'max_wait_replies': config.max_wait_replies, 'max_turns': config.max_turns,
                   'file_access': config.file_access,
                   'allowed_domains': list(config.allowed_domains), 'resume_sessions': config.resume_sessions,
                   'session_timeout': config.session_timeout,
                   'ssh_host': config.ssh_host,
                   'write_roots': config.root_labels(),
                   'read_roots': [config.root_label(p) for p in config.read_only_workspaces]},
    }
    if not config.state_path.exists():
        return data
    with database(config, require_identity=False) as db:
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
        for row in ([] if compact else db.execute(f'SELECT *,{outgoing} AS outgoing FROM events WHERE {scope} ORDER BY timestamp DESC LIMIT 200', params)):
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
        for row in ([] if compact else db.execute(f'SELECT channel,thread,task_id,status,turns,updated,control_state,continuation,debriefed_turn FROM tasks WHERE {scope} ORDER BY updated DESC LIMIT 100', params)):
            task = dict(row)
            if task['control_state'] == 'paused' and task['continuation'] not in (None, 'pending', 'failed'):
                task['status'] = 'continued'
            elif task['status'] == 'complete' and 0 < task['debriefed_turn'] and task['debriefed_turn'] >= task['turns']:
                task['status'] = 'finished'
            # Delivery errors do not update the task row. Use the latest request's state.
            latest = db.execute("SELECT event_id,state FROM events WHERE workspace=? AND channel=? AND thread=? "
                                "AND task_id=? AND reply_only=0 AND event_id NOT LIKE 'outgoing:%' ORDER BY timestamp DESC LIMIT 1",
                                (config.workspace_id, task['channel'], task['thread'], task['task_id'])).fetchone()
            if latest:
                if latest['state'] in {'failed', 'ambiguous', 'interrupted', 'running', 'sending', 'ready', 'blocked'} and task['status'] != 'continued':
                    task['status'] = latest['state']
                request = db.execute('SELECT status FROM file_requests WHERE event_id=?', (latest['event_id'],)).fetchone()
                if request and request['status'] == 'pending':
                    task['status'] = 'awaiting_approval'
            data['tasks'].append(task)
    data['task_counts'] = dict(Counter(t['status'] for t in data['tasks']))
    # Slack text can contain pasted credentials. Do not return recognized token formats.
    return json.loads(SECRET.sub('[redacted]', json.dumps(data)))


def task_page(config, *, view='all', query='', offset=0, limit=50, channel=None, thread=None, sender=None):
    from .dashboard_control import metadata
    result = {'items': [], 'total': 0, 'offset': offset, 'senders': [], 'counts': {'attention': 0, 'active': 0, 'waiting': 0, 'complete': 0, 'all': 0, 'archived': 0}}
    if not config.state_path.exists():
        return result
    placeholders = ','.join('?' for _ in config.channels)
    params = (config.workspace_id, *config.channels)
    scope = f'workspace=? AND channel IN ({placeholders})'
    sql = f'''
    WITH threads AS (
      SELECT channel,thread,task_id,status,updated,turns,control_state,pause_reason,control_revision,continuation,debriefed_turn FROM tasks WHERE {scope}
      UNION ALL
      SELECT channel,thread,'thread:'||thread,'pending',max(timestamp),0,'active',NULL,0,NULL,0 FROM events e
      WHERE {scope} AND state IN ('pending','running','failed','interrupted','ambiguous','blocked')
      AND NOT EXISTS(SELECT 1 FROM tasks t WHERE t.workspace=e.workspace AND t.channel=e.channel AND t.thread=e.thread)
      GROUP BY channel,thread
    ), rows AS (
      SELECT t.*,e.state AS delivery,e.result,e.decision,
        substr(json_extract(e.payload,'$.text'),1,600) AS title,
        json_extract(e.payload,'$.sender_id') AS sender,
        (SELECT count(*) FROM file_requests r JOIN events x ON x.event_id=r.event_id
         WHERE x.workspace=? AND x.channel=t.channel AND x.thread=t.thread AND r.status='pending') AS approvals,
        (SELECT r.status FROM file_requests r JOIN events x ON x.event_id=r.event_id
         WHERE x.workspace=? AND x.channel=t.channel AND x.thread=t.thread
         ORDER BY r.created DESC LIMIT 1) AS operation_status
      FROM threads t LEFT JOIN events e ON e.event_id=(
        SELECT x.event_id FROM events x WHERE x.workspace=? AND x.channel=t.channel AND x.thread=t.thread
        AND x.reply_only=0 AND (x.task_id=t.task_id OR t.task_id='thread:'||t.thread) AND x.event_id NOT LIKE 'outgoing:%'
        AND json_extract(x.payload,'$.sender_id')!=?
        ORDER BY x.timestamp DESC LIMIT 1)
    ), states AS (
      SELECT *, CASE
        WHEN control_state='paused' AND continuation IS NOT NULL AND continuation NOT IN ('pending','failed') THEN 'continued'
        WHEN control_state IN ('paused','closed','archived','cleaned') THEN control_state
        WHEN approvals>0 THEN 'awaiting_approval'
        WHEN operation_status IN ('approved','applying','rejected') THEN operation_status
        WHEN operation_status IN ('failed','interrupted') THEN operation_status
        WHEN delivery IN ('pending','running','ready','sending','failed','interrupted','ambiguous','blocked') THEN delivery
        WHEN debriefed_turn>0 AND debriefed_turn>=turns AND coalesce(json_extract(result,'$.status'),status)='complete' THEN 'finished'
        ELSE coalesce(json_extract(result,'$.status'),status) END AS current_status
      FROM rows
    ), classified AS (
      SELECT *, CASE
        WHEN current_status IN ('paused','awaiting_approval','failed','interrupted','ambiguous','blocked') THEN 'attention'
        WHEN current_status IN ('pending','running','ready','sending','approved','applying','rejected','delivery_pending') THEN 'active'
        WHEN current_status IN ('archived','cleaned') THEN current_status
        WHEN current_status='waiting' THEN 'waiting' ELSE 'complete' END AS bucket
      FROM states
    )
    '''
    parameters = (*params, *params, config.workspace_id, config.workspace_id, config.workspace_id, config.owner_id)
    with database(config, require_identity=False) as db:
        result['senders'] = [r['sender'] for r in db.execute(
            sql+"SELECT DISTINCT sender FROM classified WHERE bucket!='cleaned' AND sender IS NOT NULL ORDER BY sender", parameters)]
        has_notes = db.execute("SELECT 1 FROM sqlite_master WHERE name='collaboration'").fetchone()
        preferences = metadata(config)
        names = preferences['names']
        matching_names = json.dumps([identifier for identifier, name in names.items() if query.casefold() in name.casefold()])
        matching = "WHERE bucket!='cleaned' AND (? IS NULL OR sender=?) AND (instr(lower(coalesce(title,'')),lower(?))>0 OR instr(lower(coalesce(sender,'')),lower(?))>0 OR sender IN (SELECT value FROM json_each(?))) AND (? IS NULL OR channel=?) AND (? IS NULL OR thread=?)"
        args = (*parameters, sender, sender, query, query, matching_names, channel, channel, thread, thread)
        for row in db.execute(sql+'SELECT bucket,count(*) n FROM classified '+matching+' GROUP BY bucket', args):
            result['counts'][row['bucket']] = row['n']
            if row['bucket'] != 'archived':
                result['counts']['all'] += row['n']
        where = matching + " AND ((?='all' AND bucket!='archived') OR bucket=?)"
        args = (*args, view, view)
        result['total'] = db.execute(sql+'SELECT count(*) FROM classified '+where, args).fetchone()[0]
        for row in db.execute(sql+'SELECT * FROM classified '+where+' ORDER BY updated DESC LIMIT ? OFFSET ?', (*args, limit, offset)):
            item = dict(row)
            item['status'] = item.pop('current_status')
            response = json.loads(item.pop('result') or '{}')
            item['next_step'] = response.get('text', '')[:1200]
            item['repo'] = None
            if has_notes:
                note = db.execute("SELECT json_extract(c.data,'$.repo') FROM collaboration c JOIN tasks t "
                                  'ON c.workspace=t.workspace AND c.channel=t.channel AND c.root_thread=COALESCE(t.root_thread,t.thread) '
                                  'WHERE t.workspace=? AND t.channel=? AND t.thread=?',
                                  (config.workspace_id, item['channel'], item['thread'])).fetchone()
                item['repo'] = note[0] if note else None
            result['items'].append(item)
    for item in result['items']:
        item['project_id'] = preferences['task_projects'].get(item['channel']+':'+item['thread'])
    return json.loads(SECRET.sub('[redacted]', json.dumps(result)))


def thread_detail(config, channel, thread, offset=0):
    if channel not in config.channels:
        raise ValueError('Channel is not configured')
    result = {'events': [], 'requests': [], 'total': 0, 'offset': offset}
    if not config.state_path.exists():
        return result
    with database(config, require_identity=False) as db:
        args = (config.workspace_id, channel, thread)
        if db.execute("SELECT 1 FROM sqlite_master WHERE name='collaboration'").fetchone() and db.execute('SELECT 1 FROM tasks WHERE workspace=? AND channel=? AND thread=?', args).fetchone():
            from .collaboration import snapshot as task_notes
            result['collaboration'] = task_notes(db, config, channel, thread)
            result['collaboration'].pop('last_reply', None)
            result['collaboration']['history'] = [dict(row) for row in db.execute(
                'SELECT revision,actor,created FROM collaboration_history WHERE workspace=? AND channel=? AND root_thread=(SELECT COALESCE(root_thread,thread) FROM tasks WHERE workspace=? AND channel=? AND thread=?) ORDER BY revision DESC LIMIT 20',
                (config.workspace_id, channel, *args))]
        scope = 'workspace=? AND channel=? AND thread=?'
        result['total'] = db.execute('SELECT count(*) FROM events WHERE '+scope, args).fetchone()[0]
        for row in db.execute('SELECT * FROM events WHERE '+scope+' ORDER BY timestamp DESC LIMIT 50 OFFSET ?', (*args, offset)):
            payload = json.loads(row['payload'])
            response = json.loads(row['result'] or '{}')
            result['events'].append({'id':row['event_id'],'sender':payload.get('sender_id'),'text':payload.get('text',''),
                                     'timestamp':row['timestamp'],'state':row['state'],'result':response.get('text',''),
                                     'sent_ts':row['sent_ts'],'decision':row['decision'],'result_status':response.get('status')})
        result['requests'] = [dict(r) for r in db.execute(
            'SELECT r.id,r.sender,r.operation,r.path,r.status,r.error,r.notified,r.created,e.state AS delivery '
            'FROM file_requests r JOIN events e ON e.event_id=r.event_id '
            'WHERE e.workspace=? AND e.channel=? AND e.thread=? ORDER BY r.created DESC', args)]
    return json.loads(SECRET.sub('[redacted]', json.dumps(result)))


def create_app(config: Config, *, approval_key: str | None = None, config_path=None) -> web.Application:
    base_config = config

    @web.middleware
    async def local_only(request, handler):
        host = request.host
        if (not re.fullmatch(r'(?:127\.0\.0\.1|localhost)(?::\d+)?', host)
                or request.headers.get('Origin', f'http://{host}') != f'http://{host}'
                or request.headers.get('Sec-Fetch-Site') == 'cross-site'):
            raise web.HTTPForbidden()
        try:
            if config_path is not None and request.path.startswith('/api/'):
                nonlocal config
                from .settings import configured
                config = configured(base_config, config_path)
            response = await handler(request)
        except (ValueError, OSError):
            response = web.json_response({'error':'Configuration cannot be loaded. Repair it locally before continuing.'},status=503)
        except web.HTTPException as error:
            response = web.Response(status=error.status, text=error.reason, headers=error.headers)
        response.headers.update({
            'Cache-Control': 'no-store', 'X-Content-Type-Options': 'nosniff',
            'Content-Security-Policy': "default-src 'self'; object-src 'none'; frame-ancestors 'none'; base-uri 'none'",
            'Referrer-Policy': 'no-referrer',
        })
        return response

    def authorize(request, *, write=False):
        token = request.headers.get('Authorization', '').removeprefix('Bearer ')
        if not approval_key or not secrets.compare_digest(token, approval_key):
            raise web.HTTPForbidden(text='Unlock local controls with the key from your dashboard terminal.')
        if write and (request.headers.get('Origin') != f'http://{request.host}' or request.content_type != 'application/json'):
            raise web.HTTPForbidden(text='A same-origin JSON request is required.')

    async def settings(request):
        from .settings import read_settings, save_settings
        authorize(request, write=request.method=='POST')
        try:
            if request.method=='GET':
                result=await asyncio.to_thread(read_settings,base_config,config_path)
            else:
                body=await request.json()
                if not isinstance(body,dict): raise ValueError('Expected settings and revision.')
                result=await asyncio.to_thread(save_settings,base_config,config_path,body.get('changes'),body.get('revision'))
            return web.json_response(result)
        except (ValueError,OSError,sqlite3.Error) as error:
            return web.json_response({'error':str(error) if isinstance(error,ValueError) else 'Cannot save settings.'},status=409)

    async def grants(request):
        from .settings import update_grant
        authorize(request,write=True)
        try:
            return web.json_response(await asyncio.to_thread(update_grant,config,await request.json(),config_path))
        except (ValueError,TypeError,OSError,sqlite3.Error) as error:
            return web.json_response({'error':str(error) if isinstance(error,ValueError) else 'Cannot update grant.'},status=409)

    async def detail(request):
        from .dashboard_control import request_detail
        authorize(request)
        try:
            return web.json_response(await asyncio.to_thread(request_detail, config, request.match_info['id']))
        except (ValueError, OSError, sqlite3.Error) as error:
            return web.json_response({'error': str(error) if isinstance(error, ValueError) else 'Cannot inspect this request.'}, status=409)

    async def decision(request):
        from .dashboard_control import decide
        authorize(request, write=True)
        try:
            body = await request.json()
            if not isinstance(body, dict):
                raise ValueError('Expected a decision and proposal revision')
            result = await asyncio.to_thread(decide, config, request.match_info['id'], body.get('decision'), body.get('revision'))
            return web.json_response(result)
        except (ValueError, OSError, sqlite3.Error) as error:
            return web.json_response({'error': str(error) if isinstance(error, ValueError) else 'Cannot record the decision.'}, status=409)

    async def project(request):
        from .dashboard_control import bind_project, assign_project
        authorize(request, write=True)
        try:
            body = await request.json()
            if not isinstance(body, dict):
                raise ValueError('Expected project details')
            if request.path == '/api/projects':
                result = bind_project(config, body.get('root'), body.get('repository'))
            else:
                result = assign_project(config, body.get('channel'), body.get('thread'), body.get('project_id'))
            return web.json_response(result)
        except (ValueError, TypeError, OSError, sqlite3.Error) as error:
            return web.json_response({'error': str(error) if isinstance(error, ValueError) else 'Cannot save project details.'}, status=409)

    async def task_notes(request):
        from .collaboration import owner_update
        authorize(request, write=True)
        try:
            body = await request.json()
            if not isinstance(body, dict):
                raise ValueError('Expected task notes and revision')
            result = await asyncio.to_thread(owner_update, config, body.get('channel'), body.get('thread'),
                                            body.get('revision'), body.get('changes'), body.get('correction'))
            return web.json_response(result)
        except (ValueError, TypeError, OSError, sqlite3.Error) as error:
            return web.json_response({'error': str(error) if isinstance(error, ValueError) else 'Cannot save task notes.'}, status=409)

    async def lifecycle(request):
        from .dashboard_control import cleanup_preview, thread_action
        authorize(request, write=request.method == 'POST')
        try:
            if request.method == 'GET':
                result = await asyncio.to_thread(cleanup_preview, config, request.query.get('channel'), request.query.get('thread'))
            else:
                body = await request.json()
                if not isinstance(body, dict):
                    raise ValueError('Expected request controls')
                result = await asyncio.to_thread(thread_action, config, body.get('channel'), body.get('thread'), body.get('action'), body.get('revision'), body.get('cleanup_revision'))
            return web.json_response(result)
        except (ValueError, TypeError, OSError, sqlite3.Error) as error:
            return web.json_response({'error': str(error) if isinstance(error, ValueError) else 'Cannot update this request.'}, status=409)

    async def access(request):
        authorize(request)
        return web.json_response({'owner': config.owner_id, 'enabled': True})

    async def stop(request):
        authorize(request, write=True)
        # Only this dashboard process exits; the Slack listener is separate.
        asyncio.get_running_loop().call_later(.5, os.kill, os.getpid(), signal.SIGTERM)
        return web.json_response({'stopping': True})

    names_task = None
    names_checked = {}
    pending_names = set()

    async def resolve_names():
        from .dashboard_control import metadata, save_metadata
        from slack_sdk.web.async_client import AsyncWebClient
        import aiohttp
        token = os.environ.get(config.user_token_env)
        if not token:
            return
        async with aiohttp.ClientSession() as session:
            client = AsyncWebClient(token=token, session=session, timeout=5, retry_handlers=[])
            while pending_names:
                identifier = pending_names.pop()
                names_checked[identifier] = time.time()
                try:
                    if identifier in config.channels:
                        response = await client.conversations_info(channel=identifier)
                        name = '#' + response['channel']['name']
                    else:
                        response = await client.users_info(user=identifier)
                        profile = response['user'].get('profile', {})
                        name = profile.get('display_name') or profile.get('real_name') or response['user'].get('name')
                    if name:
                        value = metadata(config)
                        value['names'][identifier] = str(name)[:100]
                        save_metadata(config, value)
                except Exception:
                    continue

    def queue_names(rows, senders=()):
        nonlocal names_task
        from .dashboard_control import metadata
        if not os.environ.get(config.user_token_env):
            return
        ids = [config.owner_id, *config.channels, *senders]
        for row in rows:
            if row.get('sender'):
                ids.append(row['sender'])
            ids.extend(re.findall(r'<@([UW][A-Z0-9]+)(?:\|[^>]+)?>', json.dumps(row)))
        known = metadata(config)['names']
        for identifier in ids:
            if identifier not in known and time.time() - names_checked.get(identifier, 0) > 300:
                if len(pending_names) < 30:
                    pending_names.add(identifier)
        if pending_names and (names_task is None or names_task.done()):
            names_task = asyncio.create_task(resolve_names())

    async def cleanup(app):
        if names_task:
            names_task.cancel()
            await asyncio.gather(names_task, return_exceptions=True)

    async def activity(request):
        from .activity import activity_page
        try:
            before=float(request.query['before']) if 'before' in request.query else None
            result=await asyncio.to_thread(activity_page,config,view=request.query.get('view','current'),offset=max(0,int(request.query.get('offset',0))),before=before)
            queue_names(result['items'])
            return web.json_response(json.loads(SECRET.sub('[redacted]',json.dumps(result))))
        except (ValueError,OSError,sqlite3.Error):
            return web.json_response({'error':'Cannot load activity. Check the selected date and view.'},status=400)

    async def archive_activity(request):
        from .activity import move_activity
        authorize(request,write=True)
        try:
            body=await request.json()
            if not isinstance(body,dict):raise ValueError('Choose an archive action.')
            return web.json_response(await asyncio.to_thread(move_activity,config,body.get('action'),body.get('before')))
        except (ValueError,OSError,sqlite3.Error) as error:
            return web.json_response({'error':str(error) if isinstance(error,ValueError) else 'Cannot move activity.'},status=409)

    async def tasks(request):
        try:
            offset = max(0, int(request.query.get('offset', 0)))
            result = await asyncio.to_thread(task_page, config, view=request.query.get('view','all'), query=request.query.get('q','')[:200], offset=offset, channel=request.query.get('channel'), thread=request.query.get('thread'), sender=request.query.get('sender') or None)
            queue_names(result['items'], result['senders'])
            return web.json_response(result)
        except (ValueError, OSError, sqlite3.Error):
            return web.json_response({'error':'Cannot load tasks.'}, status=503)

    async def thread(request):
        try:
            offset = max(0, int(request.query.get('offset', 0)))
            result = await asyncio.to_thread(thread_detail, config, request.query.get('channel'), request.query.get('thread'), offset)
            queue_names(result['events'])
            return web.json_response(result)
        except (ValueError, OSError, sqlite3.Error):
            return web.json_response({'error':'Cannot load this thread.'}, status=503)

    async def state(request):
        from .dashboard_control import metadata
        try:
            data = await asyncio.to_thread(snapshot, config, compact=True)
            overview = await asyncio.to_thread(task_page, config)
            value = metadata(config)
            data['summary'] = overview['counts']
            data['names'] = value['names']
            data['projects'] = list(value['projects'].values())
            from .repos import load_repos
            data['repositories'] = [repo.payload() for repo in load_repos(config.repos)]
            data['approvals_enabled'] = bool(approval_key)
            data['settings_enabled'] = config_path is not None
            if config_path is not None:
                from .settings import read_settings
                settings_state=await asyncio.to_thread(read_settings,base_config,config_path)
                data['settings_revision']=settings_state['revision']
                data['settings_applied']=data['health']['status']=='connected' and settings_state['applied_revision']==settings_state['revision'] and settings_state['applied_pid']==data['health'].get('pid')
            data['tasks'] = []
            queue_names(overview['items'], overview['senders'])
        except (ValueError, sqlite3.Error, OSError):
            return web.json_response({'error': 'Cannot read state. Check the configured database and identity.'}, status=503)
        return web.json_response(data)

    async def resource(request):
        name = request.match_info.get('name', 'dashboard.html')
        content_type = {'dashboard.html': 'text/html', 'dashboard.js': 'text/javascript', 'dashboard.css': 'text/css'}
        if name not in content_type:
            raise web.HTTPNotFound()
        return web.Response(body=files('fridica').joinpath(name).read_bytes(), content_type=content_type[name])

    app = web.Application(middlewares=[local_only], client_max_size=8192)
    app.router.add_get('/api/state', state)
    app.on_cleanup.append(cleanup)
    app.router.add_get('/api/activity', activity)
    app.router.add_get('/api/tasks', tasks)
    app.router.add_get('/api/thread', thread)
    app.router.add_get('/api/access', access)
    app.router.add_get('/api/cleanup-preview', lifecycle)
    app.router.add_post('/api/thread-action', lifecycle)
    app.router.add_get('/api/requests/{id}', detail)
    app.router.add_post('/api/requests/{id}/decision', decision)
    app.router.add_post('/api/projects', project)
    app.router.add_post('/api/task-project', project)
    app.router.add_post('/api/task-notes', task_notes)
    app.router.add_post('/api/stop', stop)
    app.router.add_post('/api/activity/archive', archive_activity)
    app.router.add_get('/api/settings', settings)
    app.router.add_post('/api/settings', settings)
    app.router.add_post('/api/grants', grants)
    app.router.add_get('/', resource)
    app.router.add_get('/{name}', resource)
    return app


def serve_dashboard(config: Config, port: int, allow_approvals: bool = False, config_path=None) -> None:
    if not 1 <= port <= 65535:
        raise ValueError('port must be between 1 and 65535')
    key = secrets.token_urlsafe(32) if allow_approvals else None
    if key:
        path = config.state_path.with_suffix('.dashboard-key')
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
        with os.fdopen(descriptor, 'w') as stream:
            os.fchmod(stream.fileno(), 0o600)
            stream.write(key)
        print(f'Local approval key saved to {path}. Unlock controls in Settings.')
    web.run_app(create_app(config, approval_key=key, config_path=config_path), host='127.0.0.1', port=port, access_log=None)
