import asyncio
from dataclasses import replace
import json
import time

from aiohttp.test_utils import TestClient, TestServer
import pytest

from fridica.dashboard import create_app, snapshot
from fridica.models import AgentResult


def test_missing_database_is_not_created(config):
    result = snapshot(config)
    assert result['health']['status'] == 'unknown'
    assert result['events'] == []
    assert not config.state_path.exists()


def test_snapshot_is_read_only_and_scoped(config, store, message):
    store.bind(config.owner_id, config.workspace_id)
    store.add(message(text='hello xoxp-123456-SECRET'))
    store.begin(message(), 'task1', 1)
    store.add(message('other', channel_id='COTHER', timestamp='101.000001', text='other channel'))
    before = list(store.connection.iterdump())
    result = snapshot(config)
    assert list(store.connection.iterdump()) == before
    assert store.get('event1')['state'] == 'running'
    assert len(result['events']) == 1
    assert result['events'][0]['text'] == 'hello [redacted]'
    assert result['tasks'][0]['status'] == 'running'
    assert result['health']['status'] == 'unknown'
    assert 'SECRET' not in json.dumps(result)


def test_identity_mismatch_is_rejected(config, store):
    store.bind('USOMEONE', config.workspace_id)
    with pytest.raises(ValueError, match='identity'):
        snapshot(config)


def test_heartbeat_expiry_and_stop(config, store):
    store.heartbeat('connected', False, time.time() - 50)
    assert snapshot(config)['health']['status'] == 'connected'
    store.connection.execute('UPDATE runtime SET heartbeat_at=?', (time.time() - 30,))
    store.connection.commit()
    assert snapshot(config)['health']['status'] == 'offline'
    store.heartbeat('stopped', False, time.time() - 50)
    assert snapshot(config)['health']['status'] == 'stopped'


def test_delivery_failure_overrides_stale_task(config, store, message):
    store.add(message())
    store.begin(message(), 'task1', 1)
    store.save_result(message(), AgentResult('answer'))
    store.mark('event1', 'ambiguous')
    result = snapshot(config)
    assert result['tasks'][0]['status'] == 'ambiguous'


def test_approval_and_grants(config, store, message):
    store.add(message())
    store.begin(message(), 'task1', 1)
    with store.connection:
        store.connection.execute("INSERT INTO file_requests(id,event_id,sender,channel,operation,path,content,status,created) VALUES('r1','event1','UALICE','CROOM','write','a.txt','private content','pending',?)", (time.time(),))
        store.connection.execute("INSERT INTO grants VALUES('g1','UALICE','CROOM','a.txt',NULL,0)")
        store.connection.execute("INSERT INTO grants VALUES('g2','UALICE','COTHER','other.txt',NULL,0)")
    result = snapshot(replace(config, file_access=True))
    assert result['tasks'][0]['status'] == 'awaiting_approval'
    assert result['requests'][0]['id'] == 'r1'
    assert 'private content' not in json.dumps(result)
    assert [g['id'] for g in result['grants']] == ['g1']


def test_http_boundary_and_resources(config):
    async def run():
        async with TestClient(TestServer(create_app(config))) as client:
            response = await client.get('/api/state')
            assert response.status == 200
            assert (await response.json())['health']['status'] == 'unknown'
            assert response.headers['Cache-Control'] == 'no-store'
            for headers in ({'Host': 'evil.example'}, {'Origin': 'https://evil.example'}, {'Sec-Fetch-Site': 'cross-site'}):
                assert (await client.get('/api/state', headers=headers)).status == 403
            assert (await client.post('/api/state')).status == 405
            assert (await client.get('/tokens.env')).status == 404
            assert (await client.get('/')).status == 200
            js = await (await client.get('/dashboard.js')).text()
            assert 'innerHTML' not in js
            assert 'textContent' in js
    asyncio.run(run())


