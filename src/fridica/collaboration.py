"""Local task notes and reply suppression. Reports never grant authority."""
from __future__ import annotations

from copy import deepcopy
from dataclasses import replace
import hashlib
import json
import re
import time

from .repos import load_repos

FIELDS = ('repo', 'assignee', 'next_step', 'blocker', 'unblock_when')
UPDATE_FIELDS = (*FIELDS, 'claim', 'source_event', 'corrects', 'kind')
KINDS = ('result', 'question', 'correction', 'status', 'ack')
LIMIT = 3


def initialize(db):
    db.executescript('''
        CREATE TABLE IF NOT EXISTS collaboration (
            workspace TEXT, channel TEXT, root_thread TEXT,
            data TEXT NOT NULL DEFAULT '{}', revision INTEGER NOT NULL DEFAULT 0,
            no_progress INTEGER NOT NULL DEFAULT 0, last_reply TEXT NOT NULL DEFAULT '',
            PRIMARY KEY(workspace,channel,root_thread)
        );
        CREATE TABLE IF NOT EXISTS collaboration_history (
            id INTEGER PRIMARY KEY, workspace TEXT, channel TEXT, root_thread TEXT,
            revision INTEGER, actor TEXT, source_event TEXT, data TEXT, created REAL
        );
    ''')


def key(db, config, channel, thread):
    if channel not in config.channels:
        raise ValueError('Channel is not configured')
    row = db.execute('SELECT root_thread FROM tasks WHERE workspace=? AND channel=? AND thread=?',
                     (config.workspace_id, channel, thread)).fetchone()
    if row is None:
        raise ValueError('Task not found')
    return config.workspace_id, channel, row['root_thread'] or thread


def snapshot(db, config, channel, thread):
    scope = key(db, config, channel, thread)
    row = db.execute('SELECT * FROM collaboration WHERE workspace=? AND channel=? AND root_thread=?', scope).fetchone()
    return {'data': json.loads(row['data']) if row else {}, 'revision': row['revision'] if row else 0,
            'no_progress': row['no_progress'] if row else 0, 'last_reply': row['last_reply'] if row else ''}


def _save(db, scope, current, data, actor, source_event):
    if current['data'] == data:
        return
    revision = current['revision'] + 1
    raw = json.dumps(data)
    db.execute('INSERT INTO collaboration(workspace,channel,root_thread,data,revision) VALUES(?,?,?,?,?) '
               'ON CONFLICT(workspace,channel,root_thread) DO UPDATE SET data=excluded.data,revision=excluded.revision',
               (*scope, raw, revision))
    db.execute('INSERT INTO collaboration_history(workspace,channel,root_thread,revision,actor,source_event,data,created) '
               'VALUES(?,?,?,?,?,?,?,?)', (*scope, revision, actor, source_event, raw, time.time()))


def validate(update):
    if not isinstance(update, dict) or set(update) - set(UPDATE_FIELDS):
        raise ValueError('Invalid task update')
    for field, value in update.items():
        if not isinstance(value, str) or len(value) > (1000 if field in ('claim', 'next_step', 'blocker', 'unblock_when') else 200):
            raise ValueError('Task update is too long or invalid')
    if update.get('kind', 'result') not in KINDS:
        raise ValueError('Invalid reply kind')
    return {field: value.strip() for field, value in update.items()}


