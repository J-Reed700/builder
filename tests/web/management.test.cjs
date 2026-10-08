const { boot, response, deferred, chat, entry } = require('./harness.cjs');
let app;
afterEach(() => app?.cleanup());
async function ready() { app = await boot({ chats: [chat(), chat('two', 'Second chat')] }); await app.login(); await app.open(); }

test('rename binds to the chat captured when the dialog was opened', async () => {
  await ready(); await app.click('rename');
  expect(app.get('manage-dialog').open).toBe(true);
  expect(app.get('chat-name').required).toBe(true);
  await app.open('two'); app.get('chat-name').value = 'Renamed 🦀';
  app.override(c => c.path === '/api/sessions/one' && c.method === 'POST', () => response({}));
  await app.submit('manage-form');
  expect(app.posts('/api/sessions/one')[0].body).toEqual({ action: 'rename', title: 'Renamed 🦀' });
  expect(app.get('title').textContent).toBe('Second chat');
});

test.each(['rewind', 'cancel'])('%s includes an optimistic history tip and reports uncertain effects', async action => {
  await ready(); await app.click(action === 'rewind' ? 'rewind' : 'cancel-turn');
  app.override(c => c.path === '/api/sessions/one' && c.method === 'POST', () => response({ draft: 'recovered instruction', uncertain: 1 }));
  await app.submit('manage-form');
  expect(app.posts('/api/sessions/one')[0].body).toEqual({ action, expected_tip: 2 });
  if (action === 'rewind') expect(app.get('prompt').value).toBe('recovered instruction');
  expect(app.get('error').textContent).toContain('uncertain tool execution');
});

test('rewinding an oversized prompt clears an old draft and directs recovery to the CLI', async () => {
  await ready(); await app.type('prompt', 'old draft'); await app.click('rewind');
  app.override(c => c.path === '/api/sessions/one' && c.method === 'POST', () => response({ draft: null }));
  await app.submit('manage-form');
  expect(app.get('prompt').value).toBe('');
  expect(app.get('error').textContent).toContain('rewound prompt exceeds');
});

test('a stale history mutation surfaces the conflict without replaying it', async () => {
  await ready(); await app.click('rewind');
  app.override(c => c.path === '/api/sessions/one' && c.method === 'POST', () => response({ error: 'History changed' }, 409));
  await app.submit('manage-form');
  expect(app.get('error').textContent).toContain('Refresh this chat before trying again');
  await app.poll(); expect(app.posts('/api/sessions/one')).toHaveLength(1);
});

test('canceling a confirmation makes no mutation', async () => {
  await ready(); await app.click('rewind'); await app.click('dialog-cancel');
  expect(app.get('manage-dialog').open).toBe(false);
  expect(app.posts('/api/sessions/one')).toHaveLength(0);
});

test('compaction requires confirmation and does not affect a different selected chat', async () => {
  await ready(); app.override(c => c.path === '/api/run', () => response({ session: 'one' }, 202));
  await app.click('compact'); expect(app.posts()).toHaveLength(0);
  await app.submit('manage-form'); expect(app.posts()[0].body.operation).toEqual({ action: 'compact' });
  await app.click('compact'); await app.open('two'); await app.submit('manage-form');
  expect(app.posts()).toHaveLength(1);
});

test('archive and restore use the saved archive state and show failures', async () => {
  await ready();
  app.state.states = [{ session: 'one', run_id: 'run', phase: 'complete', notices: [] }]; await app.poll();
  app.override(c => c.path === '/api/sessions/one' && c.method === 'POST', c => {
    app.details.get('one').archived = c.body.archived; return response({});
  });
  await app.click('archive-chat'); expect(app.get('archive-chat').textContent).toBe('Restore chat');
  await app.click('archive-chat'); expect(app.get('archive-chat').textContent).toBe('Archive');
  expect(app.posts('/api/sessions/one').map(c => c.body.archived)).toEqual([true, false]);
  app.override(c => c.path === '/api/sessions/one' && c.method === 'POST', () => response({ error: 'Busy' }, 409));
  await app.click('archive-chat'); expect(app.get('error').textContent).toBe('Busy');
});

