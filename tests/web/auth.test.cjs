const { boot, response, deferred } = require('./harness.cjs');
let app;
afterEach(() => app?.cleanup());

test('local login is explicit, trims and clears the credential, and authenticates requests', async () => {
  app = await boot();
  expect(app.get('login').hidden).toBe(false);
  await app.login();
  expect(app.get('app').hidden).toBe(false);
  expect(app.get('token').value).toBe('');
  expect(app.get('workspace').textContent).toBe('/fixture');
  expect(localStorage.length).toBe(0);
  const status = app.calls.find(c => c.path === '/api/status');
  expect(status.init.headers['X-Builder-Token']).toBe('fixture-token');
  expect(status.init.cache).toBe('no-store');
  expect(status.init.signal).toBeDefined();
});

test('remembering a token is opt-in and disconnect clears credentials, chats and drafts', async () => {
  app = await boot();
  await app.login(true);
  expect(localStorage.getItem('builder-remote-token')).toBe('fixture-token');
  await app.type('prompt', 'private draft');
  await app.click('disconnect');
  expect(localStorage.length).toBe(0);
  expect(app.get('prompt').value).toBe('');
  expect(app.get('sessions').childElementCount).toBe(0);
  expect(app.get('messages').childElementCount).toBe(0);
  expect(app.get('login').hidden).toBe(false);
  await app.poll();
  expect(app.posts()).toHaveLength(0);
});

test('a saved credential reconnects automatically', async () => {
  app = await boot({ remembered: 'saved-token' });
  expect(app.get('app').hidden).toBe(false);
  expect(app.calls.find(c => c.path === '/api/status').init.headers['X-Builder-Token']).toBe('saved-token');
});

test.each([['invalid token', true], ['Host is busy', false]])('login error %s only discards an invalid remembered token', async (message, discarded) => {
  app = await boot();
  localStorage.setItem('builder-remote-token', 'previous');
  app.override(c => c.path === '/api/status', () => response({ error: message }, 401));
  await app.login(true);
  expect(app.get('app').hidden).toBe(true);
  expect(app.get('error').textContent).toBe(message);
  expect(localStorage.getItem('builder-remote-token')).toBe(discarded ? null : 'previous');
  await app.click('error');
  expect(app.get('error').hidden).toBe(true);
});

test('local storage restrictions do not block an authenticated tab', async () => {
  jest.spyOn(Storage.prototype, 'getItem').mockImplementation(() => { throw new Error('blocked'); });
  jest.spyOn(Storage.prototype, 'setItem').mockImplementation(() => { throw new Error('blocked'); });
  app = await boot();
  await app.login(true);
  expect(app.get('app').hidden).toBe(false);
});

test('non-JSON and empty errors remain readable', async () => {
  app = await boot();
  app.override(c => c.path === '/api/status', () => ({ ok: false, status: 502, json: async () => { throw new Error('HTML'); } }));
  await app.login();
  expect(app.get('error').textContent).toBe('Host returned HTTP 502');
  app.override(c => c.path === '/api/status', () => response({}, 500));
  await app.login();
  expect(app.get('error').textContent).toBe('Request failed');
});

test('a login response arriving after disconnect cannot reopen the app', async () => {
  app = await boot();
  const pending = deferred();
  app.override(c => c.path === '/api/status', () => pending.promise);
  await app.login();
  await app.click('disconnect');
  pending.resolve(response(app.state)); await app.flush();
  expect(app.get('app').hidden).toBe(true);
  expect(app.get('login').hidden).toBe(false);
  expect(app.get('error').hidden).toBe(true);
});

test('host read-only policy locks the permission selector', async () => {
  app = await boot({ status: { approval_locked: true } });
  await app.login();
  expect(app.get('approval-mode').value).toBe('read-only');
  expect(app.get('approval-mode').disabled).toBe(true);
  expect(app.get('permission').textContent).toBe('Read only');
});

test.each([
  [{ mode: 'gateway', connected: false, paired: false }, 'waiting for a computer'],
  [{ mode: 'gateway', connected: false, paired: true, device: { name: 'Workstation' } }, 'Workstation is offline'],
  [{ mode: 'gateway', connected: false, paired: true }, 'Your computer is offline'],
])('gateway setup shows pairing state %#', async (gateway, text) => {
  app = await boot({ gateway });
  expect(app.get('gateway-setup').hidden).toBe(false);
  expect(app.get('login').hidden).toBe(true);
  expect(app.get('gateway-state').textContent).toContain(text);
});

