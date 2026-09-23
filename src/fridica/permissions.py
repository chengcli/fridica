from __future__ import annotations

from contextlib import contextmanager
from dataclasses import asdict, replace
import json
import logging
import math
import os
from pathlib import Path
import re
import stat
import time
import uuid

from .config import within_casefold
from .models import AgentResult, Message


logger = logging.getLogger(__name__)

FILE_LIMIT = 65536
PROTECTED = {'.git', '.codex', '.claude', '.ssh', '.env'}


class Files:
    def __init__(self, config):
        self.writable = tuple(root.resolve(strict=True) for root in (config.workspace, *config.additional_workspaces))
        self.read_only = tuple(root.resolve(strict=True) for root in config.read_only_workspaces)

    def path(self, value: str, *, write: bool = False) -> Path:
        path = Path(value)
        if not path.is_absolute() or '..' in path.parts or any(part.casefold() in PROTECTED for part in path.parts):
            raise ValueError('Use an absolute path inside a configured file access root')
        roots = self.writable if write else (*self.writable, *self.read_only)
        if not any(path.is_relative_to(root) for root in roots):
            raise ValueError('Path is outside the configured file access roots')
        if write and any(within_casefold(path, root) for root in self.read_only):
            raise ValueError('Path is read-only')
        return path

    @contextmanager
    def parent(self, value: str, *, write: bool = False):
        path = self.path(value, write=write)
        descriptor = os.open('/', os.O_RDONLY | os.O_DIRECTORY)
        try:
            for part in path.parts[1:-1]:
                child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
                os.close(descriptor)
                descriptor = child
            yield descriptor, path.name
        finally:
            os.close(descriptor)

    @staticmethod
    def contents(parent: int, name: str) -> tuple[str | None, int]:
        try:
            descriptor = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=parent)
        except FileNotFoundError:
            return None, 0o644
        with os.fdopen(descriptor, 'rb') as stream:
            metadata = os.fstat(stream.fileno())
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
                raise ValueError('Only regular files without hard links are supported')
            data = stream.read(FILE_LIMIT + 1)
        if len(data) > FILE_LIMIT or b'\0' in data:
            raise ValueError('Only UTF-8 text files up to 64 KiB are supported')
        return data.decode('utf-8'), stat.S_IMODE(metadata.st_mode) & 0o777

    def read(self, value: str) -> str:
        with self.parent(value) as (parent, name):
            text, _mode = self.contents(parent, name)
        if text is None:
            raise ValueError('File does not exist')
        return text

    def snapshot(self, value: str) -> str | None:
        with self.parent(value, write=True) as (parent, name):
            return self.contents(parent, name)[0]

    def apply(self, request) -> None:
        with self.parent(request['path'], write=True) as (parent, name):
            before, mode = self.contents(parent, name)
            if before != request['before_content']:
                raise ValueError('File changed after the request was prepared')
            if request['operation'] == 'delete':
                if before is None:
                    raise ValueError('File does not exist')
                os.unlink(name, dir_fd=parent)
            elif request['operation'] == 'write':
                data = request['content'].encode('utf-8')
                if len(data) > FILE_LIMIT or b'\0' in data:
                    raise ValueError('Only UTF-8 text files up to 64 KiB are supported')
                temporary = '.fridica-' + uuid.uuid4().hex
                descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                                     mode, dir_fd=parent)
                try:
                    with os.fdopen(descriptor, 'wb') as stream:
                        stream.write(data)
                        stream.flush()
                        os.fsync(stream.fileno())
                    os.replace(temporary, name, src_dir_fd=parent, dst_dir_fd=parent)
                finally:
                    try:
                        os.unlink(temporary, dir_fd=parent)
                    except FileNotFoundError:
                        pass
            else:
                raise ValueError('Unsupported file operation')
            os.fsync(parent)