test('Escape dismisses a folder dialog while a listing is pending', async () => {
  await ready(); await app.click('new'); await app.click('folder');
  const pending = deferred(); app.override(c => c.path === '/api/folders', () => pending.promise);
  app.get('folder-list').querySelector('button').click(); await app.flush();
  app.get('folder-dialog').dispatchEvent(new Event('cancel')); app.get('folder-dialog').close();
  pending.resolve(response({ path: 'src', parent: '', folders: [] })); await app.flush();
  expect(app.get('folder-dialog').open).toBe(false);
});

test('export paginates in order, copies, downloads and releases the object URL on close', async () => {
  await ready();
  app.override(c => c.path.endsWith('/messages'), c => response({ entries: c.query.has('before') ? [entry(1)] : [entry(2)], next_before: c.query.has('before') ? null : 2 }));
  app.get('archived').checked = true; await app.click('export');
  expect(app.get('export-dialog').open).toBe(true);
  const exported = JSON.parse(app.get('export-text').value);
  expect(exported).toEqual({ title: 'First chat', session: 'one', includes_rewound: true, entries: [entry(1), entry(2)] });
  expect(app.get('download-transcript').download).toBe('builder-one.json');
  expect(app.get('download-transcript').href).toBe('blob:fixture');
  await app.click('copy-transcript'); expect(navigator.clipboard.writeText).toHaveBeenCalledWith(app.get('export-text').value);
  navigator.clipboard.writeText.mockRejectedValue(new Error('denied')); await app.click('copy-transcript');
  expect(app.get('export-text').selectionEnd).toBe(app.get('export-text').value.length);
  expect(app.get('error').textContent).toContain('Copy the selected transcript');
  await app.click('export'); expect(URL.revokeObjectURL).toHaveBeenCalledWith('blob:fixture');
  await app.click('close-export');
  expect(app.get('export-text').value).toBe('');
  expect(app.get('download-transcript').hasAttribute('href')).toBe(false);
});

test.each(['count', 'size'])('export %s limit reports a CLI recovery path without producing a partial download', async kind => {
  await ready();
  const entries = kind === 'count' ? Array.from({ length: 5001 }, (_, i) => entry(i)) : [entry(1, 'user', 'x'.repeat(32 * 1024 * 1024))];
  app.override(c => c.path.endsWith('/messages'), () => response({ entries, next_before: null }));
  await app.click('export');
  expect(app.get('error').textContent).toContain('Browser export limit reached');
  expect(app.get('export-dialog').open).toBe(false);
  expect(URL.createObjectURL).not.toHaveBeenCalled();
  expect(app.get('export').disabled).toBe(false);
});

test('a disconnected export cannot open a dialog with stale data', async () => {
  await ready(); const pending = deferred();
  app.override(c => c.path.endsWith('/messages'), () => pending.promise);
  await app.click('export'); await app.click('disconnect');
  pending.resolve(response({ entries: [entry(1)], next_before: null })); await app.flush();
  expect(app.get('export-dialog').open).toBe(false);
  expect(app.get('error').hidden).toBe(true);
});

test('folder browsing navigates up and down, handles empty folders, and scopes new runs', async () => {
  await ready(); await app.click('new'); await app.click('folder');
  expect(app.get('folder-up').disabled).toBe(true);
  app.override(c => c.path === '/api/folders' && c.query.get('path') === 'src', () => response({ path: 'src', parent: '', folders: [], limited: true }));
  app.get('folder-list').querySelector('button').click(); await app.flush();
  expect(app.get('folder-list').textContent).toContain('No subfolders');
  expect(app.get('folder-list').textContent).toContain('first 500');
  expect(app.get('folder-path').textContent).toBe('src');
  await app.click('folder-up'); expect(app.get('folder-path').textContent).toBe('workspace root');
  app.get('folder-list').querySelector('button').click(); await app.flush(); await app.click('folder-use');
  expect(app.get('folder').textContent).toBe('src');
  app.override(c => c.path === '/api/run', () => response({ session: 'one' }, 202));
  await app.type('prompt', 'in src'); await app.submit('composer');
  expect(app.posts()[0].body.workspace).toBe('src');
});

