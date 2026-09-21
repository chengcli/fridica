import asyncio
import time

from aiohttp.test_utils import TestClient, TestServer
import pytest

from fridica.activity import activity_page, move_activity
from fridica.dashboard import create_app


def test_archive_is_reversible_scoped_and_keeps_event_state(config, store, message):
    store.bind(config.owner_id,config.workspace_id)
    for identifier,ts,channel in [('old','100.000001','CROOM'),('new','300.000001','CROOM'),('other','100.000001','COTHER')]:
        store.add(message(identifier,timestamp=ts,thread_id=ts,channel_id=channel),'observed')
    before=[tuple(r) for r in store.connection.execute('SELECT * FROM events ORDER BY event_id')]
    preview=activity_page(config,before=200)
    assert preview['movable']==1 and preview['counts']=={'current':2,'archived':0}
    move_activity(config,'archive',200)
    assert activity_page(config)['total']==1
    archived=activity_page(config,view='archived')
    assert archived['total']==1 and archived['items'][0]['id']=='message:old'
    assert [tuple(r) for r in store.connection.execute('SELECT * FROM events ORDER BY event_id')]==before
    move_activity(config,'restore')
    assert activity_page(config)['total']==2
    assert activity_page(config,view='archived')['total']==0


def test_archive_includes_decisions_and_paginates(config,store,message):
    store.bind(config.owner_id,config.workspace_id)
    for i in range(60):
        store.add(message(str(i),timestamp=f'{100+i}.000001',thread_id=f'{100+i}.000001'),'observed')
    with store.connection:
        store.connection.execute("INSERT INTO thread_decisions VALUES(?,?,?,?,?)",(config.workspace_id,'CROOM','100.000001','close',120))
    move_activity(config,'archive',200)
    page=activity_page(config,view='archived',offset=50)
    assert page['total']==61 and len(page['items'])==11
    assert activity_page(config,before=200)['movable']==0
    for before in (-1,float('inf'),float('nan'),True,time.time()+100):
        with pytest.raises(ValueError):move_activity(config,'archive',before)


def test_archive_http_requires_owner_and_masks_secrets(config,store,message):
    store.bind(config.owner_id,config.workspace_id)
    store.add(message(text='hello xoxp-123456-SECRET'),'observed')
    async def run():
        async with TestClient(TestServer(create_app(config,approval_key='owner'))) as client:
            assert (await client.post('/api/activity/archive',json={'action':'archive','before':200})).status==403
            origin=str(client.make_url('/')).rstrip('/')
            headers={'Authorization':'Bearer owner','Origin':origin}
            assert (await client.post('/api/activity/archive',headers=headers,json={'action':'archive','before':200})).status==200
            data=await (await client.get('/api/activity?view=archived')).json()
            assert data['total']==1 and 'SECRET' not in str(data)
    asyncio.run(run())
