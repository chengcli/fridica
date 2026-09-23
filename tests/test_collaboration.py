import asyncio
import json
from dataclasses import replace

import pytest

from fridica.collaboration import owner_update, prepare, record, snapshot
from fridica.dashboard import task_page
from fridica.dashboard_control import cleanup_preview, thread_action
from fridica.models import AgentResult, ConversationContext
from fridica.prompts import conversation_prompt, digest_prompt
from fridica.replica import Replica
from test_replica import Agent, Transport, process


@pytest.mark.parametrize('paraphrase', [False, True])
def test_repeated_reply_pauses_after_three_no_progress_turns(config, store, message, paraphrase):
    replies = [AgentResult(f'Waiting {i}.' if paraphrase else 'Waiting.',
               update={'kind': 'status', 'next_step': 'Wait for review'}) for i in range(4)]
    replica = Replica(replace(config, max_turns=20), store, Agent(results=replies), Transport())
    for i in range(5):
        process(replica, message(str(i), timestamp=str(200+i)))
    assert len(replica.transport.sent) == 1
    assert len(replica.agent.responded) == 4
    assert store.task(message())['control_state'] == 'paused'
    assert store.get('1')['decision'] == 'silent'
    assert not replica.transport.announced


def test_continuation_keeps_task_notes_and_repository(config, store, message):
    entry, notes = seed_claim(config, store, message)
    owner_update(config, 'CROOM', entry.thread_id, notes['revision'], {'repo': 'snapy-cli'})
    before = snapshot(store.connection, config, 'CROOM', entry.thread_id)
    store.finish_continuation(entry, '500', 'next', None)
    assert snapshot(store.connection, config, 'CROOM', '500') == before
    assert {item['repo'] for item in task_page(config)['items']} == {'snapy-cli'}
    context = ConversationContext([], config.owner_id, '', 'next', 1, task=before['data'])
    for prompt in (conversation_prompt(entry, context, False), digest_prompt(context, 'Summarize')):
        assert '"current_task": ' + json.dumps(before['data']) in prompt



def seed_claim(config, store, message):
    entry = message(text='Use monitor for approvals.')
    store.add(entry)
    store.begin(entry, 'task', 1)
    record(store.connection, config, entry, {'claim': entry.text, 'source_event': entry.event_id})
    store.mark(entry.event_id, 'observed')
    store.connection.execute("UPDATE tasks SET status='complete'")
    store.connection.commit()
    store.bind(config.owner_id, config.workspace_id)
    return entry, snapshot(store.connection, config, 'CROOM', entry.thread_id)


def test_task_control_requires_key_and_same_origin(config, store, message):
    from aiohttp.test_utils import TestClient, TestServer
    from fridica.dashboard import create_app
    entry, notes = seed_claim(config, store, message)
    async def run():
        async with TestClient(TestServer(create_app(config, approval_key='key'))) as client:
            body = {'channel': 'CROOM', 'thread': entry.thread_id, 'revision': notes['revision'],
                    'changes': {'next_step': 'Wait for GitHub approval'}}
            assert (await client.post('/api/task-notes', json=body)).status == 403
            assert (await client.post('/api/task-notes', json=body, headers={'Authorization': 'Bearer key'})).status == 403
            headers = {'Authorization': 'Bearer key', 'Origin': str(client.make_url('/')).rstrip('/')}
            assert (await client.post('/api/task-notes', json=body, headers=headers)).status == 200
            assert (await client.post('/api/task-notes', json=body, headers=headers)).status == 409
            result = await (await client.get('/api/thread', params={'channel': 'CROOM', 'thread': entry.thread_id})).json()
            assert result['collaboration']['data']['next_step'] == body['changes']['next_step']
    asyncio.run(run())


@pytest.mark.parametrize('send', [False, True])
def test_blocked_task_stays_quiet_until_resumed(config, store, message, send):
    result = AgentResult('Need sign-in' if send else '', 'blocked', send=send,
                         update={'blocker': 'Need sign-in'})
    replica = Replica(config, store, Agent(results=[result]), Transport())
    entry = message()
    process(replica, entry)
    process(replica, message('again', timestamp='200'))
    assert len(replica.agent.responded) == 1
    assert len(replica.transport.sent) == int(send)
    row = store.task(entry)
    assert row['status'] == 'blocked'
    # Resume answers the message turned away while blocked; an older message that arrives late does not trigger.
    assert thread_action(config, 'CROOM', entry.thread_id, 'resume', row['control_revision'])['replayed'] is True
    assert store.task(entry)['status'] == 'complete'
    replica.agent.results.append(AgentResult('Signed in now.'))
    process(replica, message('again', timestamp='200'))
    assert len(replica.agent.responded) == 2 and store.get('again')['state'] == 'sent'
    process(replica, message('late', timestamp='150'))
    assert len(replica.agent.responded) == 2 and store.get('late')['decision'] == 'before_resume'


