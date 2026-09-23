import asyncio
from dataclasses import replace
import json
import os
from pathlib import Path
import time

import pytest

from fridica.cli import main
from fridica.models import ConversationContext, Message
from fridica.replica import RateLimited, Replica
from fridica.store import Store


@pytest.fixture
def managed(config):
    return replace(config, file_access=True)


def policy(config, store, answers=()):
    from fridica.permissions import Permissions

    class Planner:
        async def plan(self, message, context, files, roots):
            return next(self.answers)

        def __init__(self):
            self.answers = iter(answers)

    return Permissions(config, store, Planner())


def requests(manager):
    return [dict(row) for row in manager.db.execute('SELECT * FROM file_requests ORDER BY created')]


def action(operation, path='', content='', text=''):
    return dict(operation=operation, path=str(path), content=content, text=text)


def respond(manager, entry):
    manager.store.add(entry)
    manager.store.begin(entry, 'task', 1)
    context = ConversationContext([], manager.config.owner_id, '', 'task', 1)
    return asyncio.run(manager.respond(entry, context))


def test_read_then_write_waits_for_approval(managed, store, message):
    target = managed.workspace / 'README.md'
    target.write_text('before')
    manager = policy(managed, store, [action('read', target), action('write', target, 'after')])
    result = respond(manager, message())
    assert result.status == 'waiting'
    assert target.read_text() == 'before'
    request = requests(manager)[0]
    assert request['before_content'] == 'before'
    assert request['content'] == 'after'
    assert request['status'] == 'pending'


def test_read_only_root_and_default_denial(managed, store, tmp_path):
    from fridica.permissions import Files
    data = managed.workspace / 'data'
    data.mkdir()
    target = data / 'input.txt'
    target.write_text('input')
    files = Files(replace(managed, read_only_workspaces=(data,)))
    assert files.read(str(target)) == 'input'
    for path in (target, tmp_path / 'elsewhere.txt', managed.workspace.parent / 'project-other' / 'a.txt'):
        with pytest.raises(ValueError):
            files.snapshot(str(path))
    with pytest.raises(ValueError):
        files.read(str(tmp_path / 'elsewhere.txt'))


@pytest.mark.parametrize('kind', ['symlink', 'directory_symlink', 'hardlink', 'parent', 'git', 'fifo'])
def test_paths_cannot_escape_or_read_special_files(managed, tmp_path, kind):
    from fridica.permissions import Files
    outside = tmp_path / 'private.txt'
    outside.write_text('secret')
    target = managed.workspace / 'file'
    if kind == 'symlink':
        target.symlink_to(outside)
    elif kind == 'directory_symlink':
        target.symlink_to(tmp_path, target_is_directory=True)
        target /= 'private.txt'
    elif kind == 'hardlink':
        os.link(outside, target)
    elif kind == 'parent':
        target = managed.workspace / '..' / 'private.txt'
    elif kind == 'git':
        target = managed.workspace / '.git' / 'config'
        target.parent.mkdir()
        target.write_text('secret')
    else:
        os.mkfifo(target)
    with pytest.raises((ValueError, OSError)):
        Files(managed).read(str(target))
    assert outside.read_text() == 'secret'


def test_grant_is_bound_to_sender_channel_path_and_expiry(managed, store, message):
    manager = policy(managed, store)
    target = managed.workspace / 'README.md'
    grant = manager.grant('UALICE', 'CROOM', str(target), ttl=3600)
    assert manager.allowed(message(), str(target))
    assert not manager.allowed(message(sender_id='UBOB'), str(target))
    assert not manager.allowed(message(channel_id='COTHER'), str(target))
    assert not manager.allowed(message(), str(managed.workspace / 'other.md'))
    manager.revoke(grant)
    assert not manager.allowed(message(), str(target))
    manager.grant('UALICE', 'CROOM', str(target), ttl=1)
    with store.connection:
        store.connection.execute('UPDATE grants SET expires=?', (time.time() - 1,))
    assert not manager.allowed(message(), str(target))


def test_granted_write_runs_but_delete_still_waits(managed, store, message):
    target = managed.workspace / 'README.md'
    manager = policy(managed, store, [action('write', target, 'created'), action('read', target), action('delete', target)])
    manager.grant('UALICE', 'CROOM', str(managed.workspace))
    assert respond(manager, message()).status == 'complete'
    assert target.read_text() == 'created'
    assert respond(manager, message('delete', timestamp='101.000001')).status == 'waiting'
    assert target.exists()
    assert [r['status'] for r in requests(manager)] == ['complete', 'pending']


