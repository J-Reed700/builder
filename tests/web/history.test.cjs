const { boot, response, deferred, chat, entry } = require('./harness.cjs');
let app;
afterEach(() => app?.cleanup());
async function ready(options) { app = await boot(options); await app.login(); }

test('switching chats preserves independent Unicode drafts and a new-chat draft', async () => {
  await ready({ chats: [chat(), chat('two', 'Second chat')] });
  await app.type('prompt', 'new 🦀'); await app.open(); await app.type('prompt', 'first 中文');
  await app.open('two'); await app.type('prompt', 'second 👨‍👩‍👧‍👦');
  await app.open(); expect(app.get('prompt').value).toBe('first 中文');
  expect(app.get('sessions').textContent).toContain('Draft');
  await app.click('new'); expect(app.get('prompt').value).toBe('new 🦀');
  await app.open('two'); expect(app.get('prompt').value).toBe('second 👨‍👩‍👧‍👦');
});

test('the 64-draft limit refuses navigation without discarding any draft', async () => {
  await ready({ chats: Array.from({ length: 65 }, (_, i) => chat(String(i), `Chat ${i}`)) });
  for (let i = 0; i < 64; i++) { await app.open(String(i)); await app.type('prompt', `draft ${i}`); }
  await app.open('64'); await app.type('prompt', 'draft 64'); await app.click('new');
  expect(app.get('title').textContent).toBe('Chat 64');
  expect(app.get('prompt').value).toBe('draft 64');
  expect(app.get('error').textContent).toContain('64 unsent chat drafts');
  await app.type('prompt', ''); await app.open('0'); expect(app.get('prompt').value).toBe('draft 0');
});

test('saved rewind drafts load only when no local draft exists', async () => {
  await ready(); app.details.get('one').draft = 'saved prompt'; await app.open();
  expect(app.get('prompt').value).toBe('saved prompt');
  await app.type('prompt', 'local edit'); await app.click('new'); await app.open();
  expect(app.get('prompt').value).toBe('local edit');
  app.details.get('one').draft_too_large = true; await app.open();
  expect(app.get('error').textContent).toContain('Recover it on the host CLI');
});

test('late metadata and history cannot replace the selected chat', async () => {
  await ready({ chats: [chat(), chat('two', 'Second chat')] });
  const metadata = deferred(), history = deferred();
  app.override(c => c.path === '/api/sessions/one', () => metadata.promise);
  app.override(c => c.path === '/api/sessions/one/messages', () => history.promise);
  await app.open(); await app.open('two');
  metadata.resolve(response({ session: chat('one', 'STALE'), draft: 'STALE' }));
  history.resolve(response({ entries: [entry(99, 'user', 'STALE')], next_before: null })); await app.flush();
  expect(app.get('title').textContent).toBe('Second chat');
  expect(app.get('messages').textContent).not.toContain('STALE');
  expect(app.get('prompt').value).toBe('');
});

test('session search is debounced, URL encoded, and ignores out-of-order responses', async () => {
  await ready(); const first = deferred();
  app.override(c => c.path === '/api/sessions' && c.query.get('search') === 'a&b?', () => first.promise);
  app.override(c => c.path === '/api/sessions' && c.query.get('search') === 'second', () => response({ sessions: [chat('two', 'Correct result')], next_offset: null }));
  const initial = app.calls.length;
  await app.type('search', 'a&b?'); expect(app.calls).toHaveLength(initial);
  await jest.advanceTimersByTimeAsync(250);
  await app.type('search', 'second'); await jest.advanceTimersByTimeAsync(250);
  first.resolve(response({ sessions: [chat('bad', 'STALE')], next_offset: null })); await app.flush();
  expect(app.get('sessions').textContent).toContain('Correct result');
  expect(app.get('sessions').textContent).not.toContain('STALE');
});

