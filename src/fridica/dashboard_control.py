from __future__ import annotations

from contextlib import contextmanager
import difflib
import hashlib
import json
import os
import re
import sqlite3
import tempfile
import time
from types import SimpleNamespace

from .permissions import Files, Permissions


@contextmanager
def database(config, *, write=False, require_identity=True):
    db = sqlite3.connect(config.state_path.as_uri() + ('?mode=rw' if write else '?mode=ro'), uri=True, timeout=2)
    db.row_factory = sqlite3.Row
    try:
        if not write:
            db.execute('PRAGMA query_only=ON')
        db.execute('BEGIN IMMEDIATE' if write else 'BEGIN')
        identity = db.execute('SELECT owner,workspace FROM identity').fetchone()
        if ((identity is None and (write or require_identity))
                or (identity is not None and tuple(identity) != (config.owner_id, config.workspace_id))):
            raise ValueError('Database identity does not match this owner')
        yield db
    finally:
        db.close()


def get_request(config, db, identifier):
    row = db.execute('SELECT r.*,e.workspace,e.thread,e.state AS delivery FROM file_requests r '
                     'JOIN events e ON e.event_id=r.event_id WHERE r.id=?', (identifier,)).fetchone()
    if row is None or row['workspace'] != config.workspace_id or row['channel'] not in config.channels:
        raise ValueError('Request is not available in this workspace and channel')
    return dict(row)


def revision(row):
    fields = {k: row[k] for k in ('id', 'event_id', 'sender', 'channel', 'operation', 'path', 'content', 'before_content')}
    return hashlib.sha256(json.dumps(fields, sort_keys=True).encode()).hexdigest()


def request_detail(config, identifier):
    if not config.file_access:
        raise ValueError('Managed file access is disabled')
    with database(config) as db:
        row = get_request(config, db, identifier)
    before = row['before_content'] or ''
    after = '' if row['operation'] == 'delete' else row['content']
    lines = difflib.unified_diff(before.splitlines(keepends=True), after.splitlines(keepends=True),
                                 fromfile='Before', tofile='After')
    diff = ''.join(line if line.endswith('\n') else line + '\n\\ No newline at end of file\n' for line in lines)
    problem = None
    try:
        if Files(config).snapshot(row['path']) != row['before_content']:
            problem = 'File changed after this request was prepared. Reject it and request a fresh proposal.'
    except (ValueError, OSError):
        problem = 'The file is no longer accessible within the configured write boundaries.'
    return {**{k: row[k] for k in ('id','sender','channel','operation','path','status','delivery')},
            'revision': revision(row), 'diff': diff, 'before': before, 'after': after, 'problem': problem}


def decide(config, identifier, decision, expected):
    if not config.file_access:
        raise ValueError('Managed file access is disabled')
    if not isinstance(decision, str) or decision not in {'approved', 'rejected'}:
        raise ValueError('Choose approve or reject')
    with database(config, write=True) as db:
        row = get_request(config, db, identifier)
        if row['status'] != 'pending':
            raise ValueError('This request has already been decided. Refresh its status.')
        if revision(row) != expected:
            raise ValueError('The proposal changed. Review the current proposal before deciding.')
        if decision == 'approved' and Files(config).snapshot(row['path']) != row['before_content']:
            raise ValueError('The file changed. Reject this request and request a fresh proposal.')
        db.execute('CREATE TABLE IF NOT EXISTS dashboard_decisions (request_id TEXT PRIMARY KEY, owner TEXT NOT NULL, decision TEXT NOT NULL, revision TEXT NOT NULL, decided_at REAL NOT NULL)')
        db.execute('INSERT INTO dashboard_decisions VALUES(?,?,?,?,?)',
                   (identifier, config.owner_id, decision, expected, time.time()))
        # Commit the decision and audit entry together.
        Permissions(config, SimpleNamespace(connection=db)).decide(identifier, decision)
    return {'status': decision, 'execution': 'queued' if decision == 'approved' else 'not_requested'}


def metadata(config):
    path = config.state_path.with_suffix('.dashboard.json')
    try:
        result = json.loads(path.read_text())
        if isinstance(result, dict) and result.get('identity') == [config.owner_id, config.workspace_id] and all(isinstance(result.get(k), dict) for k in ('names','projects','task_projects')):
            return result
    except (OSError, ValueError):
        pass
    return {'identity': [config.owner_id, config.workspace_id], 'names': {}, 'projects': {}, 'task_projects': {}}


