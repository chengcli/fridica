import asyncio
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace

import pytest
import tomlkit
from aiohttp.test_utils import TestClient, TestServer

from fridica.config import load_config
from fridica.dashboard import create_app
from fridica.settings import read_settings, save_settings, update_grant
from fridica.permissions import Permissions
from fridica.replica import Replica


@pytest.fixture
def configured(config, store):
    config = replace(config, file_access=True, model='gpt-5.6-luna', reasoning_effort='medium')
    path = config.state_path.parent / 'config.toml'
    path.write_text(tomlkit.dumps(dict(owner_id=config.owner_id, workspace_id=config.workspace_id,
        channels=list(config.channels), workspace=str(config.workspace), state_path=str(config.state_path),
        file_access=True, model=config.model, reasoning_effort=config.reasoning_effort)))
    store.bind(config.owner_id, config.workspace_id)
    return load_config(path), path


def test_settings_save_validate_and_reject_stale_revision(configured):
    config, path = configured
    revision = read_settings(config, path)['revision']
    save_settings(config, path, {'model':'gpt-5.6-sol','max_wait_replies':4}, revision)
    assert load_config(path).model == 'gpt-5.6-sol'
    assert load_config(path).max_wait_replies == 4
    with pytest.raises(ValueError, match='changed'):
        save_settings(config, path, {'model':'stale'}, revision)
    original = path.read_text()
    for patch in ({'channels':['COTHER']}, {'max_wait_replies':0}, {'reasoning_effort':'invalid'}, {'model':'--anything'}):
        with pytest.raises(ValueError):
            save_settings(config, path, patch, read_settings(config,path)['revision'])
        assert path.read_text() == original


def test_directory_boundaries_and_scoped_grant_revocation(configured, message):
    config, path = configured
    folder = path.parent / 'extra';folder.mkdir()
    save_settings(config,path,{'additional_workspaces':[str(folder)]},read_settings(config,path)['revision'])
    new = load_config(path)
    grant = update_grant(new, {'action':'grant','sender':'UALICE','channel':'CROOM','path':str(folder),'ttl':3600})
    from fridica.dashboard_control import database
    with database(new) as db:
        permissions = Permissions(new,SimpleNamespace(connection=db))
        assert permissions.allowed(message(),str(folder/'new.txt'))
        assert not permissions.allowed(message(sender_id='UBOB'),str(folder/'new.txt'))
    update_grant(new, {'action':'revoke','id':grant['id']})
    with database(new) as db:
        assert not Permissions(new,SimpleNamespace(connection=db)).allowed(message(),str(folder/'new.txt'))
    for root in (str(Path.home()),str(path.parent),str(folder/'missing')):
        with pytest.raises(ValueError):
            save_settings(new,path,{'additional_workspaces':[root]},read_settings(new,path)['revision'])
    alias = path.parent/'alias';alias.symlink_to(folder,target_is_directory=True)
    with pytest.raises(ValueError):
        save_settings(new,path,{'additional_workspaces':[str(alias)]},read_settings(new,path)['revision'])
    with pytest.raises(ValueError):
        update_grant(new,{'action':'grant','sender':'UALICE','channel':'COTHER','path':str(folder),'ttl':3600})


def test_listener_reloads_settings_at_request_boundary(configured, store):
    config,path=configured
    replica=Replica(config,store,None,SimpleNamespace(),observe_only=True,config_path=path)
    replica.reload_config()
    save_settings(config,path,{'model':'gpt-5.6-sol'},read_settings(config,path)['revision'])
    assert replica.config.model=='gpt-5.6-luna'
    replica.reload_config()
    assert replica.config.model=='gpt-5.6-sol'
    assert read_settings(config,path)['applied_revision']==read_settings(config,path)['revision']


def test_settings_and_grants_require_owner_auth(configured):
    config,path=configured
    async def run():
        async with TestClient(TestServer(create_app(config,approval_key='owner',config_path=path))) as client:
            assert (await client.get('/api/settings')).status==403
            assert (await client.post('/api/settings',json={})).status==403
            assert (await client.post('/api/grants',json={})).status==403
            headers={'Authorization':'Bearer owner'}
            current=await (await client.get('/api/settings',headers=headers)).json()
            response=await client.post('/api/settings',headers={**headers,'Origin':str(client.make_url('/')).rstrip('/')},json={'revision':current['revision'],'changes':{'model':'gpt-5.6-sol'}})
            assert response.status==200
            assert (await (await client.get('/api/state')).json())['config']['model']=='gpt-5.6-sol'
    asyncio.run(run())


def test_settings_values_and_revision_are_from_one_snapshot(configured, monkeypatch):
    config,path=configured
    import fridica.settings as settings
    original_revision=settings.fingerprint(path)
    original_loader=settings.load_config
    def concurrent_edit(source, **kwargs):
        result=original_loader(source,**kwargs)
        document=tomlkit.parse(path.read_text());document['model']='gpt-5.6-sol'
        path.write_text(tomlkit.dumps(document))
        return result
    monkeypatch.setattr(settings,'load_config',concurrent_edit)
    result=read_settings(config,path)
    assert result['values']['model']=='gpt-5.6-luna'
    assert result['revision']==original_revision
    assert result['revision']!=settings.fingerprint(path)