test('chat pagination, archive filter and empty search states are visible', async () => {
  await ready();
  app.override(c => c.path === '/api/sessions', c => response({ sessions: c.query.get('archived') === 'true' ? [] : [chat(c.query.get('offset') === '0' ? 'one' : 'two')], next_offset: c.query.get('offset') === '0' ? 1 : null }));
  await app.change('chat-filter', 'false'); expect(app.get('more-sessions').hidden).toBe(false);
  await app.click('more-sessions'); expect(app.get('sessions').querySelectorAll('button')).toHaveLength(2);
  expect(app.get('more-sessions').hidden).toBe(true);
  await app.change('chat-filter', 'true'); expect(app.get('sessions').textContent).toContain('No matching chats');
  app.override(c => c.path === '/api/sessions', () => { throw new Error('list offline'); });
  await app.change('chat-filter', 'false'); expect(app.get('error').textContent).toBe('list offline');
  await app.click('more-sessions'); expect(app.get('error').textContent).toBe('list offline');
  await app.type('search', 'fail'); await jest.advanceTimersByTimeAsync(250);
  expect(app.get('error').textContent).toBe('list offline');
});

test('history pagination deduplicates, orders messages and preserves expanded sections', async () => {
  await ready(); let older = false;
  app.override(c => c.path.endsWith('/messages'), c => {
    if (c.query.has('before')) { older = true; return response({ entries: [entry(1), entry(2)], next_before: null }); }
    return response({ entries: [entry(2), entry(3, 'assistant', 'Explanation', true, { reasoning: 'reason', tool_calls: [{ function: { name: 'read_file', arguments: '{}' } }] }), entry(4, 'tool', 'output')], next_before: 2 });
  });
  await app.open(); expect(app.get('older').hidden).toBe(false);
  const details = app.get('messages').querySelector('details'); details.open = true;
  await app.click('older'); expect(older).toBe(true);
  expect([...app.get('messages').querySelectorAll('article')].map(n => n.dataset.seq)).toEqual(['1', '2', '3', '4']);
  expect(app.get('messages').querySelector('details').open).toBe(true);
  expect(app.get('older').hidden).toBe(true);
});

test('including rewound messages refreshes active status without dropping the transcript', async () => {
  await ready(); await app.open();
  app.override(c => c.path.endsWith('/messages'), () => response({ entries: [entry(1, 'user', 'old', false), entry(2, 'system', 'instructions')], next_before: null }));
  app.get('archived').checked = true; await app.change('archived', 'on');
  expect(app.get('messages').querySelector('article').getAttribute('aria-disabled')).toBe('true');
  expect(app.get('messages').textContent).toContain('REWOUND');
  expect(app.get('messages').textContent).toContain('Workspace instructions');
  expect(app.calls.at(-1).query.get('include_archived')).toBe('true');
});

test.each(['count', 'size'])('transcript %s limit preserves the previously rendered history', async kind => {
  await ready(); await app.open();
  const entries = kind === 'count' ? Array.from({ length: 501 }, (_, i) => entry(i)) : [entry(3, 'user', 'x'.repeat(8 * 1024 * 1024))];
  app.override(c => c.path.endsWith('/messages'), () => response({ entries, next_before: 1 }));
  app.state.states = [{ session: 'one', run_id: 'new', phase: 'running', notices: [] }]; await app.poll();
  expect(app.get('error').textContent).toContain('Transcript display limit');
  expect(app.get('messages').textContent).toContain('Saved answer');
  expect(app.get('older').hidden).toBe(true);
});

test('model and user HTML are text, code fences are copyable, and clipboard failure is recoverable', async () => {
  await ready();
  app.messages.set('one', [entry(1, 'user', '<img src=x onerror=alert(1)>'), entry(2, 'assistant', '<script>attack()</script>\n\n```js\nconst x = "<b>";\n```\n\n```\nplain\n```')]);
  await app.open();
  expect(app.get('messages').querySelector('img, script, b')).toBeNull();
  expect(app.get('messages').textContent).toContain('<script>attack()</script>');
  const buttons = app.get('messages').querySelectorAll('button');
  buttons[0].click(); await app.flush();
  expect(navigator.clipboard.writeText).toHaveBeenCalledWith('const x = "<b>";');
  expect(buttons[0].textContent).toBe('Copied');
  navigator.clipboard.writeText.mockRejectedValue(new Error('denied'));
  buttons[1].click(); await app.flush(); expect(app.get('error').textContent).toContain('Select the code');
});

test('history failures can be retried from the visible earlier-messages control', async () => {
  await ready(); app.override(c => c.path.endsWith('/messages'), () => { throw new Error('history offline'); });
  await app.open(); expect(app.get('error').textContent).toBe('history offline');
  await app.click('older'); expect(app.get('error').textContent).toBe('history offline');
  await app.change('archived', 'on'); expect(app.get('error').textContent).toBe('history offline');
});
