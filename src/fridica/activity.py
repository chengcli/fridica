import math
import time

from .dashboard_control import database


def cutoff(value):
    if isinstance(value,bool) or not isinstance(value,(int,float)) or not math.isfinite(value) or not 0 < value <= time.time():
        raise ValueError('Choose a cutoff date in the past.')
    return value


def activity_page(config, *, view='current', offset=0, before=None):
    if view not in {'current','archived'}:
        raise ValueError('Choose Current or Archived activity.')
    if before is not None: cutoff(before)
    result={'items':[],'total':0,'counts':{'current':0,'archived':0},'movable':0}
    if not config.state_path.exists():return result
    with database(config) as db:
        marks=','.join('?' for _ in config.channels)
        sql=f"SELECT 'message:'||event_id id,channel,timestamp,'message' kind,json_extract(payload,'$.sender_id') sender,json_extract(payload,'$.text') text,state status FROM events WHERE workspace=? AND channel IN ({marks}) AND decision IS NOT 'cleaned'"
        args=(config.workspace_id,*config.channels)
        tables={r['name'] for r in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        if 'dashboard_decisions' in tables:
            sql+=f" UNION ALL SELECT 'decision:'||d.request_id,e.channel,d.decided_at,'decision',d.owner,r.operation||' '||r.path,d.decision FROM dashboard_decisions d JOIN file_requests r ON r.id=d.request_id JOIN events e ON e.event_id=r.event_id WHERE e.workspace=? AND e.channel IN ({marks})"
            args+=(config.workspace_id,*config.channels)
        if 'thread_decisions' in tables:
            sql+=f" UNION ALL SELECT 'control:'||rowid,channel,decided_at,'control',?,action||' request',action FROM thread_decisions WHERE workspace=? AND channel IN ({marks})"
            args+=(config.owner_id,config.workspace_id,*config.channels)
        if 'activity_archives' in tables:
            sql='SELECT c.*,coalesce(a.before,0) archived_before FROM ('+sql+') c LEFT JOIN activity_archives a ON a.channel=c.channel AND a.workspace=?'
            args+=(config.workspace_id,)
        else:sql='SELECT c.*,0 archived_before FROM ('+sql+') c'
        sql="WITH activity AS ("+sql+") "
        for row in db.execute(sql+"SELECT CASE WHEN timestamp<archived_before THEN 'archived' ELSE 'current' END view,count(*) n FROM activity GROUP BY view",args):
            result['counts'][row['view']]=row['n']
        if before is not None:
            result['movable']=db.execute(sql+'SELECT count(*) FROM activity WHERE timestamp<? AND timestamp>=archived_before',(*args,before)).fetchone()[0]
        result['total']=result['counts'][view]
        condition='timestamp<archived_before' if view=='archived' else 'timestamp>=archived_before'
        result['items']=[dict(row) for row in db.execute(sql+'SELECT * FROM activity WHERE '+condition+' ORDER BY timestamp DESC,id DESC LIMIT 50 OFFSET ?',(*args,max(0,offset)))]
    return result


def move_activity(config, action, before=None):
    if action not in {'archive','restore'}:raise ValueError('Choose archive or restore.')
    if action=='archive':cutoff(before)
    with database(config,write=True) as db:
        db.execute('CREATE TABLE IF NOT EXISTS activity_archives (workspace TEXT,channel TEXT,before REAL NOT NULL,PRIMARY KEY(workspace,channel))')
        for channel in config.channels:
            if action=='restore':
                db.execute('DELETE FROM activity_archives WHERE workspace=? AND channel=?',(config.workspace_id,channel))
            else:
                db.execute('INSERT INTO activity_archives VALUES(?,?,?) ON CONFLICT(workspace,channel) DO UPDATE SET before=max(before,excluded.before)',(config.workspace_id,channel,before))
        db.commit()
    return {'status':'archived' if action=='archive' else 'restored'}
