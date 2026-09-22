import asyncio
from dataclasses import replace
import time

import pytest

from fridica.agents import CodexBackend
from fridica.dashboard import task_page
from fridica.dashboard_control import cleanup_preview, thread_action
from fridica.models import AgentResult
from fridica.replica import Replica
from fridica.store import Store
from test_replica import Agent, Transport, process


def test_three_waits_pause_without_fourth_model_call_and_survive_restart(config, message):
    db = Store(config.state_path)
    agent = Agent(results=[AgentResult('Which branch?', 'waiting')] * 3)
    transport = Transport()
    replica = Replica(config, db, agent, transport)
    for i in range(3):
        process(replica, message(str(i), timestamp=f'{100+i*2}.000001'))
    assert db.task(message())['control_state'] == 'paused'
    assert len(agent.responded) == 3
    assert len(transport.sent) == 1
    assert task_page(config)['counts']['attention'] == 1
    db.close()
    db = Store(config.state_path)
    try:
        fresh = Agent()
        replica = Replica(config, db, fresh, Transport())
        followup = message('fourth', timestamp='120.000001')
        process(replica, followup)
        assert not fresh.responded and not fresh.classified and not replica.transport.sent
        row = db.task(followup)
        thread_action(config, 'CROOM', followup.thread_id, 'resume', row['control_revision'])
        assert db.get('fourth')['state'] == 'observed'
        process(replica, message('late-after-resume', timestamp='125.000001'))
        assert not fresh.responded and db.get('late-after-resume')['decision'] == 'before_resume'
        process(replica, message('new', timestamp=f'{time.time()+1:.6f}', turn=99, generated=True))
        assert len(fresh.responded) == 1
        assert fresh.responded[0][1].turn == 1
    finally:
        db.close()


def test_progress_resets_wait_streak_and_other_threads_work(config, store, message):
    agent = Agent(results=[AgentResult('Question', 'waiting'), AgentResult('Done'), AgentResult('Question', 'waiting'), AgentResult('Other done')])
    replica = Replica(config, store, agent, Transport())
    for i in range(3):
        process(replica, message(str(i), timestamp=f'{100+i*2}.000001'))
    assert store.task(message())['control_state'] == 'active'
    process(replica, message('other', timestamp='130.000001', thread_id='130.000001'))
    assert len(agent.responded) == 4


def test_archive_restore_and_cleanup_keep_dedup_and_project_file(config, store, message):
    replica = Replica(config, store, Agent(), Transport())
    process(replica, message())
    path = config.workspace / 'keep.txt'
    path.write_text('keep me')
    row = store.task(message())
    thread_action(config, 'CROOM', message().thread_id, 'archive', row['control_revision'])
    assert task_page(config)['total'] == 0
    archived = task_page(config, view='archived')['items'][0]
    thread_action(config, 'CROOM', message().thread_id, 'restore', archived['control_revision'])
    assert task_page(config)['total'] == 1
    row = store.task(message())
    thread_action(config, 'CROOM', message().thread_id, 'archive', row['control_revision'])
    preview = cleanup_preview(config, 'CROOM', message().thread_id)
    assert preview['messages'] == 2
    thread_action(config, 'CROOM', message().thread_id, 'clean', store.task(message())['control_revision'], preview['revision'])
    assert not replica.receive(message())
    assert store.get(message().event_id)['result'] is None
    assert 'help' not in store.get(message().event_id)['payload']
    assert path.read_text() == 'keep me'
    assert task_page(config, view='archived')['total'] == 0
    process(replica, message('late', timestamp='140.000001'))
    assert store.get('late')['decision'] == 'cleaned'
    assert 'help' not in store.get('late')['payload']
    assert store.add(message('direct', timestamp='150.000001'))
    assert 'help' not in store.get('direct')['payload']
    assert store.get('direct')['state'] == 'observed'


def test_cleanup_rejects_active_stale_and_other_channel(config, store, message):
    process(Replica(config, store, Agent(results=[AgentResult('Question', 'waiting')]), Transport()), message())
    row = store.task(message())
    with pytest.raises(ValueError):
        thread_action(config, 'CROOM', message().thread_id, 'archive', row['control_revision'])
    with pytest.raises(ValueError):
        thread_action(config, 'COTHER', message().thread_id, 'close', row['control_revision'])
    thread_action(config, 'CROOM', message().thread_id, 'close', row['control_revision'])
    with pytest.raises(ValueError, match='changed'):
        thread_action(config, 'CROOM', message().thread_id, 'archive', row['control_revision'])
    thread_action(config, 'CROOM', message().thread_id, 'archive', store.task(message())['control_revision'])
    preview = cleanup_preview(config, 'CROOM', message().thread_id)
    store.add(message('new', timestamp='140.000001'), 'observed')
    with pytest.raises(ValueError, match='changed'):
        thread_action(config, 'CROOM', message().thread_id, 'clean', store.task(message())['control_revision'], preview['revision'])


def test_codex_model_and_effort_are_explicit(config, tmp_path):
    config = replace(config, backend='codex', model='gpt-5.6-luna', reasoning_effort='low')
    command = CodexBackend(config).command(tmp_path, tmp_path/'schema.json', True)
    assert command[command.index('--model')+1] == 'gpt-5.6-luna'
    assert 'model_reasoning_effort="low"' in command
    with pytest.raises(ValueError):
        replace(config, reasoning_effort='invalid')