def test_stale_grant_request_cannot_restore_removed_scope(configured):
    config,path=configured
    extra=path.parent/'extra';extra.mkdir()
    save_settings(config,path,{'additional_workspaces':[str(extra)]},read_settings(config,path)['revision'])
    stale=load_config(path)
    save_settings(config,path,{'additional_workspaces':[]},read_settings(config,path)['revision'])
    with pytest.raises(ValueError,match='outside'):
        update_grant(stale,{'action':'grant','sender':'UALICE','channel':'CROOM','path':str(extra),'ttl':3600},config_path=path)


def test_invalid_config_at_message_boundary_leaves_work_recoverable(configured, store, message):
    config,path=configured
    replica=Replica(config,store,None,SimpleNamespace(),observe_only=True,config_path=path)
    msg=message();store.add(msg)
    original=path.read_text();path.write_text('invalid = [')
    asyncio.run(replica.process(msg))
    assert store.get(msg.event_id)['state']=='pending'
    path.write_text(original)
    asyncio.run(replica.process(msg))
    assert store.get(msg.event_id)['state']=='observed'


def test_native_mode_loop_edit_preserves_default_model_and_symlink(configured):
    config,path=configured
    alias=path.parent/'native-alias';alias.symlink_to(config.workspace,target_is_directory=True)
    document=tomlkit.parse(path.read_text());document['file_access']=False;document['workspace']=str(alias)
    document.pop('model');document.pop('reasoning_effort');path.write_text(tomlkit.dumps(document))
    native=load_config(path)
    save_settings(native,path,{'max_turns':9,'model':None,'reasoning_effort':None},read_settings(native,path)['revision'])
    current=load_config(path)
    assert current.max_turns==9 and current.workspace==alias
    assert current.model is None and current.reasoning_effort is None
    with pytest.raises(ValueError,match='managed'):
        save_settings(native,path,{'workspace':str(config.workspace)},read_settings(native,path)['revision'])


def test_reload_updates_codex_command_and_managed_file_adapter(configured, store):
    from fridica.agents import CodexBackend
    config,path=configured
    config=replace(config,backend='codex')
    document=tomlkit.parse(path.read_text());document['backend']='codex';path.write_text(tomlkit.dumps(document))
    replica=Replica(config,store,CodexBackend(config),SimpleNamespace(),config_path=path)
    save_settings(config,path,{'model':'gpt-5.6-sol','reasoning_effort':'high'},read_settings(config,path)['revision'])
    replica.reload_config()
    assert replica.permissions.agent is replica.agent
    assert replica.permissions.config==replica.config
    command=replica.agent.command(path.parent,path.parent/'schema.json',True)
    assert command[command.index('--model')+1]=='gpt-5.6-sol'
    assert 'model_reasoning_effort="high"' in command


def test_directory_change_revokes_only_configured_channel_grants(configured, store):
    config, path = configured
    extra = path.parent / 'extra'
    extra.mkdir()
    save_settings(config, path, {'additional_workspaces': [str(extra)]}, read_settings(config, path)['revision'])
    expanded = load_config(path)
    manager = Permissions(replace(expanded, channels=('CROOM', 'COTHER')), store)
    local = manager.grant('UALICE', 'CROOM', str(extra), None)
    other = manager.grant('UBOB', 'COTHER', str(extra), None)
    kept = manager.grant('UALICE', 'CROOM', str(config.workspace), None)

    save_settings(config, path, {'additional_workspaces': []}, read_settings(config, path)['revision'])

    grants = dict(store.connection.execute('SELECT id,revoked FROM grants'))
    assert grants == {local: 1, other: 0, kept: 0}


def test_idle_reload_skips_unchanged_parsing_but_requests_revalidate(configured, store, message, monkeypatch):
    import fridica.settings as settings

    config, path = configured
    replica = Replica(config, store, None, SimpleNamespace(), observe_only=True, config_path=path)
    replica.reload_config()
    loader = settings.load_config
    parsed = []
    def counted_load(*args, **kwargs):
        parsed.append(args[0])
        return loader(*args, **kwargs)
    monkeypatch.setattr(settings, 'load_config', counted_load)
    for _ in range(4):
        replica.reload_config(idle=True)
    assert parsed == []

    document = tomlkit.parse(path.read_text())
    document['max_turns'] = 8
    path.write_text(tomlkit.dumps(document))
    replica.reload_config(idle=True)
    assert replica.config.max_turns == 8
    assert parsed == [path]

    config.workspace.rmdir()
    replica.receive(message())
    asyncio.run(replica.process(message()))
    assert store.get(message().event_id)['state'] == 'pending'
