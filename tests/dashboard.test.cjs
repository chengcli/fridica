const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const {test} = require('node:test');

const source = fs.readFileSync(path.join(__dirname, '../src/fridica/dashboard.js'), 'utf8');

function dashboard() {
  const nodes = new Map();
  const context = vm.createContext({
    URLSearchParams, AbortSignal, console,
    location: {hash: ''},
    sessionStorage: {getItem: () => null},
    document: {
      getElementById(id) {
        if (!nodes.has(id)) nodes.set(id, {value: '', replaceChildren() {}, setAttribute() {}, classList: {toggle() {}}});
        return nodes.get(id);
      },
      querySelectorAll: () => [], addEventListener() {}, createElement: () => ({dataset: {}, children: [], append(...nodes) {this.children.push(...nodes);}, get firstChild() {return this.children[0];}}),
    },
    fetch: () => new Promise(() => {}),
    setTimeout() {}, clearTimeout() {},
  });
  vm.runInContext(source, context);
  vm.runInContext(`
    busy=false;
    renderShell=renderTasks=schedule=()=>{};
    renderDetail=()=>{globalThis.shown={thread:selected.thread,conversation:detail.thread};};
    view='tasks';
    selected={channel:'CROOM',thread:'A'};
    api=async path=>{
      if(path==='/api/state')return {};
      if(path.startsWith('/api/tasks?'))return {items:[selected]};
      if(path.includes('thread=A'))return new Promise(resolve=>{globalThis.finishA=resolve;});
      if(path.includes('thread=B'))return {thread:'B'};
      throw Error(path);
    };
  `, context);
  return context;
}

for (const action of ['switch', 'close']) test(`late refresh cannot overwrite a ${action === 'switch' ? 'different' : 'closed'} request`, async () => {
  const context = dashboard();
  const pending = vm.runInContext('refresh()', context);
  for (let i=0; i<10 && !context.finishA; i++) await Promise.resolve();
  assert.ok(context.finishA, 'refresh requested thread A');
  if (action === 'switch') await vm.runInContext("openTask({channel:'CROOM',thread:'B'})", context);
  else vm.runInContext('selected=null;detail=null', context);
  context.finishA({thread: 'A'});
  await pending;
  if (action === 'switch') assert.equal(context.shown.conversation, 'B');
  else assert.equal(vm.runInContext('detail', context), null);
});

test('changing the request filter discards the previous list response', async () => {
  const context = dashboard();
  vm.runInContext(`
    selected=null;
    renderTasks=()=>{globalThis.shownRows=page.items;};
    api=async path=>path==='/api/state'?{}:new Promise(resolve=>{globalThis.finishList=resolve;});
  `, context);
  const pending = vm.runInContext('refresh()', context);
  for (let i=0; i<10 && !context.finishList; i++) await Promise.resolve();
  assert.ok(context.finishList);
  vm.runInContext("filter='waiting';refresh()", context);
  context.finishList({items: [{thread: 'old-filter'}]});
  await pending;
  assert.equal(context.shownRows, undefined);
});

test('changing Activity view discards a pending older page', async () => {
  const context = dashboard();
  vm.runInContext(`
    view='history';historyOffset=50;historyItems=[{text:'current'}];
    api=()=>new Promise(resolve=>{globalThis.finishHistory=resolve;});
  `, context);
  const pending = vm.runInContext('loadHistory(true)', context);
  vm.runInContext("activityView='archived';historyOffset=0;historyItems=[]", context);
  context.finishHistory({items: [{text: 'old-current'}], counts: {current: 51, archived: 0}, total: 51});
  await pending;
  assert.equal(vm.runInContext('historyItems.length', context), 0);
});

test('diff rows count changes and preserve hunk line numbers', () => {
  const context = dashboard();
  const rows = vm.runInContext("diffRows('--- Before\\n+++ After\\n@@ -8,2 +8,3 @@\\n same\\n-old\\n+++literal\\n+new\\n\\\\ No newline at end of file\\n')", context);
  assert.equal(rows.filter(row=>row.kind==='add').length, 2);
  assert.equal(rows.filter(row=>row.kind==='remove').length, 1);
  const added = rows.filter(row=>row.kind==='add');
  assert.equal(added[0].text, '+++literal');
  assert.equal(added[0].after, 9);
  assert.equal(added[1].after, 10);
  assert.equal(rows.find(row=>row.kind==='remove').before, 9);
});

test('an approval preview cannot open after switching requests', async () => {
  const context=dashboard();
  vm.runInContext('unlocked=true;api=()=>new Promise(resolve=>{globalThis.finishReview=resolve;})',context);
  const pending=vm.runInContext("openReview('write')",context);
  vm.runInContext("selected={channel:'CROOM',thread:'B'}",context);
  context.finishReview({id:'write',path:'/project/README.md'});
  await pending;
  assert.equal(vm.runInContext('review',context),undefined);
});


test('file cards stay expanded until Slack delivery is confirmed', () => {
  const context=dashboard();
  vm.runInContext("state={names:{UALICE:'Alice'},config:{}}",context);
  const card=delivery=>vm.runInContext(`operationCard({id:'write',status:'complete',notified:1,delivery:'${delivery}',operation:'write',path:'/project/README.md',sender:'UALICE'})`,context);
  for(const delivery of ['ready','sending','failed','ambiguous']) assert.equal(card(delivery).open,true,delivery);
  assert.equal(card('sent').open,false);
  assert.notEqual(card('ready').dataset.key,card('failed').dataset.key);
});

test('mention labels render and leading mentions do not leave partial names in titles', () => {
  const context=dashboard();
  vm.runInContext("state={names:{UOWNER:'Demo Owner',UXI:'Xi Zhang'},config:{owner:'UOWNER'}}",context);
  assert.equal(vm.runInContext("text('Ask <@UXI|Old label> and <@UNEW|New Member>')",context),'Ask @Xi Zhang and @New Member');
  assert.equal(vm.runInContext("title({title:'<@UOWNER> <@UXI> — Review the changes'})",context),'Review the changes');
});

test('resolved names update an open detail without reloading its conversation', async () => {
  const context=dashboard();
  vm.runInContext(`
    state={names:{}}; detail={thread:'A'};
    api=async path=>path==='/api/state'?{names:{UXI:'Xi Zhang'}}:{items:[selected]};
    renderDetail=()=>{globalThis.shownName=name('UXI');};
  `,context);
  await vm.runInContext('refresh()',context);
  assert.equal(context.shownName,'Xi Zhang');
});