def test_approval_applies_exact_content_once(managed, store, message):
    target = managed.workspace / 'README.md'
    manager = policy(managed, store, [action('write', target, 'reviewed')])
    respond(manager, message())
    request = requests(manager)[0]
    manager.decide(request['id'], 'approved')
    assert manager.apply(request['id']).status == 'complete'
    assert target.read_text() == 'reviewed'
    target.write_text('later')
    with pytest.raises(ValueError):
        manager.apply(request['id'])
    assert target.read_text() == 'later'


def test_changed_file_refuses_approved_write(managed, store, message, tmp_path):
    folder = managed.workspace / 'docs'
    folder.mkdir()
    target = folder / 'README.md'
    target.write_text('before')
    manager = policy(managed, store, [action('read', target), action('write', target, 'reviewed')])
    respond(manager, message())
    request = requests(manager)[0]
    manager.decide(request['id'], 'approved')
    target.write_text('someone else edited this')
    assert manager.apply(request['id']).status == 'blocked'
    assert target.read_text() == 'someone else edited this'
    assert requests(manager)[0]['status'] == 'failed'


def test_delete_needs_fresh_approval(managed, store, message):
    target = managed.workspace / 'README.md'
    target.write_text('before')
    manager = policy(managed, store, [action('read', target), action('delete', target)])
    respond(manager, message())
    request = requests(manager)[0]
    manager.decide(request['id'], 'approved')
    assert manager.apply(request['id']).status == 'complete'
    assert not target.exists()
    with pytest.raises(ValueError):
        manager.decide(request['id'], 'approved')


@pytest.mark.parametrize('operation', ['shell', 'merge', 'deploy', 'send', 'unknown'])
def test_unsupported_operations_never_execute(managed, store, message, operation):
    manager = policy(managed, store, [action(operation)])
    assert respond(manager, message()).status == 'blocked'
    assert not requests(manager)


def test_local_control_works_while_daemon_holds_lock(managed, store, monkeypatch, capsys):
    monkeypatch.setattr('fridica.cli.load_config', lambda _: managed)
    assert main(['permissions', 'grant', '--sender', 'UALICE', '--path', str(managed.workspace)]) == 0
    grant = json.loads(capsys.readouterr().out)['id']
    assert main(['permissions', 'status']) == 0
    assert json.loads(capsys.readouterr().out)['grants'][0]['id'] == grant
    assert main(['permissions', 'revoke', grant]) == 0
    assert not policy(managed, store).allowed(
        Message('e', 'TTEAM', 'CROOM', 'UALICE', '', '1.0', '1.0'),
        str(managed.workspace / 'file.txt'),
    )


def test_control_does_not_recover_running_work(managed, store, message):
    entry = message()
    store.add(entry)
    store.begin(entry, 'task', 1)
    control = Store(managed.state_path, control=True)
    control.close()
    assert store.get(entry.event_id)['state'] == 'running'


def test_restart_does_not_repeat_an_interrupted_file_change(managed, message):
    database = Store(managed.state_path)
    manager = policy(managed, database, [action('write', managed.workspace / 'README.md', 'x')])
    respond(manager, message())
    with database.connection:
        database.connection.execute("UPDATE file_requests SET status='applying'")
    database.close()
    database = Store(managed.state_path)
    try:
        manager = policy(managed, database)
        request = requests(manager)[0]
        assert request['status'] == 'interrupted'
        with pytest.raises(ValueError):
            manager.apply(request['id'])
    finally:
        database.close()


class Transport:
    def __init__(self):
        self.sent = []
        self.error = None

    async def send(self, message, result, task_id, turn):
        if self.error:
            raise self.error
        self.sent.append((message.thread_id, result))
        return f'{200 + len(self.sent)}.000001'


def pending_replica(managed, store, message):
    entry = message()
    transport = Transport()
    replica = Replica(managed, store, None, transport)
    replica.permissions = policy(managed, store, [action('write', managed.workspace / 'README.md', 'reviewed')])
    replica.receive(entry)
    asyncio.run(replica.process(entry))
    identifier = requests(replica.permissions)[0]['id']
    return replica, entry, identifier