class Permissions:
    def __init__(self, config, store, agent=None):
        self.config = config
        self.store = store
        self.agent = agent
        self.files = Files(config)
        self.db = store.connection

    def grant(self, sender: str, channel: str, path: str, ttl: float | None = None) -> str:
        if not re.fullmatch(r'[UW][A-Z0-9]+', sender) or channel not in self.config.channels:
            raise ValueError('Grant must name a Slack sender and a configured channel')
        if ttl is not None and (not math.isfinite(ttl) or ttl <= 0):
            raise ValueError('Grant duration must be positive and finite')
        target = self.files.path(path, write=True)
        # Check every existing path component before recording the grant.
        with self.files.parent(str(target / '.fridica-probe') if target.is_dir() else str(target), write=True):
            pass
        identifier = uuid.uuid4().hex
        with self.db:
            self.db.execute('INSERT INTO grants VALUES(?,?,?,?,?,0)',
                            (identifier, sender, channel, str(target), time.time() + ttl if ttl else None))
        return identifier

    def revoke(self, identifier: str) -> None:
        with self.db:
            changed = self.db.execute('UPDATE grants SET revoked=1 WHERE id=? AND revoked=0', (identifier,))
            if not changed.rowcount:
                raise ValueError('Active grant not found')

    def allowed(self, message: Message, path: str) -> bool:
        target = self.files.path(path, write=True)
        return any(target.is_relative_to(Path(row['path'])) for row in self.db.execute(
            'SELECT path FROM grants WHERE sender=? AND channel=? AND revoked=0 AND (expires IS NULL OR expires>?)',
            (message.sender_id, message.channel_id, time.time()),
        ))

    def decide(self, identifier: str, status: str) -> None:
        if status not in {'approved', 'rejected'}:
            raise ValueError('Choose approve or reject')
        with self.db:
            changed = self.db.execute("UPDATE file_requests SET status=? WHERE id=? AND status='pending'",
                                      (status, identifier))
            if not changed.rowcount:
                raise ValueError('Pending request not found')

    def awaiting(self, message: Message) -> bool:
        return self.db.execute(
            "SELECT 1 FROM file_requests r JOIN events e ON r.event_id=e.event_id "
            "WHERE e.workspace=? AND e.channel=? AND e.thread=? AND r.status IN ('pending','approved','rejected')",
            (message.workspace_id, message.channel_id, message.thread_id),
        ).fetchone() is not None

    async def respond(self, message, context) -> AgentResult:
        files = {}
        roots = {'writable': [str(root) for root in self.files.writable],
                 'read_only': [str(root) for root in self.files.read_only]}
        try:
            for _ in range(8):
                plan = await self.agent.plan(message, context, files, roots)
                if not isinstance(plan, dict) or set(plan) - {'operation', 'path', 'content', 'text', 'update'} or not {'operation', 'path', 'content', 'text'} <= set(plan):
                    raise ValueError('Invalid file plan')
                if any(not isinstance(plan[field], str) for field in ('operation', 'path', 'content', 'text')):
                    raise ValueError('Invalid file plan')
                from .collaboration import validate
                update = validate(plan['update']) if 'update' in plan else None
                operation = plan['operation']
                if operation == 'observe':
                    return AgentResult('', send=False, update=update)
                if operation in {'reply', 'clarify'}:
                    if not plan['text'].strip() or len(plan['text']) > 3500:
                        raise ValueError('Invalid reply')
                    return AgentResult(plan['text'], 'waiting' if operation == 'clarify' else 'complete', update=update)
                if operation == 'escalate':
                    return self._escalation(plan, update)
                if operation == 'read':
                    files[plan['path']] = self.files.read(plan['path'])
                    continue
                if operation not in {'write', 'delete'}:
                    return AgentResult('This operation requires local handling; file access mode cannot run it.', 'blocked', update=update)
                if len(plan['content'].encode('utf-8')) > FILE_LIMIT or '\0' in plan['content']:
                    raise ValueError('File content exceeds the text limit')
                before = self.files.snapshot(plan['path'])
                if files.get(plan['path']) != before:
                    raise ValueError('Read the current file before proposing a change')
                if operation == 'delete' and before is None:
                    raise ValueError('File does not exist')
                identifier = uuid.uuid4().hex
                with self.db:
                    self.db.execute(
                        'INSERT INTO file_requests(id,event_id,sender,channel,operation,path,content,before_content,created) '
                        'VALUES(?,?,?,?,?,?,?,?,?)',
                        (identifier, message.event_id, message.sender_id, message.channel_id, operation,
                         plan['path'], plan['content'], before, time.time()),
                    )
                if operation == 'write' and self.allowed(message, plan['path']):
                    return replace(self.apply(identifier, automatic=True), update=update)
                return AgentResult(f'File request {identifier} is waiting for local approval.', 'waiting', update=update)
            return AgentResult('The file request needs more local context before I can continue.', 'blocked')
        except (ValueError, OSError) as error:
            logger.warning('File plan for event %s was rejected (%s): %s', message.event_id, type(error).__name__, error)
            return AgentResult('The file request could not pass the local access checks. No change was applied.', 'blocked')

    def _escalation(self, plan: dict, update) -> AgentResult:
        """A heavy-task hand-off: the brief runs on a remote host's worker; the local roots stay scoped."""
        from .prompts import ESCALATE_LIMIT
        brief, host, text = plan['content'].strip(), plan['path'].strip(), plan['text']
        if not text.strip() or len(text) > 3500 or not brief or len(brief) > ESCALATE_LIMIT:
            raise ValueError('Invalid heavy-task brief')
        candidates = [candidate.name for candidate in self.config.heavy_hosts]
        if not self.config.heavy_tasks or not candidates:
            logger.warning('Planner asked to escalate a heavy task while heavy_tasks is disabled; ignoring the brief')
            return AgentResult(text, 'complete', update=update)
        if host and host not in candidates:
            logger.warning('Planner asked to escalate to unknown host %r; running the job on %s instead', host[:80], candidates[0])
            host = ''
        return AgentResult(text, 'complete', update=update, escalate=brief, escalate_host='' if host == candidates[0] else host)

    def apply(self, identifier: str, *, automatic: bool = False) -> AgentResult:
        self.db.execute('BEGIN IMMEDIATE')
        try:
            request = self.db.execute('SELECT * FROM file_requests WHERE id=?', (identifier,)).fetchone()
            if request is None or request['status'] != ('pending' if automatic else 'approved'):
                raise ValueError('Request is not available for execution')
            if request['channel'] not in self.config.channels:
                raise ValueError('Request channel is no longer allowed')
            if automatic:
                event = self.store.get(request['event_id'])
                message = Message(**json.loads(event['payload']))
                if request['operation'] != 'write' or not self.allowed(message, request['path']):
                    raise ValueError('Write grant is no longer active')
            self.db.execute("UPDATE file_requests SET status='applying' WHERE id=?", (identifier,))
            self.db.commit()
        except BaseException:
            self.db.rollback()
            raise
        try:
            self.files.apply(request)
        except (ValueError, OSError) as error:
            status, detail = 'failed', type(error).__name__
            result = AgentResult('The approved file operation failed. Inspect it locally before retrying.', 'blocked')
        else:
            status, detail = 'complete', None
            verb = 'Deleted' if request['operation'] == 'delete' else 'Updated'
            result = AgentResult(f'{verb} the file for request {identifier}.')
        with self.db:
            self.db.execute('UPDATE file_requests SET status=?,error=?,result=? WHERE id=?',
                            (status, detail, json.dumps(asdict(result)), identifier))
        return result

    async def process_approved(self, replica) -> None:
        rows = self.db.execute(
            "SELECT r.* FROM file_requests r JOIN events e ON r.event_id=e.event_id "
            "WHERE r.status IN ('approved','rejected','complete','failed','interrupted') "
            "AND r.notified=0 AND e.state IN ('sent','interrupted','failed','ambiguous') ORDER BY r.created LIMIT 100"
        ).fetchall()
        for request in rows:
            message = Message(**json.loads(self.store.get(request['event_id'])['payload']))
            if message.workspace_id != self.config.workspace_id or message.channel_id not in self.config.channels:
                continue
            task = self.store.task(message)
            if task and task['control_state'] != 'active':
                continue
            if request['status'] == 'rejected':
                result = AgentResult('The file request was declined locally.', 'complete')
            elif request['status'] == 'approved':
                result = self.apply(request['id'])
            elif request['result']:
                result = AgentResult(**json.loads(request['result']))
            else:
                result = AgentResult('The file operation was interrupted. Inspect it locally before retrying.', 'blocked')
            with self.db:
                self.db.execute("UPDATE file_requests SET status=CASE WHEN status='rejected' "
                                "THEN 'declined' ELSE status END WHERE id=?", (request['id'],))
                self.store.save_result(message, result)
            await replica._deliver(message)
