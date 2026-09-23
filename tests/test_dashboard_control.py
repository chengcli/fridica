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


def test_continued_thread_can_be_archived_and_resume_clears_continuation(config, store, message):
    from fridica.dashboard_control import thread_action
    from fridica.models import AgentResult
    store.bind(config.owner_id, config.workspace_id)
    msg = message()
    store.add(msg)
    store.begin(msg, 't1', 6)
    store.save_result(msg, AgentResult('done'))
    store.delivered(msg, '100.000002')
    store.pause_loop(msg, 6, 3)
    store.begin_continuation(msg)
    store.finish_continuation(msg, '900.000001', 't2', None)
    task = store.task(msg)
    assert task['control_state'] == 'paused' and task['continuation'] == '900.000001'
    thread_action(config, 'CROOM', msg.thread_id, 'resume', task['control_revision'])
    resumed = store.task(msg)
    assert resumed['control_state'] == 'active' and resumed['continuation'] is None and resumed['turns'] == 0
    with store.connection:
        store.connection.execute("UPDATE tasks SET control_state='paused',continuation='900.000001' WHERE thread=?", (msg.thread_id,))
    task = store.task(msg)
    thread_action(config, 'CROOM', msg.thread_id, 'archive', task['control_revision'])
    assert store.task(msg)['control_state'] == 'archived'


def blocked_thread(config, store, message):
    from fridica.models import AgentResult
    store.bind(config.owner_id, config.workspace_id)
    first = message()
    store.add(first)
    store.begin(first, 't1', 1)
    store.save_result(first, AgentResult('failed', 'blocked'))
    store.delivered(first, '100.000002')
    assert store.task(first)['status'] == 'blocked'
    return first


@pytest.mark.parametrize('turned_away', ['blocked', 'before_resume', 'notice'])
def test_resuming_blocked_thread_answers_latest_unanswered_message(config, store, message, turned_away):
    from fridica.dashboard_control import thread_action
    from fridica.models import AgentResult
    first = blocked_thread(config, store, message)
    later = message('event2', timestamp='200.000001', text='<@UOWNER> could you check the error?')
    store.add(later)
    if turned_away == 'notice':
        store.save_notice(later, AgentResult('notice', 'blocked'), 't1', 1)
        store.delivered(later, '200.000002')
    else:
        store.mark('event2', 'observed', turned_away)
    # The owner's own later message does not hide the one waiting for an answer.
    store.add(message('event3', timestamp='300.000001', sender_id=config.owner_id, text='nudge'), 'observed')
    result = thread_action(config, 'CROOM', first.thread_id, 'resume', store.task(first)['control_revision'])
    assert result['replayed'] is True
    task = store.task(first)
    assert task['status'] == 'complete' and 100.000001 < task['reset_at'] < 200.000001
    assert [row['event_id'] for row in store.pending()] == ['event2']
    assert store.get('event2')['reply_only'] == 0 and store.get('event2')['result'] is None
    assert store.get('event1')['state'] == 'sent'


def test_resume_does_not_replay_answered_or_paused_threads(config, store, message):
    from fridica.dashboard_control import thread_action
    first = blocked_thread(config, store, message)
    # Nothing arrived after the failure: the failed request itself is not retried.
    result = thread_action(config, 'CROOM', first.thread_id, 'resume', store.task(first)['control_revision'])
    assert result['replayed'] is False and store.pending() == []
    assert store.task(first)['reset_at'] > 200
    # A paused (loop-protected) thread only accepts future messages.
    later = message('event2', timestamp='200.000001')
    store.add(later)
    store.mark('event2', 'observed', 'blocked')
    with store.connection:
        store.connection.execute("UPDATE tasks SET control_state='paused',pause_reason='loop' WHERE thread=?", (first.thread_id,))
    result = thread_action(config, 'CROOM', first.thread_id, 'resume', store.task(first)['control_revision'])
    assert result['replayed'] is False and store.pending() == []


@pytest.mark.parametrize('changes', [
    {'generated': True, 'task_status': 'complete', 'text': 'Report: all checks passed.'},
    {'text': 'any update on this?'},
])
def test_resume_reports_when_latest_message_cannot_get_a_reply(config, store, message, changes):
    """A message without a mention (such as a peer's completed report) would be ignored or only observed after Resume."""
    from fridica.dashboard_control import REPLAY_UNMENTIONED, thread_action
    first = blocked_thread(config, store, message)
    store.add(message('event2', timestamp='200.000001', **changes))
    store.mark('event2', 'observed', 'blocked')
    result = thread_action(config, 'CROOM', first.thread_id, 'resume', store.task(first)['control_revision'])
    assert result['replayed'] is False and result['note'] == REPLAY_UNMENTIONED
    assert store.pending() == [] and store.task(first)['reset_at'] > 200


def test_replayed_mention_gets_exactly_one_reply(config, store, message):
    from fridica.dashboard_control import thread_action
    from fridica.models import AgentResult
    from test_replica import Agent, Transport
    from fridica.replica import Replica
    first = blocked_thread(config, store, message)
    later = message('event2', timestamp='200.000001', generated=True, task_status='complete',
                    text='<@UOWNER> the report is in; can you confirm the paths?')
    store.add(later)
    store.mark('event2', 'observed', 'blocked')
    assert thread_action(config, 'CROOM', first.thread_id, 'resume', store.task(first)['control_revision'])['replayed'] is True
    replica = Replica(config, store, Agent(results=[AgentResult('Paths confirmed.')]), Transport())
    assert [row['event_id'] for row in store.pending()] == ['event2']
    asyncio.run(replica.process(later))
    assert [m.event_id for m, _ in replica.agent.responded] == ['event2']
    assert len(replica.transport.sent) == 1 and store.get('event2')['state'] == 'sent'
    assert store.pending() == []
