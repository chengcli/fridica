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
    roots = {config.root_label(p if config.remote else p.resolve())
             for p in (config.workspace, *config.additional_workspaces, *config.read_only_workspaces)}
    roots |= {host.label(p) for host in config.remote_hosts for p in host.roots}
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
    if db.execute("SELECT 1 FROM tasks WHERE workspace=? AND channel=? AND thread=? AND (continuation='pending' OR digest_pending=1)", args).fetchone():
        raise ValueError('This request is still processing its summary. Wait for it to settle.')
    if db.execute("SELECT 1 FROM events WHERE workspace=? AND channel=? AND thread=? AND state IN ('pending','running','ready','sending')", args).fetchone():
        raise ValueError('This request is still processing or delivering a reply. Wait for it to settle.')
    if files and db.execute("SELECT 1 FROM file_requests r JOIN events e ON r.event_id=e.event_id WHERE e.workspace=? AND e.channel=? AND e.thread=? AND (r.status IN ('pending','approved','applying','rejected') OR r.notified=0)", args).fetchone():
        raise ValueError('Resolve pending file operations and notifications before closing or cleaning this request.')


REPLAY_MARGIN = 0.001


def _unanswered(db, config, args):
    """The thread's latest message from someone else, when a blocked task turned it away; else None.

    Such a message was recorded without an agent run (``blocked`` or ``before_resume``) or answered
    only with the inspection notice. The owner's own messages never trigger a reply.
    """
    row = db.execute("SELECT event_id,timestamp,state,decision,reply_only FROM events WHERE workspace=? AND channel=? AND thread=? "
                     "AND json_extract(payload,'$.sender_id')!=? ORDER BY timestamp DESC LIMIT 1", (*args, config.owner_id)).fetchone()
    if row is None:
        return None
    if row['state'] == 'observed' and row['decision'] in ('blocked', 'before_resume') or row['state'] == 'sent' and row['reply_only']:
        return row
    return None


def _cleanup_preview(config, db, channel, thread):
    task = _thread(config, db, channel, thread)
    if task['control_state'] != 'archived':
        raise ValueError('Archive this request before clearing its contents')
    _idle(db, config, channel, thread)
    args = (config.workspace_id, channel, thread)
    notes = None
    if db.execute("SELECT 1 FROM sqlite_master WHERE name='collaboration'").fetchone():
        from .collaboration import key, snapshot
        lineage = key(db, config, channel, thread)
        if db.execute("SELECT 1 FROM tasks WHERE workspace=? AND channel=? AND COALESCE(root_thread,thread)=? AND thread!=? AND control_state NOT IN ('closed','archived','cleaned')", (*lineage, thread)).fetchone():
            raise ValueError('Close or archive the continuation before clearing shared task notes')
        notes = snapshot(db, config, channel, thread)
    events = [dict(r) for r in db.execute('SELECT * FROM events WHERE workspace=? AND channel=? AND thread=? ORDER BY event_id', args)]
    requests = [dict(r) for r in db.execute('SELECT r.* FROM file_requests r JOIN events e ON e.event_id=r.event_id WHERE e.workspace=? AND e.channel=? AND e.thread=? ORDER BY r.id', args)]
    raw = json.dumps([task, events, requests, notes], sort_keys=True).encode()
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
        replayed = False
        if action == 'resume' and (current == 'paused' or current == 'active' and task['status'] == 'blocked'):
            _idle(db, config, channel, thread, files=False)
            reset_at = time.time()
            # Resuming a blocked task answers the latest message it turned away while blocked, marked
            # 'resumed' so it is answered even when an agent posted it without a mention; a paused
            # task (loop protection) only accepts future messages.
            unanswered = _unanswered(db, config, args) if current == 'active' else None
            if unanswered is not None:
                reset_at = min(reset_at, unanswered['timestamp'] - REPLAY_MARGIN)
                db.execute("UPDATE events SET state='pending',decision='resumed',result=NULL,reply_only=0,retry_at=0,attempts=0 WHERE event_id=?",
                           (unanswered['event_id'],))
                replayed = True
            # A resumed thread gets a fresh budget and may be wrapped up again when it reaches the limit.
            db.execute("UPDATE tasks SET control_state='active',status='complete',pause_reason=NULL,reset_at=?,turns=0,session=NULL,continuation=NULL WHERE workspace=? AND channel=? AND thread=?", (reset_at, *args))
            if db.execute("SELECT 1 FROM sqlite_master WHERE name='collaboration'").fetchone():
                from .collaboration import key
                db.execute("UPDATE collaboration SET no_progress=0,last_reply='' WHERE workspace=? AND channel=? AND root_thread=?", key(db, config, channel, thread))
        elif action == 'close' and current in ('active','paused'):
            _idle(db, config, channel, thread)
            db.execute("UPDATE tasks SET control_state='closed',status='closed',session=NULL WHERE workspace=? AND channel=? AND thread=?", args)
        elif action == 'archive' and (current == 'closed' or current == 'active' and task['status'] == 'complete'
                                       or current == 'paused' and task.get('continuation') not in (None, 'pending', 'failed')):
            _idle(db, config, channel, thread)
            db.execute("UPDATE tasks SET control_state='archived' WHERE workspace=? AND channel=? AND thread=?", args)
        elif action == 'restore' and current == 'archived':
            db.execute("UPDATE tasks SET control_state=? WHERE workspace=? AND channel=? AND thread=?", ('closed' if task['status']=='closed' else 'active', *args))
        elif action == 'clean' and current == 'archived':
            preview = _cleanup_preview(config, db, channel, thread)
            if preview['revision'] != cleanup_revision:
                raise ValueError('Request contents changed. Preview again before clearing them.')
            if db.execute("SELECT 1 FROM sqlite_master WHERE name='collaboration'").fetchone():
                from .collaboration import key
                lineage = key(db, config, channel, thread)
                db.execute('DELETE FROM collaboration_history WHERE workspace=? AND channel=? AND root_thread=?', lineage)
                db.execute('DELETE FROM collaboration WHERE workspace=? AND channel=? AND root_thread=?', lineage)
                db.execute('UPDATE tasks SET session=NULL WHERE workspace=? AND channel=? AND COALESCE(root_thread,thread)=?', lineage)
            db.execute("UPDATE file_requests SET content='',before_content=NULL,path='',error=NULL,result=NULL WHERE event_id IN (SELECT event_id FROM events WHERE workspace=? AND channel=? AND thread=?)", args)
            db.execute("UPDATE events SET payload=json_set(payload,'$.text',''),result=NULL,decision='cleaned',reply_only=1 WHERE workspace=? AND channel=? AND thread=?", args)
            db.execute("UPDATE tasks SET control_state='cleaned',pause_reason=NULL,session=NULL WHERE workspace=? AND channel=? AND thread=?", args)
            if {'worker_thread', 'worker_state', 'worker_since'} <= {row[1] for row in db.execute('PRAGMA table_info(tasks)')}:
                db.execute("UPDATE tasks SET worker_thread=NULL,worker_state=NULL,worker_since=NULL WHERE workspace=? AND channel=? AND thread=?", args)
        else:
            raise ValueError('This action is not available for the current request state')
        db.execute('UPDATE tasks SET control_revision=control_revision+1 WHERE workspace=? AND channel=? AND thread=?', args)
        db.execute('INSERT INTO thread_decisions VALUES(?,?,?,?,?)', (*args, action, time.time()))
        db.commit()
    return {'saved': True, 'action': action, 'replayed': replayed}
