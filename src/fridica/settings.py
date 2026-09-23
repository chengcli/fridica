from __future__ import annotations

from dataclasses import fields
import hashlib
import json
import os
from pathlib import Path
import re
import tempfile
import time
from types import SimpleNamespace

import tomlkit

from .config import load_config
from .dashboard_control import database
from .permissions import Files, Permissions

EDITABLE = {'model', 'reasoning_effort', 'max_wait_replies', 'max_turns',
            'workspace', 'additional_workspaces', 'read_only_workspaces'}
DIRECTORIES = {'workspace', 'additional_workspaces', 'read_only_workspaces'}


def configured_snapshot(base, path):
    contents = path.read_bytes()
    current = load_config(path, contents=contents)
    if any(getattr(current, field.name) != getattr(base, field.name)
           for field in fields(base) if field.name not in EDITABLE):
        raise ValueError('Other configuration changed. Restart Fridica to use it.')
    return current, hashlib.sha256(contents).hexdigest()


def configured(base, path):
    return configured_snapshot(base, path)[0]


def fingerprint(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def read_settings(base, path):
    if path is None:
        raise ValueError('Start the monitor with a configuration file to edit settings.')
    current, revision = configured_snapshot(base, path)
    values = {name: getattr(current, name) for name in EDITABLE}
    values.update(workspace=current.root_label(current.workspace),
                  additional_workspaces=[current.root_label(p) for p in current.additional_workspaces],
                  read_only_workspaces=[current.root_label(p) for p in current.read_only_workspaces])
    result = {'values': values, 'revision': revision, 'applied_revision': None, 'applied_pid': None}
    with database(base) as db:
        if db.execute("SELECT 1 FROM sqlite_master WHERE name='configuration_runtime'").fetchone():
            row = db.execute('SELECT revision,pid FROM configuration_runtime WHERE id=1').fetchone()
            if row:
                result.update(applied_revision=row['revision'], applied_pid=row['pid'])
    return result


def audit(db, owner, action, details):
    db.execute('CREATE TABLE IF NOT EXISTS configuration_audit (time REAL,owner TEXT,action TEXT,details TEXT)')
    db.execute('INSERT INTO configuration_audit VALUES(?,?,?,?)', (time.time(),owner,action,json.dumps(details)))


def save_settings(base, path, changes, expected):
    if path is None or path.is_symlink():
        raise ValueError('Use a regular configuration file.')
    if not isinstance(changes, dict) or not changes or set(changes)-EDITABLE:
        raise ValueError('Choose supported settings to change.')
    if set(changes) & DIRECTORIES and not base.file_access:
        raise ValueError('Directory editing requires managed file access mode.')
    if 'model' in changes and changes['model'] is not None and (not isinstance(changes['model'],str) or not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._:-]{0,99}',changes['model'])):
        raise ValueError('Enter a model ID, such as gpt-5.6-luna.')
    for name in ('max_wait_replies','max_turns'):
        if name in changes and (type(changes[name]) is not int or not 1 <= changes[name] <= 100):
            raise ValueError('Loop limits must be whole numbers from 1 to 100.')
    if 'reasoning_effort' in changes and changes['reasoning_effort'] not in {None,'low','medium','high','xhigh','max','ultra'}:
        raise ValueError('Choose a reasoning effort.')
    for name in DIRECTORIES & changes.keys():
        values = [changes[name]] if name=='workspace' else changes[name]
        if not isinstance(values,list) or len(values)>20:
            raise ValueError('Use a list of at most 20 directories.')
        for value in values:
            if not isinstance(value,str) or not Path(value).is_absolute():
                raise ValueError('Enter an existing absolute directory path.')
            target=Path(value)
            if '..' in target.parts or target.resolve()!=target or not target.is_dir():
                raise ValueError('Directories must exist and must not use symlinks or parent traversal.')
    temporary=None
    with database(base,write=True) as db:
        if expected != fingerprint(path):
            raise ValueError('Settings changed. Reload the form before saving.')
        configured(base,path)
        document=tomlkit.parse(path.read_text())
        for name,value in changes.items():
            if value is None and name in {'model','reasoning_effort'}:
                document.pop(name,None)
            else:
                document[name]=value
        try:
            with tempfile.NamedTemporaryFile(mode='w',dir=path.parent,prefix='.fridica-settings-',delete=False) as stream:
                temporary=Path(stream.name)
                stream.write(tomlkit.dumps(document));stream.flush();os.fsync(stream.fileno())
            current=configured(base,temporary)
            roots=(current.workspace,*current.additional_workspaces,*current.read_only_workspaces)
            if len(set(roots))!=len(roots):
                raise ValueError('Each directory should appear only once.')
            if current.file_access:
                files=Files(current)
                for root in roots:
                    with files.parent(str(root/'.fridica-probe')): pass
            if expected != fingerprint(path):
                raise ValueError('Settings changed. Reload the form before saving.')
            if set(changes)&DIRECTORIES:
                marks = ','.join('?' for _ in current.channels)
                for grant in db.execute(f'SELECT id,path FROM grants WHERE revoked=0 AND channel IN ({marks})', current.channels).fetchall():
                    try: files.path(grant['path'],write=True)
                    except ValueError: db.execute('UPDATE grants SET revoked=1 WHERE id=?',(grant['id'],))
            audit(db,base.owner_id,'settings',changes)
            os.replace(temporary,path)
            db.commit()
        finally:
            if temporary is not None: temporary.unlink(missing_ok=True)
    return read_settings(base,path)


def update_grant(config, body, config_path=None):
    if not config.file_access or not isinstance(body,dict):
        raise ValueError('Managed file access is required.')
    with database(config,write=True) as db:
        if config_path is not None:
            config = configured(config, config_path)
        permissions=Permissions(config,SimpleNamespace(connection=db))
        if body.get('action')=='grant':
            if not isinstance(body.get('path'),str) or not isinstance(body.get('sender'),str):
                raise ValueError('Choose a requester and a path.')
            ttl=body.get('ttl')
            if ttl is not None and (isinstance(ttl,bool) or not isinstance(ttl,(int,float))):
                raise ValueError('Choose a valid grant duration.')
            identifier=permissions.grant(body['sender'],body.get('channel'),body['path'],ttl)
        elif body.get('action')=='revoke':
            identifier=body.get('id')
            row=db.execute('SELECT channel FROM grants WHERE id=?',(identifier,)).fetchone()
            if row is None or row['channel'] not in config.channels:
                raise ValueError('Grant is not available in this channel.')
            permissions.revoke(identifier)
        else: raise ValueError('Choose grant or revoke.')
        audit(db,config.owner_id,body['action'],{'id':identifier})
        db.commit()
    return {'id':identifier,'status':'active' if body['action']=='grant' else 'revoked'}