def test_reports_reject_invalid_identity_source_and_authority(config, store, message):
    entry, notes = seed_claim(config, store, message)
    other = message('other', text='Approved', timestamp='500', thread_id='500')
    store.add(other)
    store.begin(other, 'other', 1)
    store.add(message('own', sender_id=config.owner_id, text='Verified', generated=True, timestamp='200'), 'sent')
    for update in (
        {'claim': 'Approved', 'source_event': 'other'},
        {'claim': 'Verified', 'source_event': 'own'},
        {'claim': 'Approved', 'basis': 'verified'},
        {'repo': 'not-a-repo'}, {'assignee': 'Zoey'},
    ):
        with pytest.raises(ValueError):
            record(store.connection, config, entry, update)
    assert snapshot(store.connection, config, 'CROOM', entry.thread_id) == notes


def test_cleanup_waits_for_continuations_then_clears_notes(config, store, message):
    entry, notes = seed_claim(config, store, message)
    store.finish_continuation(entry, '500', 'next', None)
    thread_action(config, 'CROOM', entry.thread_id, 'archive', store.task(entry)['control_revision'])
    with pytest.raises(ValueError, match='continuation'):
        cleanup_preview(config, 'CROOM', entry.thread_id)
    thread_action(config, 'CROOM', '500', 'close', 0)
    preview = cleanup_preview(config, 'CROOM', entry.thread_id)
    thread_action(config, 'CROOM', entry.thread_id, 'clean', preview['control_revision'], preview['revision'])
    assert not store.connection.execute('SELECT 1 FROM collaboration').fetchone()
    assert not store.connection.execute('SELECT 1 FROM collaboration_history').fetchone()


def test_progress_counts_new_sources_not_new_excerpts(config, store, message):
    entry, notes = seed_claim(config, store, message)
    fresh = message('new', text='Approved on GitHub.', timestamp='200')
    store.add(fresh, 'observed')
    for source, excerpt, expected in (
        (fresh, fresh.text, 0), (fresh, 'Approved', 3), (entry, 'Use monitor', 3),
    ):
        store.connection.execute('UPDATE collaboration SET no_progress=2')
        prepare(store.connection, config, entry, AgentResult('', send=False,
                update={'claim': excerpt, 'source_event': source.event_id}))
        current = snapshot(store.connection, config, 'CROOM', entry.thread_id)
        assert current['no_progress'] == expected
        assert all(claim['basis'] == 'reported' for claim in current['data']['claims'])


def test_task_notes_cannot_change_while_lineage_is_running(config, store, message):
    entry, notes = seed_claim(config, store, message)
    store.finish_continuation(entry, '500', 'next', None)
    followup = message('run', thread_id='500', timestamp='501')
    store.add(followup)
    store.begin(followup, 'next', 1)
    with pytest.raises(ValueError, match='processing'):
        owner_update(config, 'CROOM', entry.thread_id, notes['revision'], {'next_step': 'Changed'})


def test_managed_observe_never_proposes_or_writes(config, store, message):
    from test_permissions import policy, action
    config = replace(config, file_access=True)
    plan = action('observe')
    plan['update'] = {'kind': 'ack'}
    replica = Replica(config, store, Agent(), Transport())
    replica.permissions = policy(config, store, [plan])
    process(replica, message())
    assert not replica.transport.sent
    assert not store.connection.execute('SELECT 1 FROM file_requests').fetchone()


def test_file_proposal_cannot_be_hidden_by_ack_kind(config, store, message):
    from test_permissions import policy, action
    config = replace(config, file_access=True)
    plan = action('write', config.workspace / 'new.txt', 'hello')
    plan['update'] = {'kind': 'ack'}
    replica = Replica(config, store, Agent(), Transport())
    replica.permissions = policy(config, store, [plan])
    process(replica, message())
    assert len(replica.transport.sent) == 1
    assert store.task(message())['status'] == 'waiting'
    assert not (config.workspace / 'new.txt').exists()


def test_new_peer_result_is_considered_even_if_peer_marked_complete(config, store, message):
    replica = Replica(config, store, Agent(results=[AgentResult('What did the test report?', 'waiting'), AgentResult('Result recorded.')]), Transport())
    process(replica, message())
    process(replica, message('peer-result', timestamp='200', text='<@UOWNER> test exit 0', generated=True, task_status='complete'))
    assert len(replica.agent.responded) == 2
    assert replica.transport.sent[-1][1].text == 'Result recorded.'