test('stale folder responses cannot overwrite more recent navigation', async () => {
  await ready(); await app.click('new'); await app.click('folder');
  const pending = deferred();
  app.override(c => c.path === '/api/folders' && c.query.get('path') === 'src', () => pending.promise);
  app.get('folder-list').querySelectorAll('button')[0].click(); await app.flush();
  app.override(c => c.path === '/api/folders' && c.query.get('path') === 'docs', () => response({ path: 'docs', parent: '', folders: ['nested'] }));
  app.get('folder-list').querySelectorAll('button')[1].click(); await app.flush();
  pending.resolve(response({ path: 'src', parent: '', folders: [] })); await app.flush();
  expect(app.get('folder-path').textContent).toBe('docs');
  app.override(c => c.path === '/api/folders' && c.query.get('path') === 'docs/nested', () => { throw new Error('Cannot read folder'); });
  app.get('folder-list').querySelector('button').click(); await app.flush();
  expect(app.get('error').textContent).toBe('Cannot read folder');
  await app.click('folder-cancel'); expect(app.get('folder-dialog').open).toBe(false);
  app.override(c => c.path === '/api/folders', () => { throw new Error('gone'); });
  await app.click('folder'); expect(app.get('error').textContent).toBe('gone');
});

test.each(['folder-cancel', 'folder-use', 'disconnect'])('late folder listings cannot reopen a dialog after %s', async action => {
  await ready(); await app.click('new'); await app.click('folder');
  const pending = deferred();
  app.override(c => c.path === '/api/folders' && c.query.get('path') === 'src', () => pending.promise);
  app.get('folder-list').querySelector('button').click(); await app.flush();
  await app.click(action);
  pending.resolve(response({ path: 'src', parent: '', folders: [] })); await app.flush();
  expect(app.get('folder-dialog').open).toBe(false);
  expect(app.get('error').hidden).toBe(true);
});

test('mobile drawer traps focus, Escape restores focus, and desktop resize closes it', async () => {
  await ready();
  jest.spyOn(HTMLElement.prototype, 'getClientRects').mockReturnValue([{}]);
  await app.click('menu'); expect(app.get('workspace-main').inert).toBe(true);
  expect(document.activeElement).toBe(app.get('search'));
  const focusable = [...app.get('drawer').querySelectorAll('button, input, select')].filter(el => !el.disabled);
  focusable.at(-1).focus(); document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', cancelable: true }));
  expect(document.activeElement).toBe(focusable[0]);
  document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab', shiftKey: true, cancelable: true }));
  expect(document.activeElement).toBe(focusable.at(-1));
  document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }));
  expect(document.activeElement).toBe(app.get('menu')); expect(app.get('workspace-main').inert).toBe(false);
  await app.click('menu'); app.mediaListeners.get('(min-width: 561px)')({ matches: true });
  expect(app.get('side').getAttribute('data-open')).toBe('false'); expect(document.activeElement).toBe(app.get('search'));
  await app.click('menu'); await app.click('drawer-close'); expect(app.get('menu').getAttribute('aria-expanded')).toBe('false');
});

test('chat options dismiss on Escape, outside click, action click and restore focus after a dialog', async () => {
  await ready(); app.get('chat-tools').open = true;
  document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' })); expect(app.get('chat-tools').open).toBe(false);
  app.get('chat-tools').open = true;
  app.get('prompt').dispatchEvent(new Event('pointerdown', { bubbles: true })); expect(app.get('chat-tools').open).toBe(false);
  app.get('chat-tools').open = true; await app.click('rename'); expect(app.get('chat-tools').open).toBe(false);
  await app.click('dialog-cancel'); expect(document.activeElement).toBe(app.get('chat-tools').querySelector('summary'));
});

test('suggestions populate the composer without submitting', async () => {
  await ready(); await app.click('new'); const suggestion = document.querySelector('[data-prompt]');
  suggestion.click(); await app.flush();
  expect(app.get('prompt').value).toBe(suggestion.dataset.prompt); expect(document.activeElement).toBe(app.get('prompt'));
  expect(app.posts()).toHaveLength(0);
});
