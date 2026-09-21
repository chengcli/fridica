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