@pytest.mark.parametrize('side', [0, 1])
def test_owner_correction_resolves_dispute_and_stays_authoritative(config, store, message, side):
    entry, notes = seed_claim(config, store, message)
    old = notes['data']['claims'][0]['id']
    reply = AgentResult('Conflicting advice.', update={'claim': 'Use dashboard.',
                        'source_event': 'correction', 'corrects': old})
    replica = Replica(config, store, Agent(results=[reply]), Transport())
    process(replica, message('correction', text='<@UOWNER> Use dashboard.', timestamp='200'))
    assert store.task(entry)['status'] == 'blocked'
    notes = snapshot(store.connection, config, 'CROOM', entry.thread_id)
    assert all(c['state'] == 'disputed' for c in notes['data']['claims'])
    owner_update(config, 'CROOM', entry.thread_id, notes['revision'], {'repo': 'snapy-cli'},
                 {'id': notes['data']['claims'][side]['id'], 'text': 'Confirmed dashboard command.', 'evidence': 'CLI help'})
    claims = snapshot(store.connection, config, 'CROOM', entry.thread_id)['data']['claims']
    assert [c['state'] for c in claims] == ['superseded', 'superseded', 'current']
    assert claims[-1]['basis'] == 'owner_confirmed'
    assert store.connection.execute('SELECT actor FROM collaboration_history ORDER BY id DESC').fetchone()[0] == config.owner_id
    assert store.task(entry)['status'] == 'blocked'
    assert store.connection.execute('SELECT count(*) FROM grants').fetchone()[0] == 0
    record(store.connection, config, entry, {'repo': 'snapy-xiz'})
    assert snapshot(store.connection, config, 'CROOM', entry.thread_id)['data']['repo'] == 'snapy-cli'
    with pytest.raises(ValueError, match='superseded'):
        record(store.connection, config, entry, {'claim': 'Use dashboard.', 'source_event': 'correction', 'corrects': old})


@pytest.mark.parametrize('finished', [False, True])
def test_owner_edits_wait_for_digest(config, store, message, finished):
    class DigestAgent(Agent):
        async def summarize(self, context):
            notes = snapshot(store.connection, config, 'CROOM', message().thread_id)
            with pytest.raises(ValueError, match='processing'):
                owner_update(config, 'CROOM', message().thread_id, notes['revision'], {'next_step': 'Changed'})
            return 'Summary.'
        debrief = summarize
    agent = DigestAgent(results=[AgentResult('Done', session='old-session', finished=finished)])
    process(Replica(replace(config, max_turns=1), store, agent, Transport()), message())
    assert len(agent.responded) == 1
    notes = snapshot(store.connection, config, 'CROOM', message().thread_id)
    owner_update(config, 'CROOM', message().thread_id, notes['revision'], {'next_step': 'Changed'})
    assert all(row['session'] is None for row in store.connection.execute('SELECT session FROM tasks'))


def test_correction_just_before_summary_claim_detaches_child_session(config, store, message):
    original = store.begin_continuation
    def claim(entry):
        notes = snapshot(store.connection, config, 'CROOM', entry.thread_id)
        owner_update(config, 'CROOM', entry.thread_id, notes['revision'], {'next_step': 'Updated step'})
        return original(entry)
    store.begin_continuation = claim
    agent = Agent(results=[AgentResult('Done', session='old-session')])
    process(Replica(replace(config, max_turns=1), store, agent, Transport()), message())
    assert all(row['session'] is None for row in store.connection.execute('SELECT session FROM tasks'))


# ---- regression: a note defect must never cost the reply (snapy thread, 2026-09-22, "chen sihe" as assignee) ----

def test_display_name_assignee_drops_the_field_not_the_reply(config, store, message, caplog):
    entry = message(sender_id='U05N9MASG9X', text='grav1 PR is up. Honest result: *this test would not have caught the #206 bug.*')
    answer = AgentResult("As snapy-cli owner I'm holding #207 for now.", update={
        'repo': 'snapy-cli', 'assignee': 'chen sihe', 'kind': 'result',
        'next_step': 'Add the decomposed moist-wall case to #207',
        'claim': 'Honest result: *this test would not have caught the #206 bug.*', 'source_event': entry.event_id,
    })
    replica = Replica(config, store, Agent(results=[answer]), Transport())
    import logging
    with caplog.at_level(logging.WARNING, logger='fridica.collaboration'):
        process(replica, entry)
    assert [sent[1].text for sent in replica.transport.sent] == ["As snapy-cli owner I'm holding #207 for now."]
    assert replica.transport.sent[0][1].status == 'complete'
    assert store.task(entry)['status'] == 'complete'
    notes = snapshot(store.connection, config, 'CROOM', entry.thread_id)['data']
    assert notes['repo'] == 'snapy-cli' and 'assignee' not in notes
    assert notes['next_step'].startswith('Add the decomposed') and notes['claims'][0]['sender'] == 'U05N9MASG9X'
    assert "assignee 'chen sihe' is not a known Slack member ID; dropped" in caplog.text
    assert 'could not be validated' not in caplog.text