test('gateway reconnects on polling and never stores its placeholder credential', async () => {
  app = await boot({ gateway: { mode: 'gateway', connected: false } });
  app.gateway.status.connected = true;
  await jest.advanceTimersByTimeAsync(1500);
  expect(app.get('app').hidden).toBe(false);
  expect(localStorage.length).toBe(0);
  await app.click('disconnect');
  expect(app.get('gateway-setup').hidden).toBe(false);
});

test('connected gateway goes straight to the workspace', async () => {
  app = await boot({ gateway: { mode: 'gateway', connected: true } });
  expect(app.get('app').hidden).toBe(false);
});

test('pairing commands can be generated and copied, with clipboard fallback', async () => {
  app = await boot({ gateway: { mode: 'gateway', connected: false } });
  app.override(c => c.path.endsWith('/invitations'), () => response({ command: 'builder remote connect https://example.test --code fixture' }));
  await app.click('create-invitation');
  expect(app.get('invitation').hidden).toBe(false);
  expect(app.get('create-invitation').disabled).toBe(false);
  await app.click('copy-connect');
  expect(navigator.clipboard.writeText).toHaveBeenCalledWith(expect.stringContaining('--code fixture'));
  navigator.clipboard.writeText.mockRejectedValue(new Error('denied'));
  await app.click('copy-connect');
  expect(app.get('error').textContent).toContain('Select the command');
});

test('gateway and invitation errors can recover on a later poll', async () => {
  app = await boot({ gateway: { mode: 'gateway', connected: false } });
  app.override(c => c.path.endsWith('/invitations'), () => response({ error: 'Too many invitations' }, 429));
  await app.click('create-invitation');
  expect(app.get('error').textContent).toBe('Too many invitations');
  expect(app.get('create-invitation').disabled).toBe(false);
  app.override(c => c.path.endsWith('/invitations'), () => ({ ok: false, json: async () => { throw new Error('bad JSON'); } }));
  await app.click('create-invitation');
  expect(app.get('error').textContent).toBe('Could not create a pairing command');
  app.override(c => c.path === '/api/gateway/status', () => response({}, 503));
  await jest.advanceTimersByTimeAsync(1500);
  expect(app.get('error').textContent).toBe('Gateway is unavailable');
});

test('poll failures never submit a write and do not log the user out', async () => {
  app = await boot(); await app.login();
  app.override(c => c.path === '/api/status', () => { throw new Error('offline'); });
  await app.poll();
  expect(app.get('error').textContent).toContain('No action is automatically resubmitted');
  expect(app.get('app').hidden).toBe(false);
  expect(app.posts()).toHaveLength(0);
});

test.each(['unavailable', 'offline', 'invalid JSON', 'different mode'])('initial gateway detection handles %s', async kind => {
  app = await boot({ overrides: [{ match: c => c.path === '/api/gateway/status', handle: () => {
    if (kind === 'offline') throw new Error('Network offline');
    if (kind === 'invalid JSON') return { ok: true, status: 200, json: async () => { throw new Error('bad JSON'); } };
    return response(kind === 'different mode' ? { mode: 'local' } : { error: 'Sign in to gateway' }, kind === 'unavailable' ? 401 : 200);
  } }] });
  expect(app.get(kind === 'unavailable' ? 'gateway-setup' : 'login').hidden).toBe(false);
});

test('gateway host failure returns to setup and polling reports the disconnected host', async () => {
  app = await boot({ gateway: { mode: 'gateway', connected: true }, overrides: [{ match: c => c.path === '/api/status', handle: () => response({ error: 'Host offline' }, 503) }] });
  expect(app.get('gateway-setup').hidden).toBe(false);
  app.gateway.status.connected = false; await jest.advanceTimersByTimeAsync(1500);
  expect(app.get('gateway-state').textContent).toContain('waiting for a computer');
});

test('reconnection opens the latest running chat', async () => {
  app = await boot({ status: { states: [{ session: 'one', run_id: 'run', phase: 'running', notices: [] }] } });
  await app.login(); expect(app.get('title').textContent).toBe('First chat');
});
