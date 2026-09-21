import asyncio
from dataclasses import replace
import time

from aiohttp.test_utils import TestClient, TestServer
import pytest

from fridica.dashboard import create_app
from fridica.dashboard_control import request_detail, decide
from fridica.permissions import Permissions


def prepare(config, store, message, *, channel='CROOM', content='new\n'):
    config = replace(config, file_access=True)
    store.bind(config.owner_id, config.workspace_id)
    msg = message(channel_id=channel)
    store.add(msg, 'sent')
    path = config.workspace / 'sample.txt'
    path.write_text('old\n')
    with store.connection:
        store.connection.execute('INSERT INTO file_requests(id,event_id,sender,channel,operation,path,content,before_content,created) VALUES(?,?,?,?,?,?,?,?,?)',
                                 ('r1', msg.event_id, msg.sender_id, channel, 'write', str(path), content, 'old\n', time.time()))
    return config, path


def test_review_approve_and_real_file_execution(config, store, message):
    config, path = prepare(config, store, message)
    detail = request_detail(config, 'r1')
    assert '-old' in detail['diff'] and '+new' in detail['diff']
    decide(config, 'r1', 'approved', detail['revision'])
    assert path.read_text() == 'old\n'
    Permissions(config, store).apply('r1')
    assert path.read_text() == 'new\n'
    assert store.connection.execute('SELECT count(*) FROM dashboard_decisions').fetchone()[0] == 1
    with pytest.raises(ValueError):
        decide(config, 'r1', 'approved', detail['revision'])


def test_reject_never_changes_file(config, store, message):
    config, path = prepare(config, store, message)
    detail = request_detail(config, 'r1')
    decide(config, 'r1', 'rejected', detail['revision'])
    assert path.read_text() == 'old\n'
    assert store.connection.execute("SELECT status FROM file_requests WHERE id='r1'").fetchone()[0] == 'rejected'


def test_stale_proposal_and_changed_file_are_blocked(config, store, message):
    config, path = prepare(config, store, message)
    detail = request_detail(config, 'r1')
    path.write_text('someone edited this\n')
    with pytest.raises(ValueError, match='changed'):
        decide(config, 'r1', 'approved', detail['revision'])
    path.write_text('old\n')
    with store.connection:
        store.connection.execute("UPDATE file_requests SET content='different' WHERE id='r1'")
    with pytest.raises(ValueError, match='changed'):
        decide(config, 'r1', 'approved', detail['revision'])


def test_other_channel_request_cannot_be_read_or_approved(config, store, message):
    config, path = prepare(config, store, message, channel='COTHER')
    with pytest.raises(ValueError):
        request_detail(config, 'r1')
    with pytest.raises(ValueError):
        decide(config, 'r1', 'approved', 'ignored')


def test_control_http_auth_origin_and_read_only_default(config, store, message):
    config, path = prepare(config, store, message)
    async def run():
        async with TestClient(TestServer(create_app(config))) as client:
            assert (await client.get('/api/requests/r1')).status == 403
            assert (await client.post('/api/requests/r1/decision', json={})).status == 403
        async with TestClient(TestServer(create_app(config, approval_key='test-key'))) as client:
            headers = {'Authorization': 'Bearer test-key', 'Origin': str(client.make_url('/')).rstrip('/')}
            assert (await client.get('/api/requests/r1')).status == 403
            response = await client.get('/api/requests/r1', headers=headers)
            assert response.status == 200
            detail = await response.json()
            body = {'decision':'approved', 'revision':detail['revision']}
            assert (await client.post('/api/requests/r1/decision', headers={'Authorization':'Bearer test-key'}, json=body)).status == 403
            assert (await client.post('/api/requests/r1/decision', headers={**headers,'Origin':'https://evil.example'},json=body)).status == 403
            assert (await client.post('/api/requests/r1/decision', headers=headers,json=body)).status == 200
            assert (await client.post('/api/requests/r1/decision', headers=headers,json=body)).status == 409
    asyncio.run(run())
    assert path.read_text() == 'old\n'


def test_rejection_is_queued_for_notification_not_waiting_for_information(config, store, message):
    from fridica.dashboard import task_page
    from fridica.models import AgentResult
    config, path = prepare(config, store, message)
    store.begin(message(), 't1', 1)
    store.save_result(message(), AgentResult('Waiting for approval', 'waiting'))
    store.delivered(message(), '101.000001')
    detail = request_detail(config, 'r1')
    decide(config, 'r1', 'rejected', detail['revision'])
    page = task_page(config)
    assert page['items'][0]['status'] == 'rejected'
    assert page['counts']['active'] == 1
    assert page['counts']['waiting'] == 0


def test_project_binding_cannot_expand_roots_or_grant_writes(config, store, message):
    from fridica.dashboard_control import bind_project, assign_project, metadata
    store.bind(config.owner_id, config.workspace_id)
    store.add(message()); store.begin(message(), 't1', 1)
    with pytest.raises(ValueError):
        bind_project(config, str(config.workspace.parent), 'https://github.com/example/repo')
    with pytest.raises(ValueError):
        bind_project(config, str(config.workspace), 'javascript:alert(1)')
    project = bind_project(config, str(config.workspace), 'https://github.com/example/repo')
    assign_project(config, 'CROOM', message().thread_id, project['id'])
    assert metadata(config)['task_projects']['CROOM:'+message().thread_id] == project['id']
    assert store.connection.execute('SELECT count(*) FROM grants').fetchone()[0] == 0
    assert config.additional_workspaces == ()


def test_review_diff_keeps_unterminated_lines_separate(config, store, message):
    config, path = prepare(config, store, message, content='new')
    path.write_text('old')
    with store.connection:
        store.connection.execute("UPDATE file_requests SET before_content='old' WHERE id='r1'")
    diff = request_detail(config, 'r1')['diff']
    assert diff.splitlines() == ['--- Before', '+++ After', '@@ -1 +1 @@',
                                 '-old', '\\ No newline at end of file',
                                 '+new', '\\ No newline at end of file']