def test_assignee_resolves_from_id_mention_or_unique_cached_name(config, store, message):
    from fridica.collaboration import resolve_assignee
    from fridica.dashboard_control import metadata, save_metadata
    store.add(message())  # UALICE has posted in CROOM
    store.bind(config.owner_id, config.workspace_id)
    db = store.connection
    assert resolve_assignee(db, config, 'CROOM', 'UALICE') == 'UALICE'
    assert resolve_assignee(db, config, 'CROOM', '<@UALICE>') == 'UALICE'
    assert resolve_assignee(db, config, 'CROOM', '<@UALICE|Alice Example>') == 'UALICE'
    assert resolve_assignee(db, config, 'CROOM', config.owner_id) == config.owner_id
    assert resolve_assignee(db, config, 'CROOM', 'UNOBODY') is None, 'an ID that never posted here is not accepted'
    assert resolve_assignee(db, config, 'CROOM', 'Alice Example') is None, 'no name cache yet'
    cache = metadata(config)
    cache['names'] = {'UALICE': 'Alice Example', 'UBOB': 'Alice  Example', 'UCAT': 'Cat'}
    save_metadata(config, cache)
    assert resolve_assignee(db, config, 'CROOM', 'alice example') is None, 'two members share the name; do not guess'
    cache['names'] = {'UALICE': 'Alice Example', 'UCAT': 'Cat'}
    save_metadata(config, cache)
    assert resolve_assignee(db, config, 'CROOM', 'ALICE   example') == 'UALICE'
    assert resolve_assignee(db, config, 'CROOM', 'Cat') is None, 'cached but never posted in the channel'
    assert resolve_assignee(db, config, 'CROOM', '') is None and resolve_assignee(db, config, 'CROOM', None) is None


def test_sanitize_salvages_what_it_can(config, store, message):
    from fridica.collaboration import sanitize
    entry = message(text='Approved by Alice')
    store.add(entry)
    store.begin(entry, 'task', 1)
    own = message('own', sender_id=config.owner_id, text='Verified', generated=True, timestamp='200')
    store.add(own, 'sent')
    update = {
        'repo': 'SNAPY-CLI', 'assignee': 'Zoey', 'kind': 'shout', 'owner': 'x', 'blocker': 42,
        'next_step': 'x' * 1200, 'claim': 'Approved by Bob', 'source_event': entry.event_id, 'corrects': 'nope',
    }
    clean, problems = sanitize(store.connection, config, entry, update)
    assert clean == {'repo': 'snapy-cli', 'kind': 'result', 'next_step': 'x' * 1000, 'source_event': entry.event_id}
    joined = ' | '.join(problems)
    for expected in ("unknown field 'owner'", 'blocker was not text', 'next_step truncated', "kind 'shout' replaced",
                     "assignee 'Zoey' is not a known", 'not an exact excerpt'):
        assert expected in joined, expected
    clean, problems = sanitize(store.connection, config, entry, {'claim': 'Verified', 'source_event': 'own'})
    assert 'claim' not in clean and any('own generated reply' in p for p in problems)
    clean, problems = sanitize(store.connection, config, entry, {'claim': 'Approved by Alice', 'source_event': 'missing'})
    assert 'claim' not in clean and any('no valid source_event' in p for p in problems)
    assert sanitize(store.connection, config, entry, ['not', 'a', 'dict']) == ({}, ['update was not an object'])
    clean, problems = sanitize(store.connection, config, entry, {'claim': 'Approved by Alice', 'source_event': entry.event_id, 'assignee': 'UALICE'})
    assert clean == {'claim': 'Approved by Alice', 'source_event': entry.event_id, 'assignee': 'UALICE'} and problems == []


def test_note_recording_failure_never_blocks_the_reply(config, store, message, monkeypatch, caplog):
    import logging
    from fridica import collaboration
    def explode(*args, **kwargs):
        raise ValueError('Review or clear task notes before adding more claims')
    monkeypatch.setattr(collaboration, 'record', explode)
    answer = AgentResult('Done.', update={'next_step': 'ship it'})
    replica = Replica(config, store, Agent(results=[answer]), Transport())
    entry = message()
    with caplog.at_level(logging.WARNING, logger='fridica.collaboration'):
        process(replica, entry)
    assert [sent[1].text for sent in replica.transport.sent] == ['Done.']
    assert store.task(entry)['status'] == 'complete'
    assert 'were not recorded (Review or clear task notes' in caplog.text