def test_cleanup_clears_proposals_but_preserves_decisions(config, store, message):
    from test_dashboard_control import prepare, request_detail, decide
    config, path = prepare(config, store, message)
    msg = message()
    store.begin(msg, 't1', 1)
    store.save_result(msg, AgentResult('Awaiting approval', 'waiting'))
    store.delivered(msg, '101.000001')
    with pytest.raises(ValueError, match='pending file'):
        thread_action(config, 'CROOM', msg.thread_id, 'close', 0)
    review = request_detail(config, 'r1')
    decide(config, 'r1', 'rejected', review['revision'])
    with store.connection:
        store.connection.execute("UPDATE file_requests SET status='declined',notified=1 WHERE id='r1'")
    store.save_result(msg, AgentResult('Declined'))
    store.delivered(msg, '102.000001')
    thread_action(config, 'CROOM', msg.thread_id, 'archive', 0)
    preview = cleanup_preview(config, 'CROOM', msg.thread_id)
    thread_action(config, 'CROOM', msg.thread_id, 'clean', 1, preview['revision'])
    row = store.connection.execute("SELECT * FROM file_requests WHERE id='r1'").fetchone()
    assert row['content'] == row['path'] == '' and row['before_content'] is None
    assert store.connection.execute('SELECT decision FROM dashboard_decisions').fetchone()[0] == 'rejected'
    assert not store.add(message('redelivered'))
    assert path.read_text() == 'old\n'


def test_lifecycle_requires_key_and_same_origin(config, store, message):
    from aiohttp.test_utils import TestClient, TestServer
    from fridica.dashboard import create_app
    process(Replica(config, store, Agent(), Transport()), message())
    async def run():
        async with TestClient(TestServer(create_app(config, approval_key='test-key'))) as client:
            body = {'channel':'CROOM','thread':message().thread_id,'action':'archive','revision':0}
            assert (await client.post('/api/thread-action',json=body)).status == 403
            assert (await client.post('/api/thread-action',json=body,headers={'Authorization':'Bearer test-key'})).status == 403
            headers = {'Authorization':'Bearer test-key','Origin':str(client.make_url('/')).rstrip('/')}
            assert (await client.post('/api/thread-action',json=body,headers=headers)).status == 200
            assert (await client.get('/api/cleanup-preview',params={'channel':'CROOM','thread':message().thread_id})).status == 403
            assert (await client.post('/api/thread-action',json=body,headers=headers)).status == 409
    asyncio.run(run())


def test_paused_followups_drain_even_with_pending_file_request(config, store, message):
    from test_dashboard_control import prepare, request_detail, decide
    config, path = prepare(config, store, message)
    config = replace(config, max_turns=1)
    msg = message()
    store.begin(msg, 't1', 1)
    store.save_result(msg, AgentResult('Approve this file', 'waiting'))
    store.delivered(msg, '101.000001')
    replica = Replica(config, store, Agent(), Transport())
    assert store.task(msg)['control_state'] == 'paused'
    later = message('paused-followup', timestamp='120.000001')
    replica.receive(later)
    assert store.get(later.event_id)['state'] == 'observed'
    asyncio.run(replica.process(later))
    decide(config, 'r1', 'rejected', request_detail(config, 'r1')['revision'])
    asyncio.run(replica.permissions.process_approved(replica))
    assert not replica.transport.sent
    thread_action(config, 'CROOM', msg.thread_id, 'resume', store.task(msg)['control_revision'])
    asyncio.run(replica.permissions.process_approved(replica))
    assert len(replica.transport.sent) == 1
    assert path.read_text() == 'old\n'


def test_wait_limit_requires_integer(config):
    with pytest.raises(ValueError):
        replace(config, max_wait_replies=2.5)


def test_wait_streak_follows_reply_order_for_delayed_messages(config, store, message):
    agent = Agent(results=[AgentResult('Which branch?', 'waiting'), AgentResult('Done'), AgentResult('Which branch?', 'waiting'), AgentResult('Which branch?', 'waiting')])
    replica = Replica(config, store, agent, Transport())
    for i, timestamp in enumerate(('100.000001','50.000001','200.000001','300.000001')):
        process(replica, message(str(i), timestamp=timestamp, thread_id='10.000001'))
    assert store.task(message(thread_id='10.000001'))['control_state'] == 'active'


@pytest.mark.parametrize('automatic', [True, False])
def test_successful_file_write_breaks_wait_streak(config, store, message, automatic):
    from test_permissions import action, policy, requests

    config = replace(config, file_access=True, max_turns=20)
    target = config.workspace / 'note.txt'
    target.write_text('before')
    clarify = action('clarify', text='Which branch?')
    manager = policy(config, store, [clarify, clarify, action('read', target),
                                   action('write', target, 'after'), clarify, clarify, clarify])
    replica = Replica(config, store, Agent(), Transport())
    replica.permissions = manager
    if automatic:
        manager.grant('UALICE', 'CROOM', str(config.workspace), None)
    for i in range(6):
        process(replica, message(str(i), timestamp=f'{100+i*2}.000001'))
        if i == 2:
            request = requests(manager)[0]
            if not automatic:
                assert store.task(message())['control_state'] == 'active'
                manager.decide(request['id'], 'approved')
                asyncio.run(manager.process_approved(replica))
            assert target.read_text() == 'after'
        assert store.task(message())['control_state'] == ('paused' if i == 5 else 'active')