def save_metadata(config, value):
    path = config.state_path.with_suffix('.dashboard.json')
    with tempfile.NamedTemporaryFile(mode='w', dir=path.parent, delete=False) as stream:
        temporary = stream.name
        json.dump(value, stream, ensure_ascii=False)
    try:
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def bind_project(config, root, repository):
    roots = {str(p.resolve()) for p in (config.workspace, *config.additional_workspaces, *config.read_only_workspaces)}
    if root not in roots:
        raise ValueError('Choose an already configured directory. This page cannot expand file access.')
    if not isinstance(repository, str) or not re.fullmatch(r'https://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repository):
        raise ValueError('Use a GitHub repository URL: https://github.com/owner/repository')
    project_id = hashlib.sha256(root.encode()).hexdigest()[:16]
    value = metadata(config)
    value['projects'][project_id] = {'id': project_id, 'root': root, 'repository': repository,
                                    'name': repository.removeprefix('https://github.com/'), 'source': 'local_owner'}
    save_metadata(config, value)
    return value['projects'][project_id]


def assign_project(config, channel, thread, project_id):
    value = metadata(config)
    if project_id and project_id not in value['projects']:
        raise ValueError('Project not found')
    with database(config) as db:
        if channel not in config.channels or not db.execute('SELECT 1 FROM tasks WHERE workspace=? AND channel=? AND thread=?',
                    (config.workspace_id, channel, thread)).fetchone():
            raise ValueError('Task not found')
    value['task_projects'][channel + ':' + thread] = project_id
    save_metadata(config, value)
    return {'saved': True}


def _thread(config, db, channel, thread):
    if channel not in config.channels or not isinstance(thread, str):
        raise ValueError('Thread is not in a configured channel')
    row = db.execute('SELECT * FROM tasks WHERE workspace=? AND channel=? AND thread=?',
                     (config.workspace_id, channel, thread)).fetchone()
    if row is None:
        raise ValueError('Request not found')
    return dict(row)


def _idle(db, config, channel, thread, *, files=True):
    args = (config.workspace_id, channel, thread)
    if db.execute("SELECT 1 FROM events WHERE workspace=? AND channel=? AND thread=? AND state IN ('pending','running','ready','sending')", args).fetchone():
        raise ValueError('This request is still processing or delivering a reply. Wait for it to settle.')
    if files and db.execute("SELECT 1 FROM file_requests r JOIN events e ON r.event_id=e.event_id WHERE e.workspace=? AND e.channel=? AND e.thread=? AND (r.status IN ('pending','approved','applying','rejected') OR r.notified=0)", args).fetchone():
        raise ValueError('Resolve pending file operations and notifications before closing or cleaning this request.')


def _cleanup_preview(config, db, channel, thread):
    task = _thread(config, db, channel, thread)
    if task['control_state'] != 'archived':
        raise ValueError('Archive this request before clearing its contents')
    _idle(db, config, channel, thread)
    args = (config.workspace_id, channel, thread)
    events = [dict(r) for r in db.execute('SELECT * FROM events WHERE workspace=? AND channel=? AND thread=? ORDER BY event_id', args)]
    requests = [dict(r) for r in db.execute('SELECT r.* FROM file_requests r JOIN events e ON e.event_id=r.event_id WHERE e.workspace=? AND e.channel=? AND e.thread=? ORDER BY r.id', args)]
    raw = json.dumps([task, events, requests], sort_keys=True).encode()
    return {'messages': len(events), 'file_requests': len(requests),
            'revision': hashlib.sha256(raw).hexdigest(), 'control_revision': task['control_revision'],
            'description': 'Permanently clear locally stored message text, replies, file proposals, paths, and diffs for this request. Keep minimal event IDs for deduplication and decision records. Project files and Slack messages are not deleted. This cannot be undone; it is not a secure erase of backups or SQLite journal files.'}


def cleanup_preview(config, channel, thread):
    with database(config) as db:
        return _cleanup_preview(config, db, channel, thread)


def thread_action(config, channel, thread, action, expected, cleanup_revision=None):
    with database(config, write=True) as db:
        task = _thread(config, db, channel, thread)
        if type(expected) is not int or task['control_revision'] != expected:
            raise ValueError('Request controls changed. Refresh before deciding.')
        args = (config.workspace_id, channel, thread)
        current = task['control_state']
        if action == 'resume' and current == 'paused':
            _idle(db, config, channel, thread, files=False)
            db.execute("UPDATE tasks SET control_state='active',pause_reason=NULL,reset_at=?,turns=0,session=NULL WHERE workspace=? AND channel=? AND thread=?", (time.time(), *args))
        elif action == 'close' and current in ('active','paused'):
            _idle(db, config, channel, thread)
            db.execute("UPDATE tasks SET control_state='closed',status='closed',session=NULL WHERE workspace=? AND channel=? AND thread=?", args)
        elif action == 'archive' and (current == 'closed' or current == 'active' and task['status'] == 'complete'):
            _idle(db, config, channel, thread)
            db.execute("UPDATE tasks SET control_state='archived' WHERE workspace=? AND channel=? AND thread=?", args)
        elif action == 'restore' and current == 'archived':
            db.execute("UPDATE tasks SET control_state=? WHERE workspace=? AND channel=? AND thread=?", ('closed' if task['status']=='closed' else 'active', *args))
        elif action == 'clean' and current == 'archived':
            preview = _cleanup_preview(config, db, channel, thread)
            if preview['revision'] != cleanup_revision:
                raise ValueError('Request contents changed. Preview again before clearing them.')
            db.execute("UPDATE file_requests SET content='',before_content=NULL,path='',error=NULL,result=NULL WHERE event_id IN (SELECT event_id FROM events WHERE workspace=? AND channel=? AND thread=?)", args)
            db.execute("UPDATE events SET payload=json_set(payload,'$.text',''),result=NULL,decision='cleaned',reply_only=1 WHERE workspace=? AND channel=? AND thread=?", args)
            db.execute("UPDATE tasks SET control_state='cleaned',pause_reason=NULL,session=NULL WHERE workspace=? AND channel=? AND thread=?", args)
        else:
            raise ValueError('This action is not available for the current request state')
        db.execute('UPDATE tasks SET control_revision=control_revision+1 WHERE workspace=? AND channel=? AND thread=?', args)
        db.execute('INSERT INTO thread_decisions VALUES(?,?,?,?,?)', (*args, action, time.time()))
        db.commit()
    return {'saved': True, 'action': action}
