'use strict';
const $ = id => document.getElementById(id);
let state, page, selected, detail, review, cleanupReview, reviewBusy = false;
let view = 'overview', filter = 'all', senderFilter = '', offset = 0, historyOffset = 0, historyItems = [];
let unlocked = false, stopped = false, busy = false, queuedRefresh = false, refreshTimer, searchTimer;
let activityView='current', historyRequest=0, refreshRequest=0;
let editor, editorView, filledRevision, settingsAction, settingsSaving=false;
let layout = sessionStorage.getItem('fridica-layout') || 'list';
let key = sessionStorage.getItem('fridica-key') || '';
const fragment = new URLSearchParams(location.hash.slice(1));
if (fragment.has('key')) { key = fragment.get('key'); sessionStorage.setItem('fridica-key', key); history.replaceState(null, '', location.pathname); }
const labels = {continued:'Continued in a new thread',finished:'Finished · debrief posted',debriefed:'Debrief posted',paused:'Paused · needs you',closed:'Closed locally',archived:'Archived',cleaned:'Contents cleared',resume:'Resumed',close:'Closed',archive:'Archived',restore:'Restored',clean:'Contents cleared',awaiting_approval:'Awaiting approval',pending:'Queued',running:'Processing',ready:'Delivery queued',sending:'Sending',approved:'Approved · queued',applying:'Applying',delivery_pending:'Delivery queued',waiting:'Waiting for information',complete:'Completed',failed:'Failed',interrupted:'Interrupted',ambiguous:'Delivery unconfirmed',blocked:'Needs attention',rejected:'Rejected · notifying',declined:'Rejected',sent:'Delivered',observed:'Recorded',ignored:'No action needed',connected:'Slack connected',reconnecting:'Slack reconnecting',connecting:'Slack connecting',offline:'Listener heartbeat lost',stopped:'Listener stopped',unknown:'Connection unknown'};
const bucket = s => ['paused','awaiting_approval','failed','interrupted','ambiguous','blocked'].includes(s) ? 'attention' : ['pending','running','ready','sending','approved','applying','rejected','delivery_pending'].includes(s) ? 'active' : s === 'waiting' ? 'waiting' : 'complete';
function el(tag, text, cls) { const n = document.createElement(tag); if (text !== undefined) n.textContent = text; if (cls) n.className = cls; return n; }
function button(text, fn, cls) { const b = el('button',text,cls); b.type='button'; b.onclick=fn; return b; }
function status(s) { return el('span', labels[s] || s, `status ${bucket(s)} ${s}`); }
function name(id) { return state?.names[id] || (id === state?.config.owner ? 'My account' : id?.startsWith('C') || id?.startsWith('G') ? 'Configured channel' : 'Unknown member'); }
function text(value='') { return value.replace(/<@([A-Z0-9]+)(?:\|([^>]+))?>/g,(_,id,label)=>'@'+(state?.names[id] || label || name(id))).replace(/<#([A-Z0-9]+)(?:\|[^>]+)?>/g,(_,id)=>name(id)).replace(/&amp;/g,'&').replace(/&lt;/g,'<').replace(/&gt;/g,'>'); }
function ago(t) { const s=Math.max(0,Math.floor(Date.now()/1000-t)); return s<60?'Just now':s<3600?Math.floor(s/60)+'m ago':s<86400?Math.floor(s/3600)+'h ago':Math.floor(s/86400)+'d ago'; }
function when(t) { return t ? new Date(t*1000).toLocaleString() : 'No records yet'; }
function notify(message, error=false) { const target=$(error?'error':'notice'); target.textContent=message; target.hidden=false; if (!error) setTimeout(()=>target.hidden=true,7000); }
async function api(path, body) {
  const headers = key ? {Authorization:'Bearer '+key} : {};
  const options = {headers,cache:'no-store',signal:AbortSignal.timeout(8000)};
  if (body !== undefined) { options.method='POST'; headers['Content-Type']='application/json'; options.body=JSON.stringify(body); }
  const response=await fetch(path,options);
  let value; try { value=await response.json(); } catch { throw new Error(response.status===403?'Local controls are locked or your access key has expired.':'The service is unavailable. Check the connection.'); }
  if (!response.ok) throw new Error(value.error || 'The action did not complete. Refresh its status before trying again.');
  return value;
}
function projectFor(task) { return state.projects.find(p=>p.id===task.project_id); }
function repositoryLabel(task) { return task.repo || projectFor(task)?.name || 'No repository linked'; }
function title(task) { const value=text((task.title || 'Request to review').replace(/^(?:<@[A-Z0-9]+(?:\|[^>]+)?>\s*)+[—–,:-]?\s*/, '')).trim(); return value.split(/(?<=[?。？!！])\s+/)[0].slice(0,180); }
function setView(next, nextFilter, nextSender = '') {
  senderFilter=nextSender;
  view=next; offset=0; selected=null; detail=null;
  window.scrollTo(0,0);
  filter=nextFilter || (view==='inbox'?'attention':'all');
  $('search').value='';
  document.querySelectorAll('.nav').forEach(n=>{n.classList.toggle('active',n.dataset.view===view);if(n.dataset.view===view)n.setAttribute('aria-current','page');else n.removeAttribute('aria-current');});
  const headings={overview:'Overview',inbox:'Inbox',tasks:'Requests',projects:'Projects & access',history:'Activity',settings:'Settings'};
  $('heading').textContent=headings[view];
  $('task-view').hidden=!['inbox','tasks'].includes(view);
  for (const id of ['overview','projects','history','settings']) $(id+'-view').hidden=view!==id;
  $('detail').hidden=true;
  if (view==='history') { historyOffset=0; historyItems=[]; }
  refresh();
}
function renderShell() {
  const counts=state.summary;
  $('inbox-count').textContent=counts.attention;
  $('task-count').textContent=counts.all;
  $('owner-name').textContent=name(state.config.owner);
  $('owner-avatar').textContent=name(state.config.owner).slice(0,1).toUpperCase();
  $('channel-name').textContent=state.config.channels.map(name).join(' · ');
  $('breadcrumb').textContent=state.config.channels.map(name).join(' · ');
  $('connection').textContent=labels[state.health.status] || 'Connection unknown';
  $('connection').className='connection '+state.health.status;
  $('mode-label').textContent=unlocked?'Local controls unlocked':'Read-only mode';
  $('refresh-state').textContent='Updated '+new Date().toLocaleTimeString();
  const bad=state.health.status!=='connected' || !!state.health.observe_only;
  $('system-alert').hidden=!bad;
  if (bad) $('system-alert').replaceChildren(el('span', state.health.observe_only?'The listener is in observe-only mode. It records messages but does not execute approvals or send replies.':`${labels[state.health.status] || 'Connection unknown'}. Decisions remain saved; execution and notifications require an active listener.`),button('Connection details',()=>setView('settings')));
}
function renderOverview() {
  const people=[...new Set(page.items.map(t=>t.sender).filter(Boolean))];
  const avatars=el('span',undefined,'avatar-stack');
  for(const [i,id] of people.slice(0,4).entries()) {const avatar=el('span',name(id).slice(0,1).toUpperCase(),'avatar person-'+i);avatar.title=name(id);avatars.append(avatar);}
  if(people.length>4) avatars.append(el('span','+'+(people.length-4),'avatar'));
  $('people-summary').replaceChildren(avatars,el('span',`${people.length} recent requester${people.length===1?'':'s'}`));
  $('people-list').replaceChildren(el('small','Filter requests by person'),...people.map(id=>button(name(id)+' →',()=>setView('tasks','all',id))));
  const cards=[['attention','Needs your attention','Approvals and issues to review'],['active','In progress','Queued, working, or notifying'],['waiting','Waiting for information','A reply is needed to continue'],['complete','Finished','Completed, declined, or closed']];
  $('scorecards').replaceChildren(...cards.map(([id,label,description])=>{
    const card=button('',()=>setView('tasks',id),'scorecard '+id);
    card.append(el('span',label),el('strong',state.summary[id]),el('small',description));
    return card;
  }));
  $('recent-requests').replaceChildren(...page.items.slice(0,5).map(task=>{
    const row=button('',()=>{setView('tasks');openTask(task);},'recent-request');
    row.append(el('strong',title(task)),el('small',`${name(task.sender)} · ${ago(task.updated)}`),status(task.status));return row;
  }));
  if (!page.items.length) $('recent-requests').append(el('p','New requests will appear here.','panel-empty'));
  const info=$('workspace-summary');info.replaceChildren();
  const facts=el('dl',undefined,'workspace-facts');
  for(const [label,value] of [['Channel',state.config.channels.map(name).join(', ')],['Repositories linked',state.projects.length],['Configured directories',state.config.write_roots.length+state.config.read_roots.length],['Write grants shown',state.grants.length],['Controls',unlocked?'Unlocked':'Read-only']]) facts.append(el('dt',label),el('dd',value));
  info.append(facts);
  if(!state.projects.length) info.append(el('p','No repository linked. Add a label in Projects & access to identify the project behind each request.','panel-empty'));
  else for(const project of state.projects) info.append(button(project.name,()=>setView('projects'),'project-shortcut'));
}
function renderLayout() {
  const cards=view==='tasks' && layout==='cards';
  document.querySelector('.task-list').classList.toggle('card-layout',cards);
  $('layout-switch').hidden=view!=='tasks';
  $('list-layout').setAttribute('aria-pressed',String(!cards));
  $('card-layout').setAttribute('aria-pressed',String(cards));
}
function renderTasks() {
  renderLayout();
  if (document.activeElement!==$('sender-filter')) {
    const ids=[...new Set([...page.senders,...(senderFilter?[senderFilter]:[])])];
    $('sender-filter').replaceChildren(...['',...ids].map(id=>{const option=el('option',id?name(id):'All requesters');option.value=id;return option;}));
    $('sender-filter').value=senderFilter;
  }
  $('clear-sender').hidden=!senderFilter;
  $('intro').textContent=view==='inbox'?'Decisions and issues that need your attention. Requests waiting for information are separate.':'Each request brings its conversation, operations, and results together.';
  const tabs=view==='inbox'?[['attention','Needs attention'],['waiting','Waiting']]:[['all','All'],['active','In progress'],['waiting','Waiting'],['complete','Finished'],['attention','Needs attention'],['archived','Archived']];
  $('tabs').replaceChildren(...tabs.map(([id,label])=>{const b=button(label,()=>{filter=id;offset=0;selected=null;$('detail').hidden=true;refresh();},'tab'+(filter===id?' selected':'')); b.setAttribute('aria-pressed',String(filter===id));b.append(el('span',page.counts[id]));return b;}));
  const rows=page.items.map(task=>{
    const row=button('',()=>openTask(task),'task-row'+(selected?.channel===task.channel && selected?.thread===task.thread?' selected':''));
    const main=el('div',undefined,'task-main');
    main.append(el('div',title(task),'task-title'));
    const meta=el('div',undefined,'task-meta');
    meta.append(el('span',name(task.sender)),el('span','·'),el('span',repositoryLabel(task)));
    if (task.approvals) meta.append(el('span',`· ${task.approvals} approvals pending`));
    main.append(meta);
    const side=el('div',undefined,'task-side'); side.append(status(task.status),el('time',ago(task.updated)));
    row.append(el('span',name(task.sender).slice(0,1).toUpperCase(),'avatar'),main,side);
    return row;
  });
  if (!rows.length) {
    const empty=el('div',undefined,'empty');
    if (senderFilter || $('search').value) {
      empty.append(el('h2','No matching requests'),el('p','Try another requester, status, or search.'));
    } else {
    empty.append(el('h2',filter==='attention'?'Nothing needs your attention':'No requests here yet'),el('p',state.summary.waiting ? `${state.summary.waiting} requests are waiting for information. New replies will update their status here.`:'New requests and decisions will appear here.'));
    if (filter==='attention' && state.summary.waiting) empty.append(button('View waiting requests',()=>{filter='waiting';offset=0;refresh();}));
    }
    rows.push(empty);
  }
  $('task-list').replaceChildren(...rows);
  $('page-info').textContent=page.total?`${offset+1}–${offset+page.items.length} / ${page.total} requests`:'0 requests';
  $('previous').disabled=offset===0; $('next').disabled=offset+page.items.length>=page.total;
}
async function openTask(task) {
  selected=task; detail=null; $('detail').hidden=false;
  $('detail-body').replaceChildren(el('p','Loading request…','muted'));
  renderTasks();
  try { await loadDetail(task); } catch(e) { notify(e.message,true); }
}
async function loadDetail(task) {
  const result=await api(`/api/thread?channel=${encodeURIComponent(task.channel)}&thread=${encodeURIComponent(task.thread)}`);
  if (selected!==task) return;
  detail=result;
  renderDetail();
}
function section(titleText) {const s=el('section',undefined,'detail-section');s.append(el('h3',titleText));return s;}
function disclosure(key, label, expanded=false, cls='disclosure') {
  const panel=el('details',undefined,cls);
  panel.dataset.key=key;
  panel.open=expanded;
  panel.append(el('summary',label));
  return panel;
}
function operationCard(request) {
  const pending=request.status==='pending';
  const failed=['failed','interrupted'].includes(request.status) || ['failed','ambiguous'].includes(request.delivery);
  const delivered=request.notified && request.delivery==='sent';
  const card=disclosure('operation:'+request.id+':'+request.status+':'+request.notified+':'+request.delivery,'',pending || failed || !delivered,'operation-card');
  const title=el('span',undefined,'operation-title');
  title.append(el('strong',request.path.split('/').pop() || 'File operation'),el('small',request.operation==='delete'?'Delete file · L2':'Write file · L1'));
  card.firstChild.append(el('span',request.operation==='delete'?'−':'↳','file-icon'),title,status(pending?'awaiting_approval':request.status));
  const body=el('div',undefined,'operation-body');
  body.append(el('code',request.path),el('p','Requested by '+name(request.sender),'muted'));
  const notification=delivered?'Result delivered to Slack':pending?'Review the exact change before deciding.':request.delivery==='ambiguous'?'Slack delivery is unconfirmed.':request.delivery==='failed'?'Slack notification failed.':'Slack notification not yet confirmed.';
  body.append(el('p',notification,'operation-outcome'));
  if(request.error) body.append(el('p',request.error,'problem'));
  const actions=el('div',undefined,'operation-actions');
  actions.append(button(unlocked?(pending?'Review change':'Inspect change'):'Unlock to review',()=>unlocked?openReview(request.id):setView('settings'),pending?'primary':''));
  body.append(actions);
  card.append(body);
  return card;
}
function diffRows(diff) {
  const rows=[];
  let before, after;
  for(const line of diff.split('\n')) {
    const hunk=line.match(/^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/);
    if(hunk) {
      before=Number(hunk[1]);after=Number(hunk[2]);
      rows.push({kind:'hunk',before:'',after:'',text:line});
    } else if(before!==undefined && line) {
      const kind=line[0]==='+'?'add':line[0]==='-'?'remove':line[0]===' '?'context':'note';
      rows.push({kind,before:['remove','context'].includes(kind)?before++:'',after:['add','context'].includes(kind)?after++:'',text:line});
    }
  }
  return rows;
}
function fileDiff(proposal) {
  const rows=diffRows(proposal.diff);
  const panel=disclosure('diff:'+proposal.id,'',true,'file-diff');
  const counts=el('span',undefined,'diff-counts');
  counts.append(el('span','+'+rows.filter(row=>row.kind==='add').length,'added-count'),el('span','−'+rows.filter(row=>row.kind==='remove').length,'removed-count'));
  panel.firstChild.append(el('strong','Changes'),counts);
  if(!rows.length) panel.append(el('p','No line changes. Review the operation and full contents below.','diff-empty'));
  else {
    const code=el('div',undefined,'diff-code');
    code.tabIndex=0;
    code.setAttribute('aria-label','File changes; old and new line numbers');
    for(const row of rows) {
      const line=el('div',undefined,'diff-line '+row.kind);
      line.append(el('span',row.before,'line-number'),el('span',row.after,'line-number'),el('code',row.text));
      code.append(line);
    }
    panel.append(code);
  }
  return panel;
}
function taskNotes(task, notes) {
  const panel=section('Task & handoff');panel.classList.add('task-notes');
  const data=notes.data, facts=el('dl',undefined,'facts');
  const repo=(state.repositories||[]).find(repo=>repo.name===data.repo);
  for(const [label,value] of [['Repository',data.repo||'Not selected'],['Repository owner',repo?.owner||'—'],['Task owner',data.assignee?name(data.assignee):'Unassigned'],['Next step',data.next_step||'Not recorded'],['Blocked by',data.blocker||'None recorded'],['Continue when',data.unblock_when||'Not recorded'],['Turns without progress',notes.no_progress]]) facts.append(el('dt',label),el('dd',String(value)));
  panel.append(facts);
  const claims=data.claims||[];
  for(const claim of claims.filter(item=>item.state!=='superseded')) {
    const item=el('div',undefined,'claim');
    item.append(el('strong',claim.state==='disputed'?'Disputed · needs review':claim.basis==='owner_confirmed'?'Confirmed locally':'Reported · not independently verified'),el('p',text(claim.text)));
    if(claim.evidence) item.append(el('p',text(claim.evidence),'muted'));
    if(claim.source_ts) {
      const link=el('a','Source message ↗');
      link.href=`https://slack.com/archives/${encodeURIComponent(task.channel)}/p${claim.source_ts.replace('.','')}`;
      link.target='_blank';link.rel='noopener noreferrer';item.append(link);
    }
    panel.append(item);
  }
  if(unlocked && !['cleaned','archived'].includes(task.control_state)) {
    const edit=disclosure('task-edit:'+task.thread,'Correct task or conclusion');
    const form=el('form'), inputs={};
    for(const [field,label] of [['repo','Repository'],['assignee','Task owner'],['next_step','Next step'],['blocker','Blocker'],['unblock_when','Continue when']]) {
      const input=el(field==='repo'||field==='assignee'?'select':'input');input.name=field;
      if(field==='repo') {input.append(new Option('Not selected',''));for(const repo of state.repositories||[])input.append(new Option(repo.name,repo.name));}
      if(field==='assignee') {
        input.append(new Option('Unassigned',''));
        const ids=new Set([state.config.owner,data.assignee,...detail.events.map(e=>e.sender)].filter(Boolean));
        for(const id of ids)input.append(new Option(name(id),id));
      }
      input.value=data[field]||'';input.maxLength=1000;inputs[field]=input;
      const row=el('label',label);row.append(input);form.append(row);
    }
    const target=el('select');target.append(new Option('No conclusion correction',''));
    for(const claim of claims.filter(item=>item.state!=='superseded'))target.append(new Option(text(claim.text).slice(0,100),claim.id));
    const correction=el('textarea'), evidence=el('input');correction.maxLength=evidence.maxLength=1000;
    for(const [label,input] of [['Conclusion to correct',target],['Corrected conclusion',correction],['Evidence / version / source',evidence]]) {const row=el('label',label);row.append(input);form.append(row);}
    form.append(el('p','Saves local task context only. It does not grant access, approve an action, send a message, or resume the task.','muted'));
    const submit=el('button','Save correction','primary');submit.type='submit';form.append(submit);
    form.onsubmit=async event=>{
      event.preventDefault();submit.disabled=true;
      const changes=Object.fromEntries(Object.entries(inputs).filter(([field,input])=>input.value!==(data[field]||'')).map(([field,input])=>[field,input.value]));
      const body={channel:task.channel,thread:task.thread,revision:notes.revision,changes};
      if(target.value)body.correction={id:target.value,text:correction.value,evidence:evidence.value};
      try {await api('/api/task-notes',body);notify('Task context saved. Resume separately when ready.');if(selected===task)await loadDetail(task);}
      catch(error){notify(error.message,true);}finally{submit.disabled=false;}
    };
    edit.append(form);panel.append(edit);
  }
  if(notes.history?.length) {
    const history=disclosure('task-history:'+task.thread,'Correction history');
    for(const item of notes.history) history.append(el('p',`Revision ${item.revision} · ${item.actor==='agent'?'Agent report':name(item.actor)} · ${when(item.created)}`));
    panel.append(history);
  }
  return panel;
}

function renderDetail() {
  if (!selected || !detail) return;
  const expanded=new Map([...$('detail-body').querySelectorAll('details[data-key]')].map(panel=>[panel.dataset.key,panel.open]));
  const task=selected, content=[];
  const heading=el('h2',title(task));heading.title=title(task);
  const identity=el('div',undefined,'request-identity');
  identity.append(el('span',name(task.sender).slice(0,1).toUpperCase(),'avatar'),el('span',name(task.sender)+' · '+name(task.channel)),el('time',ago(task.updated)));
  content.push(identity,heading,status(task.status));
  const explanations={continued:'This thread reached its turn limit. A summary was posted to the channel as a new thread, where the discussion continues with the same session. Nothing needs you here; resume only to allow more replies in this original thread.',finished:'The agent judged the discussion finished, with every action item done or handed off, and posted a debrief to the channel. Further messages here are still answered.',paused:task.pause_reason+' Automatic replies are paused. Resume accepts future messages; it does not replay earlier ones.',closed:'Closed locally. Further messages in this thread will not trigger automatic replies.',archived:'Restore this request or preview clearing its stored contents.',awaiting_approval:'Review the file changes below, then approve or reject.',approved:'Your decision is saved. The operation is waiting to execute.',applying:'The approved operation is being executed.',rejected:'The file will not change. The listener will notify the original thread.',ambiguous:'Slack delivery is unconfirmed. Check the original channel before retrying.',interrupted:'Execution was interrupted. Inspect the file and notification records before deciding how to recover.',failed:'An operation or notification failed. See the results below.',pending:'The request is waiting to be assessed.',running:'The agent is processing this request.',waiting:'More information is needed. See the latest reply below.',blocked:'This task needs local attention. See the latest reply below.'};
  if(explanations[task.status] && task.status!=='awaiting_approval') {
    const next=el('div',undefined,'next-step '+bucket(task.status));
    next.append(el('strong',bucket(task.status)==='attention'?'Needs your attention':'Current status'),el('p',explanations[task.status]));
    content.push(next);
  }
  if (unlocked && task.control_state) {
    const controls=el('div',undefined,'request-controls');
    if(task.control_state==='paused' || task.control_state==='active' && task.status==='blocked') controls.append(button('Resume',()=>controlTask('resume',task),'primary'));
    if(['active','paused'].includes(task.control_state) && !detail.requests.some(r=>!r.notified || ['pending','approved','applying','rejected'].includes(r.status))) controls.append(button('Close request',()=>controlTask('close',task)));
    if(task.control_state==='closed' || task.control_state==='active' && ['complete','finished'].includes(task.status) || task.status==='continued') controls.append(button('Archive',()=>controlTask('archive',task)));
    if(task.control_state==='archived') controls.append(button('Restore',()=>controlTask('restore',task)),button('Preview cleanup',()=>previewCleanup(task),'danger'));
    if(controls.childElementCount) content.push(controls);
  } else if(!unlocked && ['paused','closed','archived'].includes(task.control_state)) content.push(button('Unlock local controls',()=>setView('settings')));
  if(detail.collaboration) content.push(taskNotes(task,detail.collaboration));
  if(detail.requests.length) {
    const operations=section('File changes');
    operations.append(...detail.requests.map(operationCard));
    content.push(operations);
  }
  if (task.next_step) {
    const reply=disclosure('reply:'+task.channel+':'+task.thread+':'+task.status,'Latest reply',bucket(task.status)!=='complete','reply-card');
    reply.append(el('pre',text(task.next_step)));
    content.push(reply);
  }
  const timeline=section('Activity');
  for (const event of [...detail.events].reverse()) {
    const failed=['failed','ambiguous','interrupted','blocked'].includes(event.state) || event.result_status==='blocked';
    const delivered=event.state==='sent' && event.sent_ts;
    const item=disclosure('event:'+event.id+':'+event.state+':'+event.result_status,'',failed || Boolean(event.result) && !delivered,'activity-entry'+(failed?' has-error':''));
    const description=el('span',undefined,'activity-description');
    description.append(el('strong',name(event.sender)),el('span',text(event.text).replace(/\s+/g,' ').slice(0,100),'activity-preview'));
    item.firstChild.append(el('span',failed?'!':'·','activity-dot'),description,el('time',ago(event.timestamp)));
    const body=el('div',undefined,'activity-body');
    body.append(el('pre',text(event.text)));
    if(event.decision==='silent') body.append(el('p','No reply sent · no new information to add.','muted'));
    if (event.result) {
      const result=disclosure('result:'+event.id,delivered?'Reply delivered':'Reply delivery unconfirmed',failed || !delivered,'result-card');
      result.append(el('pre',text(event.result)));
      body.append(result);
    }
    item.append(body);
    timeline.append(item);
  }
  if (detail.events.length<detail.total) timeline.append(button('Load earlier messages',async()=>{try{const older=await api(`/api/thread?channel=${encodeURIComponent(task.channel)}&thread=${encodeURIComponent(task.thread)}&offset=${detail.events.length}`);if(selected===task){detail.events.push(...older.events);renderDetail();}}catch(e){notify(e.message,true);}}));
  content.push(timeline);
  const metadata=disclosure('metadata:'+task.channel+':'+task.thread,'Request details');
  const facts=el('dl',undefined,'facts');
  for (const [k,v] of [['Requested by',name(task.sender)],['Source',name(task.channel)],['Updated',when(task.updated)],['Project',repositoryLabel(task)]]) facts.append(el('dt',k),el('dd',v));
  metadata.append(facts);
  const link=el('a','Open Slack channel ↗');link.href=`https://slack.com/app_redirect?team=${encodeURIComponent(state.config.workspace)}&channel=${encodeURIComponent(task.channel)}`;link.target='_blank';link.rel='noopener noreferrer';metadata.append(link);
  if (unlocked && state.projects.length) {
    const binding=section('Project label'); const select=el('select');select.setAttribute('aria-label','Task project');select.append(new Option('No repository linked',''));
    for (const p of state.projects) select.append(new Option(p.name,p.id));select.value=task.project_id||'';
    const line=el('div',undefined,'inline');line.append(select,button('Save',async()=>{try{await api('/api/task-project',{channel:task.channel,thread:task.thread,project_id:select.value});task.project_id=select.value;notify('Project label saved. File permissions have not changed.');renderDetail();refresh();}catch(e){notify(e.message,true);}}));
    binding.append(line,el('p','Organizes this task only. It does not grant access or change a proposed operation.','muted'));metadata.append(binding);
  }
  content.push(metadata);
  const technical=disclosure('technical:'+task.channel+':'+task.thread,'Technical details',false,'technical');
  technical.append(el('pre',JSON.stringify({task_id:task.task_id,channel:task.channel,thread:task.thread,sender:task.sender,delivery:task.delivery},null,2)));
  content.push(technical);
  $('detail-body').replaceChildren(...content);
  for(const panel of $('detail-body').querySelectorAll('details[data-key]')) {
    if(expanded.has(panel.dataset.key)) panel.open=expanded.get(panel.dataset.key);
  }
}
async function openReview(id) {
  const task=selected;
  try {
    const proposal=await api('/api/requests/'+encodeURIComponent(id));
    if(selected!==task || !unlocked) return;
    review=proposal;
    const deleting=review.operation==='delete';
    const body=$('review-body');
    const heading=el('div',undefined,'review-file');
    heading.append(el('span',deleting?'−':'↳','file-icon'),el('h3',review.path.split('/').pop()));
    const scope=el('div',undefined,'review-scope');
    scope.append(el('span',deleting?'L2 · Delete file':'L1 · Write file','scope-badge'),el('span','One-time approval','scope-badge'));
    body.replaceChildren(heading,el('code',review.path,'review-path'),scope,el('p',`Requested by ${name(review.sender)} · ${name(review.channel)}`,'muted'));
    if(review.status!=='pending') body.append(status(review.status));
    if (review.problem) body.append(el('p',review.problem,'problem'));
    body.append(fileDiff(review));
    for (const [label,value] of [['Full contents before',review.before],['Full contents after',review.after]]) {
      const panel=disclosure('full:'+label,label);
      panel.append(el('pre',value,'full-content'));
      body.append(panel);
    }
    body.append(el('p','Approval applies to this exact change only. It does not create an automatic write grant.','review-note'));
    body.scrollTop=0;
    $('review-error').hidden=true;
    $('approve-request').textContent=deleting?'Approve deletion once':'Approve once';
    $('approve-request').className=deleting?'danger':'primary';
    $('approve-request').disabled=review.status!=='pending'||!!review.problem;
    $('reject-request').disabled=review.status!=='pending';
    $('review-dialog').showModal();
  } catch(e) {notify(e.message,true);}
}
async function submitDecision(decision) {
  if (!review || reviewBusy) return;
  reviewBusy=true;$('approve-request').disabled=true;$('reject-request').disabled=true;
  try {
    await api('/api/requests/'+encodeURIComponent(review.id)+'/decision',{decision,revision:review.revision});
    $('review-dialog').close();
    notify(decision==='approved'?'Approval saved and queued. File results and Slack delivery will update separately.':'Request rejected. The file will not change; the listener will notify the original thread.');
    detail=null;await refresh();
  } catch(e) { $('review-error').textContent=e.message;$('review-error').hidden=false; }
  finally {reviewBusy=false;}
}
async function controlTask(action,task,cleanupRevision) {
  try {
    await api('/api/thread-action',{channel:task.channel,thread:task.thread,action,revision:task.control_revision,cleanup_revision:cleanupRevision});
    selected=null;detail=null;$('detail').hidden=true;
    notify({resume:'Resumed for future messages. Earlier messages will not be replayed.',close:'Request closed locally. Automatic replies are stopped.',archive:'Request archived. Restore it from Requests → Archived.',restore:'Request restored.',clean:'Local request contents cleared. Project files and Slack messages are unchanged.'}[action]);
    await refresh();return true;
  } catch(e) {notify(e.message,true);return false;}
}
async function previewCleanup(task) {
  try {
    const result=await api(`/api/cleanup-preview?channel=${encodeURIComponent(task.channel)}&thread=${encodeURIComponent(task.thread)}`);
    cleanupReview={task,...result};
    $('cleanup-preview').replaceChildren(el('h3',title(task)),el('p',`${result.messages} stored messages · ${result.file_requests} file proposals`),el('p',result.description));
    $('cleanup-confirm').checked=false;$('cleanup-submit').disabled=true;$('cleanup-error').hidden=true;
    $('cleanup-dialog').showModal();
  } catch(e) {notify(e.message,true);}
}
function renderProjects() {
  const nodes=[];
  $('project-lock').hidden=unlocked;
  renderEditors();
  for (const [mode,roots] of [[state.config.file_access?'Managed files · grants or individual approval':'Native tools · not controlled by these approvals',state.config.write_roots],['Read-only directory',state.config.read_roots]]) for (const root of roots) {
    const p=state.projects.find(p=>p.root===root), box=el('section',undefined,'project');
    const head=el('div',undefined,'project-heading');head.append(el('h2',p?p.name:root.split('/').pop()),el('span',mode,'muted'));
    box.append(head,el('code',root),el('p',p?'Repository label set by the local owner.':'No repository linked. Mentioning a project in Slack does not bind or authorize it.'));
    if (p) {const a=el('a',p.repository+' ↗');a.href=p.repository;a.target='_blank';a.rel='noopener noreferrer';box.append(a);}
    const form=el('form',undefined,'project-form');
    const label=el('label','GitHub repository URL');
    const input=el('input');input.type='url';input.required=true;input.disabled=!unlocked;input.placeholder='https://github.com/owner/repository';input.value=p?.repository||'';
    label.append(input);
    const save=el('button','Save repository label','primary');save.type='submit';save.disabled=!unlocked;
    form.append(label,el('p','This identifies the repository in the monitor. It does not clone the repository or grant file access.','muted'),save);
    form.onsubmit=async e=>{e.preventDefault();save.disabled=true;try{await api('/api/projects',{root,repository:input.value.trim().replace(/\/$/,'')});notify('Repository label saved. Directory permissions have not changed.');filledRevision=null;refresh();}catch(error){notify(error.message,true);}finally{save.disabled=!unlocked;}};
    box.append(form);
    nodes.push(box);
  }
  $('project-list').replaceChildren(...nodes);
  $('grant-list').replaceChildren(...(state.grants.length?state.grants.map(g=>{const row=el('div',undefined,'request');row.append(el('strong',name(g.sender)),el('code',g.path),el('p',name(g.channel)+' · '+(g.expires?'Expires '+when(g.expires):'Until revoked locally')));const revoke=button('Revoke grant',()=>reviewChange('Revoke write grant',[['Requester',name(g.sender)],['Path',g.path]],'/api/grants',{action:'revoke',id:g.id}));revoke.disabled=!unlocked;row.append(revoke);return row;}):[el('p',state.config.file_access?'No standing write grants. Changes without a grant require individual approval.':'Managed file access is disabled. The agent uses native tools; this approval flow does not control their write permissions.','description')]));
}
function renderSettings() {
  renderEditors();
  $('access-description').textContent=unlocked?'Local controls unlocked. You can save settings and manage grants; approving a single request does not create a standing grant.':'Read-only mode. File contents and decisions require your local access key.';
  $('unlock-form').hidden=unlocked || !state.approvals_enabled;$('lock').hidden=!unlocked;
  $('unlock-help').textContent=state.approvals_enabled?'The key is generated when the monitor starts and rotates on restart. It is never sent to Slack.':'This monitor started read-only. Start it with --allow-approvals to enable decisions; the Slack listener does not need to restart.';
  $('stop-monitor').disabled=!unlocked;
  $('service-info').replaceChildren(status(state.health.status),el('p',`Last channel message: ${when(state.last_message)}.`));
  const domains=state.config.allowed_domains||[];
  const network=domains.length?(domains.includes('*')?'Every host (full internet access for task commands)':'Only '+domains.join(', ')):'Disabled for task commands';
  const continuity=state.config.resume_sessions?`On · a thread's session is resumed for ${Math.round((state.config.session_timeout||0)/86400)} days of inactivity`:'Off · every reply starts a new session';
  const facts=el('dl',undefined,'config-facts');
  for(const [term,detail] of [['Network access',network],['Session continuity',continuity],['At the turn limit','A stop notice is posted in the thread and a summary starts a new thread in the channel.'],['When a discussion is finished','A debrief is posted to the channel.']]) facts.append(el('dt',term),el('dd',detail));
  $('agent-config').replaceChildren(el('h3','Fixed in config.toml'),el('p','These settings change only by editing the file and restarting the listener.','muted'),facts);
  $('diagnostics').textContent=JSON.stringify({owner:state.config.owner,workspace:state.config.workspace,channels:state.config.channels,backend:state.config.backend,allowed_domains:domains,resume_sessions:state.config.resume_sessions,session_timeout:state.config.session_timeout,listener:state.health},null,2);
}
function renderEditors() {
  const enabled=unlocked && state.settings_enabled;
  for (const form of ['agent-form','directory-form']) for(const input of $(form).elements) input.disabled=!enabled || (form==='directory-form'&&!state.config.file_access);
  for(const input of $('grant-form').elements) input.disabled=!unlocked || !state.config.file_access;
  for(const b of document.querySelectorAll('.reload-settings')) b.disabled=!enabled;
  $('settings-status').textContent=!state.settings_enabled?'Configuration editing is not enabled for this monitor.':!unlocked?'Unlock local controls above to edit these fields.':state.health.status!=='connected'?'Saved · The listener is not connected. Start or reconnect it to apply settings.':!state.settings_applied?'Saved · Waiting for the listener to apply these settings between requests.':editor && editor.revision!==state.settings_revision?'Settings changed elsewhere. Reload saved values before editing.':'Saved settings are active in the listener.';
  const values=editor?.values || {model:state.config.model,reasoning_effort:state.config.reasoning_effort,max_wait_replies:state.config.max_wait_replies,max_turns:state.config.max_turns,workspace:state.config.write_roots[0],additional_workspaces:state.config.write_roots.slice(1),read_only_workspaces:state.config.read_roots};
  const revision=(editor?.revision || 'locked')+view;
  if(filledRevision!==revision){
    $('directory-main').value=values.workspace;
    $('directory-write').value=values.additional_workspaces.join('\n');
    $('directory-read').value=values.read_only_workspaces.join('\n');
    $('setting-model').value=values.model||'';$('setting-effort').value=values.reasoning_effort||'';
    $('setting-waits').value=values.max_wait_replies;$('setting-turns').value=values.max_turns;
    filledRevision=revision;
  }
  if(!document.activeElement?.closest('#grant-form')){
    const selectedPerson=$('grant-person').value;
    const people=Object.keys(state.names).filter(id=>/^[UW]/.test(id));
    $('grant-person').replaceChildren(Object.assign(el('option','Choose a requester'),{value:''}),...people.map(id=>{const o=el('option',name(id));o.value=id;return o;}),Object.assign(el('option','Another Slack member…'),{value:'other'}));
    if(people.includes(selectedPerson)||selectedPerson==='other')$('grant-person').value=selectedPerson;
    $('grant-member-label').hidden=$('grant-person').value!=='other';
    const channel=$('grant-channel').value;
    $('grant-channel').replaceChildren(...state.config.channels.map(id=>{const o=el('option',name(id));o.value=id;return o;}));
    if(state.config.channels.includes(channel))$('grant-channel').value=channel;
    $('grant-roots').replaceChildren(...state.config.write_roots.map(root=>Object.assign(el('option'),{value:root})));
  }
}
function reviewChange(heading,rows,path,body) {
  settingsAction={path,body};$('settings-review-title').textContent=heading;
  const facts=el('dl',undefined,'facts');for(const [label,value] of rows)facts.append(el('dt',label),el('dd',String(value)));
  $('settings-review-body').replaceChildren(facts);$('settings-review-error').hidden=true;
  $('settings-review-save').disabled=false;$('settings-dialog').showModal();
}
function reviewSettings(changes,heading) {
  if(!editor)return;
  const fieldNames={workspace:'Working folder',additional_workspaces:'Managed write folders',read_only_workspaces:'Read-only folders',model:'Model',reasoning_effort:'Reasoning',max_wait_replies:'Waiting replies',max_turns:'Max turns'};
  const changed=Object.fromEntries(Object.entries(changes).filter(([key,value])=>JSON.stringify(value)!==JSON.stringify(editor.values[key])));
  if(!Object.keys(changed).length){notify('No changes to save.');return;}
  const display=value=>value===null?'Backend default':Array.isArray(value)?value.join('\n')||'None':String(value);
  reviewChange(heading,Object.entries(changed).map(([key,value])=>[fieldNames[key],display(editor.values[key])+' → '+display(value)]),'/api/settings',{revision:editor.revision,changes:changed});
}
async function loadHistory(append=false) {
  const request=++historyRequest, requestedView=activityView, requestedOffset=historyOffset;
  const result=await api('/api/activity?view='+requestedView+'&offset='+requestedOffset);
  if (request!==historyRequest || view!=='history' || activityView!==requestedView || historyOffset!==requestedOffset) return;
  for(const view of ['current','archived']){const b=$('activity-'+view);b.textContent=(view==='current'?'Current':'Archived')+' '+result.counts[view];b.classList.toggle('selected',activityView===view);b.setAttribute('aria-pressed',String(activityView===view));}
  $('activity-archive').disabled=!unlocked;$('activity-before').disabled=!unlocked;$('activity-restore').disabled=!unlocked || !result.counts.archived;
  $('activity-access').textContent=unlocked?'Archiving keeps entries in this database. It does not delete history or reduce disk usage.':'Unlock local controls in Settings to move or restore activity.';
  historyItems=append?[...historyItems,...result.items]:result.items;
  $('history-list').replaceChildren(...historyItems.map(item=>{const row=el('article',undefined,'history-row');row.append(el('small',`${when(item.timestamp)} · ${item.kind==='decision'?'Local approvals':item.kind==='control'?'Local request control':'Channel message'} · ${name(item.sender)}`),el('p',text(item.text)),status(item.status));return row;}));
  if (!historyItems.length) $('history-list').append(el('p',activityView==='archived'?'No archived activity.':'No activity in the current view. Older entries may be in Archived.','muted'));
  $('history-more').disabled=historyItems.length>=result.total;
}
async function refresh() {
  if (stopped) return;
  const request=++refreshRequest;
  if (busy) { queuedRefresh=true; return; }
  busy=true;
  try {
    const next=await api('/api/state');
    if (request!==refreshRequest) return;
    const namesChanged=JSON.stringify(state?.names)!==JSON.stringify(next.names);
    state=next;
    renderShell();
    if(unlocked && state.settings_enabled && ['projects','settings'].includes(view) && (!editor || editorView!==view)){
      const settings=await api('/api/settings');
      if (request!==refreshRequest) return;
      editor=settings;
      editorView=view;
    }
    if (view==='overview') {
      const tasks=await api('/api/tasks?limit=5');
      if (request!==refreshRequest) return;
      page=tasks;
      renderOverview();
    } else if (['inbox','tasks'].includes(view)) {
      const tasks=await api(`/api/tasks?view=${filter}&offset=${offset}&q=${encodeURIComponent($('search').value)}&sender=${encodeURIComponent(senderFilter)}`);
      if (request!==refreshRequest) return;
      page=tasks;
      renderTasks();
      if (selected) {
        const task=selected;
        let latest=page.items.find(t=>t.channel===task.channel && t.thread===task.thread);
        if (!latest) latest=(await api(`/api/tasks?channel=${encodeURIComponent(task.channel)}&thread=${encodeURIComponent(task.thread)}`)).items[0];
        if (request!==refreshRequest || selected!==task) return;
        const changed=latest && JSON.stringify(latest)!==JSON.stringify(task);
        if (!document.activeElement?.closest('.task-notes form')) {
          if (changed) selected=latest;
          if (!detail || changed) await loadDetail(selected);
          else if (namesChanged) renderDetail();
        }
      }
    } else if (view==='projects' && (!filledRevision || !document.activeElement?.closest('#projects-view form'))) renderProjects();
    else if (view==='settings') renderSettings();
    else if (view==='history' && historyOffset===0) await loadHistory();
    $('error').hidden=true;
  } catch(e) {
    if (request===refreshRequest) {
      notify(e.message+' Displayed information may be stale.',true);
      $('refresh-state').textContent='Updates disconnected';
    }
  }
  finally {busy=false;if(queuedRefresh){queuedRefresh=false;setTimeout(refresh,0);}else schedule();}
}
function schedule() {clearTimeout(refreshTimer);if(!stopped && !document.hidden && $('auto-refresh').checked) refreshTimer=setTimeout(refresh,5000);}
async function unlock() {await api('/api/access');unlocked=true;sessionStorage.setItem('fridica-key',key);notify('Local controls unlocked.');await refresh();}
for (const n of document.querySelectorAll('[data-view]')) n.onclick=()=>setView(n.dataset.view);
for(const mode of ['list','cards']) $(mode==='list'?'list-layout':'card-layout').onclick=()=>{layout=mode;sessionStorage.setItem('fridica-layout',mode);renderLayout();};
$('project-unlock').onclick=()=>{setView('settings');$('access-key').focus();};
$('connection').onclick=()=>setView('settings');$('refresh').onclick=()=>refresh();
$('previous').onclick=()=>{offset=Math.max(0,offset-50);refresh();};$('next').onclick=()=>{offset+=50;refresh();};
function selectSender(id) {senderFilter=id;offset=0;selected=null;detail=null;$('detail').hidden=true;refresh();}
$('sender-filter').onchange=()=>selectSender($('sender-filter').value);
$('clear-sender').onclick=()=>selectSender('');
$('search').oninput=()=>{clearTimeout(searchTimer);searchTimer=setTimeout(()=>{offset=0;refresh();},250);};
$('close-detail').onclick=()=>{selected=null;detail=null;$('detail').hidden=true;renderTasks();};
$('auto-refresh').onchange=schedule;
document.addEventListener('visibilitychange',()=>{if(!document.hidden && $('auto-refresh').checked) refresh();else clearTimeout(refreshTimer);});
$('approve-request').onclick=()=>submitDecision('approved');$('reject-request').onclick=()=>submitDecision('rejected');
$('unlock-form').onsubmit=async e=>{e.preventDefault();key=$('access-key').value.trim();$('access-key').value='';try{await unlock();}catch(error){key='';unlocked=false;sessionStorage.removeItem('fridica-key');notify(error.message,true);}};
$('lock').onclick=()=>{key='';unlocked=false;sessionStorage.removeItem('fridica-key');review=null;editor=null;filledRevision=null;$('settings-dialog').close();$('review-dialog').close();$('cleanup-dialog').close();cleanupReview=null;renderSettings();renderShell();};
$('history-more').onclick=async()=>{historyOffset=historyItems.length;try{await loadHistory(true);}catch(e){notify(e.message,true);}};
$('cleanup-cancel').onclick=()=>$('cleanup-dialog').close();
$('cleanup-confirm').onchange=()=>$('cleanup-submit').disabled=!$('cleanup-confirm').checked;
$('cleanup-submit').onclick=async()=>{if(!cleanupReview || !$('cleanup-confirm').checked)return;$('cleanup-submit').disabled=true;const done=await controlTask('clean',cleanupReview.task,cleanupReview.revision);if(done){$('cleanup-dialog').close();cleanupReview=null;}else{$('cleanup-error').hidden=false;$('cleanup-error').textContent='The request changed or cleanup failed. Close this dialog and preview again.';}};
$('stop-monitor').onclick=async()=>{try{await api('/api/stop',{});stopped=true;clearTimeout(refreshTimer);notify('Monitor stopped. Slack continues listening and pending requests remain saved.');$('stop-monitor').disabled=true;$('refresh-state').textContent='Monitor stopped';}catch(e){notify(e.message,true);}};
(async()=>{if(key){try{await api('/api/access');unlocked=true;}catch{key='';sessionStorage.removeItem('fridica-key');}}await refresh();})();

for(const b of document.querySelectorAll('.reload-settings'))b.onclick=()=>{editor=null;filledRevision=null;refresh();};
$('directory-form').onsubmit=e=>{e.preventDefault();const lines=id=>$(id).value.split('\n').map(s=>s.trim()).filter(Boolean);reviewSettings({workspace:$('directory-main').value.trim(),additional_workspaces:lines('directory-write'),read_only_workspaces:lines('directory-read')},'Review directory access');};
$('agent-form').onsubmit=e=>{e.preventDefault();reviewSettings({model:$('setting-model').value.trim()||null,reasoning_effort:$('setting-effort').value||null,max_wait_replies:Number($('setting-waits').value),max_turns:Number($('setting-turns').value)},'Review model & loop settings');};
$('grant-person').onchange=()=>{$('grant-member-label').hidden=$('grant-person').value!=='other';};
$('grant-form').onsubmit=e=>{e.preventDefault();const sender=$('grant-person').value==='other'?$('grant-member').value.trim():$('grant-person').value;const ttl=$('grant-ttl').value;const body={action:'grant',sender,channel:$('grant-channel').value,path:$('grant-path').value.trim(),ttl:ttl?Number(ttl):null};reviewChange('Allow automatic text-file writes',[['Requester',name(sender)==='Unknown member'?sender:name(sender)],['Channel',name(body.channel)],['Path',body.path],['Duration',ttl?$('grant-ttl').selectedOptions[0].textContent:'Until revoked'],['Scope','Text-file writes only. Deletes need individual approval. Git commands and external actions are not supported here.']],'/api/grants',body);};
$('settings-review-cancel').onclick=()=>$('settings-dialog').close();
$('settings-review-save').onclick=async()=>{if(settingsSaving||!settingsAction)return;settingsSaving=true;$('settings-review-save').disabled=true;try{await api(settingsAction.path,settingsAction.body);$('settings-dialog').close();editor=null;filledRevision=null;notify(settingsAction.path==='/api/settings'?'Settings saved. The listener applies them between requests.':settingsAction.path==='/api/activity/archive'?'Activity '+(settingsAction.body.action==='archive'?'archived.':'restored.'):'Grant updated.');if(settingsAction.path==='/api/activity/archive'){historyOffset=0;historyItems=[];}await refresh();}catch(e){$('settings-review-error').textContent=e.message;$('settings-review-error').hidden=false;}finally{settingsSaving=false;$('settings-review-save').disabled=false;}};

for(const view of ['current','archived'])$('activity-'+view).onclick=()=>{activityView=view;historyOffset=0;historyItems=[];refresh();};
const activityDate=new Date();activityDate.setDate(activityDate.getDate()-30);
const dateField=d=>[d.getFullYear(),String(d.getMonth()+1).padStart(2,'0'),String(d.getDate()).padStart(2,'0')].join('-');
$('activity-before').value=dateField(activityDate);$('activity-before').max=dateField(new Date());
$('activity-archive-form').onsubmit=async e=>{e.preventDefault();const before=new Date($('activity-before').value+'T00:00:00').getTime()/1000;try{const preview=await api('/api/activity?before='+before);if(!preview.movable){notify('No current activity falls before this date.');return;}reviewChange('Archive older activity',[['Before',when(before)],['Entries to move',preview.movable],['Result','Move to Archived. Entries remain accessible and can be restored.']],'/api/activity/archive',{action:'archive',before});}catch(e){notify(e.message,true);}};
$('activity-restore').onclick=()=>reviewChange('Restore archived activity',[['Result','Return all archived entries in the configured channels to Current.']],'/api/activity/archive',{action:'restore'});