def record(db, config, message, update):
    """Accept bounded proposals. Only literal, attributable message excerpts become reports."""
    update = validate(update)
    scope = key(db, config, message.channel_id, message.thread_id)
    current = snapshot(db, config, message.channel_id, message.thread_id)
    data = deepcopy(current['data'])
    repositories = {repo.name for repo in load_repos(config.repos)}
    if update.get('repo') and update['repo'] not in repositories:
        raise ValueError('Choose a repository from the shared list')
    if update.get('assignee'):
        known = db.execute("SELECT 1 FROM events WHERE workspace=? AND channel=? AND json_extract(payload,'$.sender_id')=? LIMIT 1",
                           (config.workspace_id, message.channel_id, update['assignee'])).fetchone()
        if update['assignee'] != config.owner_id and not known:
            raise ValueError('Use a known Slack member ID, not an inferred name')
    for field in FIELDS:
        if update.get(field) and field not in data.get('locked_fields', []):
            # Changing an established repo or assignee needs local review.
            if field in ('repo', 'assignee') and data.get(field) and data[field] != update[field]:
                continue
            data[field] = update[field]
    claims = data.setdefault('claims', [])
    new_evidence = False
    if update.get('claim'):
        source = db.execute('SELECT e.* FROM events e JOIN tasks t ON t.workspace=e.workspace AND t.channel=e.channel '
                            'AND t.thread=e.thread WHERE e.event_id=? AND e.workspace=? AND e.channel=? '
                            'AND COALESCE(t.root_thread,t.thread)=?',
                            (update.get('source_event'), *scope)).fetchone()
        payload = json.loads(source['payload']) if source else {}
        if update['claim'] not in payload.get('text', ''):
            raise ValueError('A report needs an exact excerpt from a message in this task')
        if payload['sender_id'] == config.owner_id and payload.get('generated'):
            raise ValueError('An own generated reply cannot be evidence of new progress')
        old = next((claim for claim in claims if claim['id'] == update.get('corrects')), None)
        if update.get('corrects') and old is None:
            raise ValueError('Correction target is not in this task')
        if old and old['state'] == 'superseded':
            raise ValueError('Correction target was superseded; use the current task notes')
        if not any(claim['text'] == update['claim'] for claim in claims):
            if len(claims) >= 30:
                raise ValueError('Review or clear task notes before adding more claims')
            identifier = hashlib.sha256((source['event_id'] + update['claim']).encode()).hexdigest()[:16]
            new_evidence = not any(claim.get('source_event') == source['event_id'] for claim in claims)
            claims.append({'id': identifier, 'text': update['claim'], 'basis': 'reported',
                           'source_event': source['event_id'], 'source_ts': payload['timestamp'],
                           'sender': payload['sender_id'],
                           'corrects': update.get('corrects', ''), 'state': 'disputed' if old else 'current'})
            if old:
                old['state'] = 'disputed'
    _save(db, scope, current, data, 'agent', message.event_id)
    return new_evidence


def prepare(db, config, message, result, *, file_operation=False):
    """Persist notes and suppress acknowledgments before a reply enters the outbox."""
    scope = key(db, config, message.channel_id, message.thread_id)
    before = snapshot(db, config, message.channel_id, message.thread_id)
    update = validate(result.update or {})
    with db:
        new_evidence = record(db, config, message, update) if result.update is not None else False
        normalized = ' '.join(result.text.casefold().split())
        digest = hashlib.sha256(normalized.encode()).hexdigest() if normalized else ''
        duplicate = bool(digest and digest == before['last_reply'])
        current = snapshot(db, config, message.channel_id, message.thread_id)
        changed = current['data'] != before['data']
        quiet = (not result.send or duplicate or update.get('kind') == 'ack'
                 or update.get('kind') == 'status' and not changed)
        # File proposals and completed file operations must always reach their requester.
        if file_operation:
            quiet = False
        progress = file_operation or new_evidence or not before['last_reply'] and not quiet and result.status == 'complete'
        stalled = 0 if progress else before['no_progress'] + 1
        # Legacy/custom backends do not provide state deltas; use exact repeats only.
        if result.update is None and not quiet and result.status != 'waiting':
            stalled = 0
        db.execute('INSERT INTO collaboration(workspace,channel,root_thread,no_progress,last_reply) VALUES(?,?,?,?,?) '
                   'ON CONFLICT(workspace,channel,root_thread) DO UPDATE SET no_progress=excluded.no_progress, '
                   'last_reply=excluded.last_reply', (*scope, stalled, before['last_reply'] if quiet else digest))
    if any(claim['state'] == 'disputed' for claim in current['data'].get('claims', [])):
        return replace(result, text=(result.text + '\n\n' if file_operation else '') + 'Conflicting task information needs local review before continuing.',
                       send=True, status='blocked', finished=False)
    return replace(result, send=not quiet, finished=result.finished and not quiet)


