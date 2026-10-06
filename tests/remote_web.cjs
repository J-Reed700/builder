// Behavioral failure test for the browser adapter's uncertain POST recovery.
// No browser dependencies: the minimal DOM stands in for the visible composer.
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { webcrypto } = require('node:crypto');
const vm = require('node:vm');
const source = readFileSync(require('node:path').join(__dirname, '../remote/web/app.js'), 'utf8');

async function uncertainReply(kind) {
  const elements = new Map();
  const element = () => ({ value: '', textContent: '', hidden: false, disabled: false, checked: false, append() {}, replaceChildren() {}, setAttribute() {} });
  const get = id => { if (!elements.has(id)) elements.set(id, element()); return elements.get(id); };
  let posts = 0, acceptedId;
  const context = vm.createContext({
    crypto: webcrypto, AbortSignal, setInterval() {},
    document: { getElementById: get, createElement: element, createDocumentFragment: element, querySelectorAll: () => [], addEventListener() {} },
    async fetch(path, options) {
      if (options.method === 'POST') {
        posts++;
        acceptedId = JSON.parse(options.body).request_id;
        if (kind === 'lost-acknowledgement') throw new Error('Connection lost after acceptance');
        return { ok: false, status: 409, json: async () => ({ error: 'Worker failed before the prompt was committed' }) };
      }
      const payload = path.startsWith('/api/sessions?') ? { sessions: [], next_offset: null } : {
        states: [{ session: 'host-created-chat', run_id: acceptedId, phase: kind === 'lost-acknowledgement' ? 'complete' : 'failed', notices: [] }],
        workspace: '/fixture', approval_mode: 'ask',
      };
      return { ok: true, json: async () => payload };
    },
  });
  vm.runInContext(source, context);
  get('prompt').value = 'Keep this instruction until its saved history is inspected';
  get('profile').value = 'local';
  get('chat-filter').value = 'false';
  await vm.runInContext("token = 'fixture'; run({action: 'message', prompt: $('prompt').value})", context);
  assert.equal(posts, 1, 'Uncertain requests must never replay automatically');
  assert.equal(get('prompt').value, 'Keep this instruction until its saved history is inspected');
  assert.equal(vm.runInContext("drafts.get('new')", context), get('prompt').value);
  assert.equal(vm.runInContext('selected', context), null, 'A failed start must not select a potentially empty session and hide the draft');
  assert.match(get('error').textContent, /Check this chat before sending again/);
}
async function queuesMessageDuringCompaction() {
  const elements = new Map();
  const element = () => ({ value: '', textContent: '', hidden: false, disabled: false, checked: false, append() {}, replaceChildren() {}, setAttribute() {} });
  const get = id => { if (!elements.has(id)) elements.set(id, element()); return elements.get(id); };
  let posts = 0;
  const context = vm.createContext({
    crypto: webcrypto, AbortSignal, setInterval() {},
    document: { getElementById: get, createElement: element, createDocumentFragment: element, addEventListener() {}, querySelector: () => ({ scrollHeight: 0, scrollTop: 0, clientHeight: 0 }), scrollingElement: { scrollHeight: 0, scrollTop: 0, clientHeight: 0 }, querySelectorAll: () => [] },
    async fetch(path, options) {
      if (options.method === 'POST') { posts++; return { ok: true, status: 202, json: async () => ({ session: 'chat' }) }; }
      return { ok: true, json: async () => ({ states: [], sessions: [], next_offset: null, workspace: '/fixture', approval_mode: 'ask' }) };
    },
  });
  vm.runInContext(source, context);
  get('prompt').value = 'send this after compaction';
  get('profile').value = 'local';
  vm.runInContext("selected = 'chat'; states = [{session: 'chat', phase: 'running', operation: 'compact', compacting: true}]; info = {archived: false};", context);
  await vm.runInContext("run({action: 'message', prompt: $('prompt').value})", context);
  assert.equal(posts, 0, 'a compacting chat must not reject or send the queued message');
  assert.equal(vm.runInContext("queued.get('chat')", context), 'send this after compaction');
  vm.runInContext("states = [{session: 'chat', phase: 'complete', operation: 'compact', compacting: false}]; openSession = async () => {}; sessions = async () => {}; refresh = async () => {}; maybeSendQueued()", context);
  await new Promise(resolve => setTimeout(resolve, 0));
  assert.equal(posts, 1, 'the queued message must send after compaction completes');
  assert.equal(vm.runInContext("queued.has('chat')", context), false);
}
(async () => {
  await uncertainReply('lost-acknowledgement');
  await uncertainReply('failed-before-commit');
  await queuesMessageDuringCompaction();
  console.log('Browser recovery passed: lost acknowledgements, rejected starts, and compaction queues preserve message safety.');
})().catch(error => { console.error(error); process.exitCode = 1; });