def test_reply_only_notice_does_not_hide_failed_delivery(config, store, message):
    original = message()
    store.add(original)
    store.begin(original, 'task1', 1)
    store.save_result(original, AgentResult('answer'))
    store.mark('event1', 'ambiguous')
    notice = message('notice', timestamp='101.000001')
    store.add(notice, 'sent')
    with store.connection:
        store.connection.execute("UPDATE events SET task_id='task1',reply_only=1 WHERE event_id='notice'")
    state = snapshot(config)
    assert state['tasks'][0]['status'] == 'ambiguous'
    assert state['attention_count'] == 1


def test_heartbeat_runs_while_worker_waits_and_stops_on_cancel(config, store, monkeypatch):
    from fridica import slack
    from fridica.config import Config
    class Socket:
        def __init__(self, **kwargs):
            self.socket_mode_request_listeners = []
        async def connect(self): pass
        async def close(self): pass
        async def is_connected(self): return True
    async def validate(self): pass
    async def run(self): await asyncio.Future()
    monkeypatch.setattr(Config, 'tokens', lambda self: ('app', 'user'))
    monkeypatch.setattr(slack, 'SocketModeClient', Socket)
    monkeypatch.setattr(slack.SlackTransport, 'validate', validate)
    monkeypatch.setattr(slack.Replica, 'run', run)
    async def scenario():
        worker = asyncio.create_task(slack.serve(config, store, None, True))
        try:
            for _ in range(100):
                await asyncio.sleep(.01)
                if snapshot(config)['health']['status'] == 'connected':
                    break
            assert snapshot(config)['health']['status'] == 'connected'
        finally:
            worker.cancel()
            with pytest.raises(asyncio.CancelledError):
                await worker
        assert snapshot(config)['health']['status'] == 'stopped'
    asyncio.run(scenario())


def test_confirmed_socket_echo_is_a_sent_reply(config, store, message):
    original = message()
    store.add(original)
    store.begin(original, 'task1', 1)
    store.save_result(original, AgentResult('answer'))
    echo = message('slack-echo', timestamp='101.000001', sender_id=config.owner_id,
                   generated=True, task_id='task1', text='answer')
    store.add(echo, 'observed')
    with store.connection:
        store.connection.execute("UPDATE events SET state='sent',sent_ts=? WHERE event_id='event1'", (echo.timestamp,))
    state = snapshot(config)
    assert state['events'][0]['outgoing'] is True
    assert state['events'][0]['state'] == 'sent'
    assert state['events'][0]['task_id'] == 'task1'
    assert state['last_message'] == float(original.timestamp)


def test_task_counts_and_pending_requests_survive_history_window(config, store, message):
    from fridica.dashboard import task_page, thread_detail
    store.bind(config.owner_id, config.workspace_id)
    for i in range(130):
        msg = message('e'+str(i), timestamp=f'{100+i}.000001', thread_id=f'{100+i}.000001')
        store.add(msg)
        store.begin(msg, 't'+str(i), 1)
        store.save_result(msg, AgentResult('Waiting for more detail', 'waiting'))
        with store.connection:
            store.connection.execute("UPDATE events SET state='sent' WHERE event_id=?", (msg.event_id,))
    with store.connection:
        store.connection.execute("INSERT INTO file_requests(id,event_id,sender,channel,operation,path,content,status,created) VALUES('old','e0','UALICE','CROOM','write','a.txt','','pending',1)")
    page = task_page(config)
    assert len(page['items']) == 50
    assert page['counts']['all'] == 130
    assert page['counts']['attention'] == 1
    assert page['counts']['waiting'] == 129
    inbox = task_page(config, view='attention')
    assert inbox['items'][0]['task_id'] == 't0'
    assert thread_detail(config, 'CROOM', '100.000001')['requests'][0]['id'] == 'old'
    assert len(task_page(config, offset=100)['items']) == 30


def test_ignored_followup_does_not_clear_task_failure(config, store, message):
    from fridica.dashboard import task_page
    original = message()
    store.add(original)
    store.begin(original, 'task1', 1)
    store.save_result(original, AgentResult('answer'))
    store.mark(original.event_id, 'ambiguous')
    store.add(message('ignored', timestamp='102.000001'), 'ignored')
    page = task_page(config)
    assert page['items'][0]['status'] == 'ambiguous'
    assert page['counts']['attention'] == 1