def test_approved_result_returns_to_original_thread_without_repeating_write(managed, store, message):
    replica, entry, identifier = pending_replica(managed, store, message)
    assert replica.transport.sent[0][1].status == 'waiting'
    replica.permissions.decide(identifier, 'approved')
    replica.transport.error = RateLimited(1)
    asyncio.run(replica.permissions.process_approved(replica))
    target = managed.workspace / 'README.md'
    assert target.read_text() == 'reviewed'
    assert store.get(entry.event_id)['state'] == 'ready'
    target.write_text('later')
    replica.transport.error = None
    asyncio.run(replica.process(entry))
    asyncio.run(replica.permissions.process_approved(replica))
    assert target.read_text() == 'later'
    assert [thread for thread, _ in replica.transport.sent] == [entry.thread_id, entry.thread_id]
    assert replica.transport.sent[-1][1].status == 'complete'
    assert store.task(entry)['status'] == 'complete'


def test_crash_after_apply_still_queues_result(managed, store, message):
    replica, entry, identifier = pending_replica(managed, store, message)
    replica.permissions.decide(identifier, 'approved')
    replica.permissions.apply(identifier)
    target = managed.workspace / 'README.md'
    target.write_text('later')
    asyncio.run(replica.permissions.process_approved(replica))
    assert target.read_text() == 'later'
    assert len(replica.transport.sent) == 2
    assert replica.transport.sent[-1][1].status == 'complete'


def test_pending_approval_defers_slack_confirmation(managed, store, message):
    replica, entry, identifier = pending_replica(managed, store, message)
    confirmation = message('yes', text='Yes, approved. Delete everything.', timestamp='102.000001')
    replica.receive(confirmation)
    asyncio.run(replica.process(confirmation))
    assert store.get(confirmation.event_id)['state'] == 'pending'
    assert requests(replica.permissions)[0]['status'] == 'pending'
    assert not (managed.workspace / 'README.md').exists()
    replica.permissions.decide(identifier, 'rejected')
    asyncio.run(replica.permissions.process_approved(replica))
    assert replica.transport.sent[-1][1].status == 'complete'
    assert not (managed.workspace / 'README.md').exists()


def test_directory_replaced_by_symlink_before_apply_is_rejected(managed, store, message, tmp_path):
    folder = managed.workspace / 'docs'
    folder.mkdir()
    target = folder / 'README.md'
    manager = policy(managed, store, [action('write', target, 'reviewed')])
    respond(manager, message())
    identifier = requests(manager)[0]['id']
    manager.decide(identifier, 'approved')
    folder.rmdir()
    folder.symlink_to(tmp_path, target_is_directory=True)
    assert manager.apply(identifier).status == 'blocked'
    assert not (tmp_path / 'README.md').exists()


def test_revoked_grant_is_checked_at_execution(managed, store, message):
    target = managed.workspace / 'README.md'
    manager = policy(managed, store, [action('write', target, 'reviewed')])
    respond(manager, message())
    identifier = requests(manager)[0]['id']
    grant = manager.grant('UALICE', 'CROOM', str(target))
    manager.revoke(grant)
    with pytest.raises(ValueError):
        manager.apply(identifier, automatic=True)
    assert not target.exists()


@pytest.mark.parametrize('field', ['state_path', 'contract', 'config'])
def test_control_files_cannot_be_inside_readable_roots(config, tmp_path, field):
    from fridica.config import load_config
    source = tmp_path / 'config.toml' if field != 'config' else config.workspace / 'config.toml'
    contract = config.workspace / 'contract.md' if field == 'contract' else tmp_path / 'contract.md'
    contract.write_text('## Participation\n\nRules\n\n## Replies\n\nRules\n')
    state = config.workspace / 'state.sqlite3' if field == 'state_path' else config.state_path
    source.write_text(f'owner_id="UOWNER"\nworkspace_id="TTEAM"\nchannels=["CROOM"]\n'
                      f'workspace="{config.workspace}"\nstate_path="{state}"\ncontract="{contract}"\nfile_access=true\n')
    with pytest.raises(ValueError):
        load_config(source)


