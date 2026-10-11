const { boot, response, deferred, chat } = require('./harness.cjs');
let app;
afterEach(() => app?.cleanup());
async function ready(options) { app = await boot(options); await app.login(); return app; }
function accept() { app.override(c => c.path === '/api/run', () => response({ session: 'one' }, 202)); }
async function phase(phase, extra = {}) {
  app.state.states = [{ session: 'one', run_id: 'run-one', phase, notices: [], ...extra }];
  await app.poll();
}

test('a message carries a fresh UUID, folder, profile and explicit permission mode', async () => {
  await ready(); accept();
  await app.change('profile', 'other'); await app.change('approval-mode', 'read-only');
  await app.type('prompt', '  preserve whitespace and 🦀\nsecond line  ');
  await app.submit('composer');
  expect(app.posts()).toHaveLength(1);
  expect(app.posts()[0].body).toEqual({
    request_id: expect.stringMatching(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/),
    session: null, profile: 'other', workspace: '', approval: 'read-only',
    operation: { action: 'message', prompt: '  preserve whitespace and 🦀\nsecond line  ' },
  });
  expect(app.get('prompt').value).toBe('');
  expect(app.get('title').textContent).toBe('First chat');
  await app.type('prompt', 'next'); await app.submit('composer');
  expect(app.posts()[1].body.session).toBe('one');
  expect(app.posts()[1].body.profile).toBeNull();
  expect(app.posts()[1].body.workspace).toBeNull();
  expect(app.posts()[1].body.request_id).not.toBe(app.posts()[0].body.request_id);
});

test.each(['', ' ', '\n\t'])('blank prompt %j never creates a run', async prompt => {
  await ready(); await app.type('prompt', prompt); await app.submit('composer');
  expect(app.posts()).toHaveLength(0);
});

test('rapid repeated submission produces one POST and preserves edits made while saving', async () => {
  await ready(); const pending = deferred();
  app.override(c => c.path === '/api/run', () => pending.promise);
  await app.type('prompt', 'first'); await app.submit('composer'); await app.submit('composer');
  expect(app.posts()).toHaveLength(1);
  expect(app.get('send').disabled).toBe(true);
  await app.type('prompt', 'second unsent instruction');
  pending.resolve(response({ session: 'one' }, 202)); await app.flush();
  await app.click('new');
  expect(app.get('prompt').value).toBe('second unsent instruction');
});

test.each(['lost acknowledgement', 'rejected', 'recovery offline'])('uncertain POST: %s preserves the draft and never replays it', async kind => {
  await ready();
  app.override(c => c.path === '/api/run', call => {
    app.state.states = [{ session: 'one', run_id: call.body.request_id, phase: kind === 'rejected' ? 'failed' : 'complete', notices: [] }];
    if (kind === 'recovery offline') app.override(c => c.path === '/api/status', () => { throw new Error('offline'); });
    if (kind === 'rejected') return response({ error: 'Worker failed' }, 409);
    throw new Error('Connection lost');
  });
  await app.type('prompt', 'keep until history inspected'); await app.submit('composer'); await app.poll();
  expect(app.posts()).toHaveLength(1);
  expect(app.get('prompt').value).toBe('keep until history inspected');
  expect(app.get('title').textContent).toBe('New conversation');
  expect(app.get('error').hidden).toBe(false);
});

test('sending in one chat does not steal focus from another chat', async () => {
  await ready({ chats: [chat(), chat('two', 'Second chat')] }); await app.open();
  const pending = deferred(); app.override(c => c.path === '/api/run', () => pending.promise);
  await app.type('prompt', 'for first'); await app.submit('composer');
  await app.open('two'); await app.type('prompt', 'for second');
  pending.resolve(response({ session: 'one' }, 202)); await app.flush();
  expect(app.get('title').textContent).toBe('Second chat');
  expect(app.get('prompt').value).toBe('for second');
});

test('an old disconnected POST cannot unlock a newer in-flight submission', async () => {
  await ready(); const old = deferred(), current = deferred();
  app.override(c => c.path === '/api/run', () => old.promise);
  await app.type('prompt', 'old'); await app.submit('composer');
  await app.click('disconnect'); await app.login();
  app.override(c => c.path === '/api/run', () => current.promise);
  await app.type('prompt', 'new'); await app.submit('composer');
  old.resolve(response({ session: 'one' }, 202)); await app.flush();
  expect(app.get('send').disabled).toBe(true);
  await app.submit('composer'); expect(app.posts()).toHaveLength(2);
  current.resolve(response({ session: 'one' }, 202)); await app.flush();
});

test.each(['running', 'awaiting_approval'])('%s chat refuses another run', async current => {
  await ready(); await app.open(); await phase(current);
  await app.type('prompt', 'must not send'); await app.submit('composer');
  expect(app.posts()).toHaveLength(0);
  expect(app.get('send').disabled).toBe(true);
});

test.each(['running', 'maintaining'])('messages queue during %s compaction and send once only after completion', async current => {
  await ready(); await app.open(); accept(); await phase(current, { operation: 'compact', compacting: true });
  await app.type('prompt', 'queued'); await app.submit('composer');
  expect(app.posts()).toHaveLength(0);
  expect(app.get('phase').textContent).toContain('Queued');
  await app.type('prompt', 'edited queue'); await phase('complete', { compacting: false }); await app.poll();
  expect(app.posts()).toHaveLength(1);
  expect(app.posts()[0].body.operation.prompt).toBe('edited queue');
});