def pause_stalled(db, config, message):
    current = snapshot(db, config, message.channel_id, message.thread_id)
    if current['no_progress'] < LIMIT:
        return
    with db:
        db.execute("UPDATE tasks SET control_state='paused',pause_reason=?,control_revision=control_revision+1 "
                   "WHERE workspace=? AND channel=? AND COALESCE(root_thread,thread)=? AND control_state='active'",
                   (f'{LIMIT} turns without recorded progress; review before continuing.',
                    *key(db, config, message.channel_id, message.thread_id)))


def owner_update(config, channel, thread, expected, changes, correction=None):
    from .dashboard_control import database, _idle
    if not isinstance(changes, dict) or set(changes) - set(FIELDS):
        raise ValueError('Choose task fields to correct')
    validate(changes)
    with database(config, write=True) as db:
        scope = key(db, config, channel, thread)
        current = snapshot(db, config, channel, thread)
        if type(expected) is not int or current['revision'] != expected:
            raise ValueError('Task notes changed. Refresh before saving.')
        members = db.execute('SELECT thread,control_state FROM tasks WHERE workspace=? AND channel=? '
                             'AND COALESCE(root_thread,thread)=?', scope).fetchall()
        if any(row['control_state'] == 'cleaned' for row in members):
            raise ValueError('Task contents have been cleared')
        for row in members:
            _idle(db, config, channel, row['thread'])
        data = deepcopy(current['data'])
        repos = {repo.name for repo in load_repos(config.repos)}
        if changes.get('repo') and changes['repo'] not in repos:
            raise ValueError('Choose a repository from the shared list')
        if changes.get('assignee') and not re.fullmatch(r'[UW][A-Z0-9]+', changes['assignee']):
            raise ValueError('Use a Slack member ID for the task owner')
        data.update(changes)
        data['locked_fields'] = sorted(set(data.get('locked_fields', [])) | set(changes))
        if correction is not None:
            if not isinstance(correction, dict) or set(correction) != {'id', 'text', 'evidence'}:
                raise ValueError('A correction needs a claim, replacement and evidence')
            if any(not isinstance(v, str) or not v.strip() or len(v) > 1000 for v in correction.values()):
                raise ValueError('Give a concise correction and its evidence')
            claims = data.get('claims', [])
            old = next((claim for claim in claims if claim['id'] == correction['id'] and claim['state'] != 'superseded'), None)
            if old is None or len(claims) >= 30:
                raise ValueError('Claim is unavailable for correction')
            identifier = hashlib.sha256((str(current['revision']) + correction['text']).encode()).hexdigest()[:16]
            related = {old['id']}
            while True:
                linked = {value for claim in claims
                          if claim['id'] in related or claim.get('corrects') in related
                          for value in (claim['id'], claim.get('corrects')) if value}
                if linked <= related:
                    break
                related |= linked
            for claim in claims:
                if claim['id'] in related:
                    claim['state'] = 'superseded'
            claims.append({'id': identifier, 'text': correction['text'], 'basis': 'owner_confirmed',
                           'evidence': correction['evidence'], 'sender': config.owner_id, 'corrects': old['id'],
                           'state': 'current'})
        _save(db, scope, current, data, config.owner_id, None)
        db.execute('UPDATE tasks SET session=NULL,updated=? WHERE workspace=? AND channel=? AND COALESCE(root_thread,thread)=?', (time.time(), *scope))
        db.commit()
    return {'saved': True}
