const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const html = readFileSync(join(__dirname, '../../remote/web/index.html'), 'utf8');

function response(value, status = 200) {
  return { ok: status >= 200 && status < 300, status, json: async () => value };
}

function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

function chat(id = 'one', title = 'First chat', extra = {}) {
  return { id, title, profile: 'local', folder: '', ...extra };
}

function entry(seq, role = 'user', content = `Message ${seq}`, active = true, extra = {}) {
  return { seq, active, message: { role, content, ...extra } };
}

// Run the unmodified production script against the actual HTML. Only browser
// capabilities absent from jsdom and the HTTP boundary are substituted.
async function boot(options = {}) {
  jest.useFakeTimers();
  jest.resetModules();
  document.documentElement.innerHTML = html;
  localStorage.clear();
  if (options.remembered) localStorage.setItem('builder-remote-token', options.remembered);
  const listeners = [];
  const add = document.addEventListener.bind(document);
  jest.spyOn(document, 'addEventListener').mockImplementation((...args) => {
    listeners.push(args); add(...args);
  });
  const mediaListeners = new Map();
  window.matchMedia = jest.fn(query => ({
    matches: query === '(pointer: coarse)' && !!options.touch,
    addEventListener: (_, handler) => mediaListeners.set(query, handler),
  }));
  HTMLDialogElement.prototype.showModal = function () { this.open = true; };
  HTMLDialogElement.prototype.close = function () {
    if (!this.open) return;
    this.open = false; this.dispatchEvent(new Event('close'));
  };
  URL.createObjectURL = jest.fn(() => 'blob:fixture');
  URL.revokeObjectURL = jest.fn();
  Object.defineProperty(navigator, 'clipboard', {
    configurable: true, value: { writeText: jest.fn().mockResolvedValue(undefined) },
  });
  const state = {
    states: [], workspace: '/fixture', approval_mode: 'ask', approval_locked: false,
    ...options.status,
  };
  const chats = options.chats || [chat()];
  const details = new Map(chats.map(s => [s.id, {
    session: s, archived: false, pending: false, tip: 2, folder: s.folder,
  }]));
  const messages = new Map(chats.map(s => [s.id, [entry(1), entry(2, 'assistant', 'Saved answer')]]));
  const calls = [], overrides = options.overrides ? [...options.overrides] : [];
  const gateway = { status: options.gateway ?? null };
  global.fetch = jest.fn(async (path, init = {}) => {
    const url = new URL(path, 'http://localhost');
    const method = init.method || 'GET';
    const body = init.body ? JSON.parse(init.body) : undefined;
    const call = { path: url.pathname, query: url.searchParams, method, body, init };
    calls.push(call);
    const override = overrides.findLast(rule => rule.match(call));
    if (override) return override.handle(call);
    if (url.pathname === '/api/gateway/status') return response(gateway.status, gateway.status ? 200 : 404);
    if (url.pathname === '/api/status') return response(structuredClone(state));
    if (url.pathname === '/api/profiles') return response({ default_profile: 'local', profiles: [{ name: 'local', model: 'fixture' }, { name: 'other', model: 'second' }] });
    if (url.pathname === '/api/sessions') return response({ sessions: chats, next_offset: null });
    const history = url.pathname.match(/^\/api\/sessions\/([^/]+)\/messages$/);
    if (history) return response({ entries: messages.get(history[1]) || [], next_before: null });
    const detail = url.pathname.match(/^\/api\/sessions\/([^/]+)$/);
    if (detail && method === 'GET') return response(details.get(detail[1]));
    if (url.pathname === '/api/folders') return response({ path: '', parent: null, folders: ['src', 'docs'], limited: false });
    throw new Error(`Unexpected ${method} ${url.pathname}`);
  });
  // Node's structuredClone is absent in some jsdom environments.
  global.structuredClone ??= value => JSON.parse(JSON.stringify(value));
  const get = id => document.getElementById(id);
  const flush = () => jest.advanceTimersByTimeAsync(0);
  require('../../remote/web/app.js');
  await flush();
  const app = {
    get, state, chats, details, messages, calls, gateway, mediaListeners, flush,
    override(match, handle) { overrides.push({ match, handle }); },
    async click(id) { get(id).click(); await flush(); },
    async type(id, value) { get(id).value = value; get(id).dispatchEvent(new Event('input', { bubbles: true })); await flush(); },
    async change(id, value) { get(id).value = value; get(id).dispatchEvent(new Event('change', { bubbles: true })); await flush(); },
    async submit(id) { get(id).dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); await flush(); },
    async login(remember = false) { get('token').value = '  fixture-token  '; get('remember').checked = remember; await app.submit('connect'); },
    async open(id = 'one') { const index = chats.findIndex(c => c.id === id); get('sessions').querySelectorAll('button')[index].click(); await flush(); },
    async poll() { await jest.advanceTimersByTimeAsync(1200); },
    posts(path = '/api/run') { return calls.filter(c => c.method === 'POST' && c.path === path); },
    cleanup() { listeners.forEach(args => document.removeEventListener(...args)); jest.clearAllTimers(); jest.useRealTimers(); },
  };
  return app;
}

module.exports = { boot, response, deferred, chat, entry };