test('clearing a queued prompt cancels it', async () => {
  await ready(); await app.open(); await phase('running', { compacting: true });
  await app.type('prompt', 'queued'); await app.submit('composer'); await app.type('prompt', '');
  await phase('complete'); expect(app.posts()).toHaveLength(0);
});

test.each(['paused', 'failed', 'awaiting_approval'])('a queued message is not sent when compaction becomes %s', async current => {
  await ready(); await app.open(); await phase('running', { operation: 'compact' });
  await app.type('prompt', 'queued'); await app.submit('composer'); await phase(current);
  expect(app.posts()).toHaveLength(0);
  expect(app.get('prompt').value).toBe('queued');
});

test('failed queued sends are never retried on polling', async () => {
  await ready(); await app.open(); await phase('running', { compacting: true });
  await app.type('prompt', 'queued'); await app.submit('composer');
  app.override(c => c.path === '/api/run', () => response({ error: 'uncertain' }, 409));
  await phase('complete'); await app.poll(); await app.poll();
  expect(app.posts()).toHaveLength(1);
  expect(app.get('prompt').value).toBe('queued');
});

test('archived chats cannot send, retry, compact or rewind', async () => {
  await ready(); app.details.get('one').archived = true; await app.open();
  expect(app.get('prompt').disabled).toBe(true);
  expect(app.get('compact').disabled).toBe(true);
  expect(app.get('rewind').disabled).toBe(true);
  expect(app.get('archive-chat').textContent).toBe('Restore chat');
  await app.type('prompt', 'no'); await app.submit('composer'); expect(app.posts()).toHaveLength(0);
});

test('pending history exposes retry and cancel; retry does not submit the composer draft', async () => {
  await ready(); accept(); app.details.get('one').pending = true; await app.open();
  expect(app.get('retry').hidden).toBe(false);
  expect(app.get('cancel-turn').hidden).toBe(false);
  expect(app.get('compact').disabled).toBe(true);
  await app.type('prompt', 'unsent'); await app.click('retry');
  expect(app.posts()[0].body.operation).toEqual({ action: 'retry' });
  expect(app.get('prompt').value).toBe('unsent');
});

test.each([true, false])('approval %s carries the exact run and approval identifiers', async allow => {
  await ready(); await app.open();
  await phase('awaiting_approval', { approval: { id: 'approval-one', description: 'write result.txt' } });
  app.override(c => c.path === '/api/approval', () => { app.state.states[0].approval = null; return response({}); });
  await app.click(allow ? 'allow' : 'deny');
  expect(app.posts('/api/approval')[0].body).toEqual({ run_id: 'run-one', approval_id: 'approval-one', allow });
  expect(app.get('approval').hidden).toBe(true);
  expect(app.get('allow').disabled).toBe(false);
  await app.click('allow'); expect(app.posts('/api/approval')).toHaveLength(1);
});

test('approval errors restore controls; pause uses the current run', async () => {
  await ready(); await app.open();
  await phase('awaiting_approval', { approval: { id: 'approval-one', description: 'write result.txt' } });
  app.override(c => c.path === '/api/approval', () => response({ error: 'Expired approval' }, 409));
  await app.click('deny'); expect(app.get('error').textContent).toBe('Expired approval');
  expect(app.get('deny').disabled).toBe(false);
  app.override(c => c.path === '/api/pause', () => { app.state.states[0].phase = 'paused'; return response({}); });
  await app.click('pause'); expect(app.posts('/api/pause')[0].body).toEqual({ run_id: 'run-one' });
  expect(app.get('phase').textContent).toBe('Paused · history saved');
  app.override(c => c.path === '/api/pause', () => response({ error: 'Already stopped' }, 409));
  await app.click('pause'); expect(app.get('error').textContent).toBe('Already stopped');
});

test.each([
  ['Enter', false, false, false, 1],
  ['Enter', true, false, false, 0],
  ['Enter', false, true, false, 0],
  ['Enter', false, false, true, 0],
  ['a', false, false, false, 0],
])('composer key %s shift=%s composing=%s touch=%s', async (key, shiftKey, isComposing, touch, count) => {
  await ready({ touch }); accept(); await app.type('prompt', 'keyboard');
  app.get('prompt').dispatchEvent(new KeyboardEvent('keydown', { key, shiftKey, isComposing, bubbles: true, cancelable: true }));
  await app.flush(); expect(app.posts()).toHaveLength(count);
});

test('live output, reasoning, errors and todo progress stay distinct from saved messages', async () => {
  await ready(); await app.open();
  await phase('running', { preview: 'Partial', preview_limited: true, thinking: 'Reasoning', notices: ['Reading files'], error: 'Notice', todos: [{ status: 'completed', content: 'Inspect' }, { status: 'in_progress', content: 'Implement' }] });
  expect(app.get('live').hidden).toBe(false);
  expect(app.get('preview').textContent).toContain('Preview limit reached');
  expect(app.get('thinking').textContent).toBe('Reasoning');
  expect(app.get('activity').textContent).toBe('Reading files');
  expect(app.get('chat-error').textContent).toBe('Notice');
  expect(app.get('todo-progress').textContent).toBe('1 of 2 done');
  expect(app.get('messages').textContent).not.toContain('Partial');
  await phase('complete', { todos: [{ status: 'completed', content: 'Inspect' }] });
  expect(app.get('todos').hidden).toBe(true);
  expect(app.get('live').hidden).toBe(true);
});