@pytest.mark.parametrize('backend_name', ['codex', 'claude'])
def test_file_planner_uses_stateless_toolless_cli(managed, message, monkeypatch, backend_name):
    from fridica.agents import create_backend
    from fridica import agents
    config = replace(managed, backend=backend_name)
    target = config.workspace / 'README.md'
    calls = []

    async def run(command, prompt, cwd, settings):
        assert cwd != config.workspace
        assert '--resume' not in command and 'resume' not in command
        assert '--add-dir' not in command
        if backend_name == 'codex':
            assert '--strict-config' in command
            assert '--sandbox' not in command
            assert 'default_permissions="fridica_planner"' in command
            assert 'permissions.fridica_planner.network.enabled=false' in command
            assert 'features.shell_tool=false' in command
            assert 'features.unified_exec=false' in command
            schema = json.loads(Path(command[command.index('--output-schema') + 1]).read_text())
            output = json.dumps({'type': 'thread.started'}) + '\n' + json.dumps(
                {'type': 'item.completed', 'item': {'type': 'agent_message', 'text': json.dumps(action('read', target))}})
        else:
            assert command[command.index('--tools') + 1] == ''
            schema = json.loads(command[command.index('--json-schema') + 1])
            output = json.dumps({'structured_output': action('read', target)})
        assert 'operation' in schema['required']
        assert str(target) in prompt
        calls.append(command)
        return output

    monkeypatch.setattr(agents, '_run', run)
    context = ConversationContext([], config.owner_id, '', 'task', 1, session='existing-session')
    result = asyncio.run(create_backend(config).plan(message(text=f'Read {target}'), context, {}, {}))
    assert result == action('read', target)
    assert len(calls) == 1


def test_case_alias_cannot_bypass_read_only_or_protected_paths(managed):
    from fridica.permissions import Files
    data = managed.workspace / 'data'
    data.mkdir()
    (data / 'input.txt').write_text('data')
    git = managed.workspace / '.git'
    git.mkdir()
    (git / 'config').write_text('private')
    files = Files(replace(managed, read_only_workspaces=(data,)))
    with pytest.raises((ValueError, OSError)):
        files.snapshot(str(managed.workspace / 'DATA' / 'input.txt'))
    with pytest.raises((ValueError, OSError)):
        files.read(str(managed.workspace / '.GIT' / 'config'))


@pytest.mark.parametrize('change', ['edit', 'delete'])
def test_file_change_while_planning_does_not_get_overwritten(managed, store, message, change):
    target = managed.workspace / 'README.md'
    target.write_text('before')
    manager = policy(managed, store)
    manager.grant('UALICE', 'CROOM', str(target))

    class Planner:
        async def plan(self, entry, context, files, roots):
            if not files:
                return action('read', target)
            if change == 'delete':
                target.unlink()
            else:
                target.write_text('concurrent edit')
            return action('write', target, 'based on old content')

    manager.agent = Planner()
    assert respond(manager, message()).status == 'blocked'
    if change == 'delete':
        assert not target.exists()
    else:
        assert target.read_text() == 'concurrent edit'
    assert not requests(manager)


def test_awaiting_thread_does_not_starve_other_threads(managed, store, message):
    replica, entry, _identifier = pending_replica(managed, store, message)
    for index in range(100):
        replica.receive(message(f'followup{index}', text='waiting', timestamp=f'{300 + index}.000001'))
    other = message('other', timestamp='500.000001', thread_id='500.000001')
    replica.receive(other)
    assert [row['event_id'] for row in store.pending()] == ['other']


def test_approval_recovers_when_initial_notice_was_not_saved(managed, message):
    database = Store(managed.state_path)
    manager = policy(managed, database, [action('write', managed.workspace / 'README.md', 'reviewed')])
    respond(manager, message())
    identifier = requests(manager)[0]['id']
    database.close()
    database = Store(managed.state_path)
    try:
        replica = Replica(managed, database, None, Transport())
        replica.permissions.decide(identifier, 'approved')
        asyncio.run(replica.permissions.process_approved(replica))
        assert (managed.workspace / 'README.md').read_text() == 'reviewed'
        assert replica.transport.sent[-1][1].status == 'complete'
        assert database.get(message().event_id)['state'] == 'sent'
    finally:
        database.close()


def test_controller_code_cannot_be_a_writable_root(config):
    from fridica import permissions
    with pytest.raises(ValueError):
        replace(config, file_access=True, workspace=Path(permissions.__file__).parent)


def test_automatic_result_recovers_before_initial_outbox_save(managed, message):
    database = Store(managed.state_path)
    target = managed.workspace / 'README.md'
    manager = policy(managed, database, [action('write', target, 'reviewed')])
    manager.grant('UALICE', 'CROOM', str(target))
    assert respond(manager, message()).status == 'complete'
    database.close()
    database = Store(managed.state_path)
    try:
        replica = Replica(managed, database, None, Transport())
        target.write_text('later')
        asyncio.run(replica.permissions.process_approved(replica))
        assert target.read_text() == 'later'
        assert len(replica.transport.sent) == 1
        assert replica.transport.sent[0][1].status == 'complete'
    finally:
        database.close()