def test_sender_filter_is_exact_scoped_and_applied_before_pagination(config, store, message):
    from fridica.dashboard import task_page
    for i, sender in enumerate(['UALICE', 'UBOB', 'UALICE', 'UALICE2']):
        msg = message('person'+str(i), sender_id=sender, timestamp=f'{200+i}.000001',
                      thread_id=f'{200+i}.000001', text='UALICE mentioned in every request')
        store.add(msg)
        store.begin(msg, 'person-task'+str(i), 1)
        store.save_result(msg, AgentResult('reply', 'waiting' if i == 0 else 'complete'))
        store.mark(msg.event_id, 'sent')
    other = message('outside', channel_id='COTHER', sender_id='UOTHER', timestamp='300.000001', thread_id='300.000001')
    store.add(other)
    store.begin(other, 'outside', 1)
    page = task_page(config, sender='UALICE', limit=1, offset=1)
    assert page['total'] == 2
    assert [t['sender'] for t in page['items']] == ['UALICE']
    assert page['counts']['all'] == 2
    assert page['counts']['waiting'] == 1
    assert page['counts']['complete'] == 1
    assert set(page['senders']) == {'UALICE', 'UBOB', 'UALICE2'}
    assert task_page(config, sender='UALICE', view='waiting')['total'] == 1
    assert task_page(config, sender='UALICE', query='missing')['total'] == 0
    assert task_page(config, sender='UNKNOWN')['counts']['all'] == 0
    assert task_page(config)['total'] == 4

    async def run():
        async with TestClient(TestServer(create_app(config))) as client:
            data = await (await client.get('/api/tasks?sender=UBOB')).json()
            assert data['total'] == 1
            assert data['items'][0]['sender'] == 'UBOB'
    asyncio.run(run())


def test_continued_and_finished_threads_are_finished_not_attention(config, store, message):
    from fridica.dashboard import task_page
    store.bind(config.owner_id, config.workspace_id)
    # Thread A: wrapped up at the turn limit and continued in thread C.
    a = message()
    store.add(a)
    store.begin(a, 'task-a', 6)
    store.save_result(a, AgentResult('Which branch?', 'waiting'))
    store.delivered(a, '100.000002')
    store.pause_loop(a, 6, 3)
    store.begin_continuation(a)
    store.finish_continuation(a, '900.000001', 'task-c', 'sess')
    # Thread B: the agent declared the discussion finished and a debrief was posted.
    b = message('event-b', timestamp='200.000001', thread_id='200.000001')
    store.add(b)
    store.begin(b, 'task-b', 2)
    store.save_result(b, AgentResult('All done.', finished=True))
    store.delivered(b, '200.000002')
    assert store.claim_debrief(b)
    store.record_debrief(b)

    snap = snapshot(config)
    statuses = {t['thread']: t['status'] for t in snap['tasks']}
    assert statuses[a.thread_id] == 'continued' and statuses[b.thread_id] == 'finished'
    assert statuses['900.000001'] == 'complete'
    assert snap['config']['allowed_domains'] == [] and snap['config']['resume_sessions'] is True
    assert snap['config']['session_timeout'] == 14 * 86400

    page = task_page(config)
    by_thread = {item['thread']: item for item in page['items']}
    assert by_thread[a.thread_id]['status'] == 'continued' and by_thread[a.thread_id]['bucket'] == 'complete'
    assert by_thread[b.thread_id]['status'] == 'finished' and by_thread[b.thread_id]['bucket'] == 'complete'
    assert 'continues in a new thread' in by_thread[a.thread_id]['pause_reason']
    assert task_page(config, view='attention')['counts']['attention'] == 0
    assert {i['thread'] for i in task_page(config, view='complete')['items']} >= {a.thread_id, b.thread_id}

    from fridica.activity import activity_page
    feed = activity_page(config)
    texts = [item['text'] for item in feed['items'] if item['kind'] == 'control']
    assert any('summary posted as a new thread' in t for t in texts) and any('debrief posted' in t for t in texts)