def test_escalate_hands_brief_to_remote_host(managed, store, message, caplog):
    import logging
    from pathlib import PurePosixPath
    from fridica.config import Host
    dart9 = Host("dart9", (PurePosixPath("/mnt/a"),))
    heavy = replace(managed, heavy_tasks=True, remote_hosts=(dart9,))
    manager = policy(heavy, store, [action("escalate", "dart9", "Run the full suite.", "Started; the result lands here.")])
    result = respond(manager, message())
    assert result.status == "complete" and result.text == "Started; the result lands here."
    assert result.escalate == "Run the full suite." and result.escalate_host == "" and not requests(manager)
    # The local roots are never a worker host: an unknown or local name goes to the first remote host.
    for index, named in enumerate(("local", "snowy")):
        manager = policy(heavy, store, [action("escalate", named, "job", "Started.")])
        with caplog.at_level(logging.WARNING, logger="fridica.permissions"):
            result = respond(manager, message(f"event-{named}", timestamp=f"200.00000{index + 1}"))
        assert result.escalate == "job" and result.escalate_host == "" and "unknown host" in caplog.text
    # Without heavy tasks the brief is dropped and the text still goes out.
    manager = policy(managed, store, [action("escalate", "", "job", "Started.")])
    result = respond(manager, message("event-off", timestamp="300.000001"))
    assert result.status == "complete" and result.escalate == "" and result.text == "Started."
    # An empty brief or reply is invalid, and blocks like any other malformed plan.
    manager = policy(heavy, store, [action("escalate", "dart9", "", "Started.")])
    assert respond(manager, message("event-bad", timestamp="400.000001")).status == "blocked"
    # A brief that carries a long pasted specification fits; one past the limit blocks and logs why.
    manager = policy(heavy, store, [action("escalate", "dart9", "x" * 40000, "Started.")])
    assert respond(manager, message("event-long", timestamp="500.000001")).escalate == "x" * 40000
    manager = policy(heavy, store, [action("escalate", "dart9", "x" * 40001, "Started.")])
    with caplog.at_level(logging.WARNING, logger="fridica.permissions"):
        assert respond(manager, message("event-huge", timestamp="600.000001")).status == "blocked"
    assert "File plan for event event-huge was rejected (ValueError): Invalid heavy-task brief" in caplog.text


def test_planner_prompt_lists_only_remote_hosts_for_heavy_work(managed, message):
    from pathlib import PurePosixPath
    from fridica.agents import ClaudeBackend
    from fridica.config import Host
    from fridica.prompts import FILE_ESCALATE_NOTE, HEAVY_NOTE
    heavy = replace(managed, heavy_tasks=True, remote_hosts=(Host("dart9", (PurePosixPath("/mnt/a"),)),))
    context = ConversationContext([], managed.owner_id, "profile", "task", 1)
    seen = {}

    async def fake_invoke(self, prompt, classify, session=None, *, schema=None):
        seen["prompt"] = prompt
        return {"operation": "observe", "path": "", "content": "", "text": "", "update": None}, None

    for config, expected in ((heavy, True), (managed, False)):
        backend = ClaudeBackend(config)
        backend._invoke = fake_invoke.__get__(backend)
        asyncio.run(backend.plan(message(), context, {}, {"writable": [], "read_only": []}))
        prompt = seen["prompt"]
        assert (FILE_ESCALATE_NOTE in prompt) is expected and (HEAVY_NOTE in prompt) is expected
        assert ("under 40000 characters" in prompt) is expected
        if expected:
            assert '"hosts": [{"name": "dart9"' in prompt and '"name": "local"' not in prompt


def test_reply_plan_carries_details(managed, store, message):
    manager = policy(managed, store, [dict(action('reply', text='Executive summary.'), details='  # Details\n\nMore.  ')])
    result = respond(manager, message())
    assert result.text == 'Executive summary.' and result.details == '# Details\n\nMore.'
    manager = policy(managed, store, [dict(action('reply', text='Summary.'), details='x' * 40001)])
    assert respond(manager, message('event-long', timestamp='200.000001')).status == 'blocked'
