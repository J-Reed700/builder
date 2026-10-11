const { test: base, expect } = require('@playwright/test');
const { createServer } = require('node:http');
const { spawn } = require('node:child_process');
const { mkdtemp, mkdir, writeFile, readFile, rm } = require('node:fs/promises');
const { tmpdir } = require('node:os');
const { join, resolve } = require('node:path');
const { setTimeout: delay } = require('node:timers/promises');

const answer = content => ({ role: 'assistant', content });
const write = (path, content) => ({ role: 'assistant', tool_calls: [{ id: 'fixture-write', type: 'function', function: { name: 'write_file', arguments: JSON.stringify({ path, content }) } }] });

async function listen(server) {
  await new Promise((resolve, reject) => {
    server.once('error', reject); server.listen(0, '127.0.0.1', resolve);
  });
  return server.address().port;
}
async function close(server) {
  server.closeAllConnections();
  await new Promise(resolve => server.close(resolve));
}

async function startHost() {
  const root = await mkdtemp(join(tmpdir(), 'builder-browser-'));
  const home = join(root, 'home'), workspace = join(root, 'workspace');
  await mkdir(home); await mkdir(join(workspace, 'src'), { recursive: true });
  await mkdir(join(workspace, 'empty'));
  await writeFile(join(workspace, 'src', 'example.txt'), 'fixture source\n');
  const requests = [], replies = [], held = [];
  const model = createServer(async (req, res) => {
    if (req.url !== '/v1/chat/completions') {
      res.writeHead(404); res.end(); return;
    }
    try {
      let data = ''; for await (const chunk of req) data += chunk;
      const body = JSON.parse(data); requests.push(body);
      const step = replies.shift() || { message: answer('Fixture answer saved.') };
      const respond = () => {
        if (res.destroyed) return;
        if (step.status) { res.writeHead(step.status, { 'Content-Type': 'application/json' }); res.end(JSON.stringify({ error: { message: 'Scripted provider failure' } })); return; }
        const message = step.message || answer('Fixture answer saved.');
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({ choices: [{ message, finish_reason: message.tool_calls ? 'tool_calls' : 'stop' }] }));
      };
      if (step.hold) held.push(respond); else respond();
    } catch (error) { res.writeHead(500); res.end(String(error)); }
  });
  let child, logs = '', exited;
  try {
    const modelPort = await listen(model);
    await writeFile(join(home, 'config.toml'), `default_profile = "local"\n[memory]\nenabled = false\n[profiles.local]\nbase_url = "http://127.0.0.1:${modelPort}/v1"\nmodel = "browser-fixture"\nstream = false\nmax_attempts = 1\n[profiles.local.pipeline]\nenabled = false\n`);
    // Reserve an available loopback port, release it, then let the real CLI bind
    // it. A collision fails startup with the captured host log instead of sharing
    // an existing app or another test's data.
    const reservation = createServer(); const port = await listen(reservation); await close(reservation);
    const url = `http://127.0.0.1:${port}`;
    const binary = process.env.BUILDER_TEST_BINARY || resolve(process.env.CARGO_TARGET_DIR || 'target', 'debug', process.platform === 'win32' ? 'builder.exe' : 'builder');
    const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith('BUILDER_')));
    child = spawn(binary, ['--home', home, '-C', workspace, 'remote', '--listen', `127.0.0.1:${port}`], { env, stdio: ['ignore', 'pipe', 'pipe'] });
    exited = new Promise(resolve => { child.once('exit', resolve); child.once('error', error => { logs += String(error); resolve(); }); });
    child.stdout.on('data', chunk => { logs += chunk; }); child.stderr.on('data', chunk => { logs += chunk; });
    const deadline = Date.now() + 15_000;
    while (!logs.includes('Token file:')) {
      if (child.exitCode !== null || Date.now() > deadline) throw new Error(`Builder did not start:\n${logs}`);
      await delay(25);
    }
    const token = await readFile(join(home, 'remote-token'), 'utf8');
    const api = async (path, body) => {
      const res = await fetch(`${url}/api/${path}`, {
        method: body ? 'POST' : 'GET', headers: { 'X-Builder-Token': token, Origin: url, 'Content-Type': 'application/json' },
        body: body ? JSON.stringify(body) : undefined, signal: AbortSignal.timeout(10_000),
      });
      const result = await res.json(); if (!res.ok) throw new Error(`${res.status}: ${JSON.stringify(result)}`); return result;
    };
    return {
      url, token, workspace, requests, replies, api, logs: () => logs,
      release() { held.splice(0).forEach(respond => respond()); },
      async dispose() {
        child.kill(process.platform === 'win32' ? 'SIGTERM' : 'SIGINT');
        const stopped = await Promise.race([exited.then(() => true), delay(2000).then(() => false)]);
        if (!stopped) { child.kill('SIGKILL'); await exited; }
        await close(model); await rm(root, { recursive: true, force: true });
      },
    };
  } catch (error) {
    if (child && child.exitCode === null) { child.kill('SIGKILL'); await exited; }
    await close(model); await rm(root, { recursive: true, force: true }); throw error;
  }
}

const test = base.extend({
  host: async ({}, use, testInfo) => {
    const host = await startHost();
    try { await use(host); }
    finally {
      if (testInfo.status !== testInfo.expectedStatus) await testInfo.attach('host.log', { body: host.logs(), contentType: 'text/plain' });
      await host.dispose();
    }
  },
  // Every browser journey also fails on an uncaught production JavaScript error.
  page: async ({ page }, use) => {
    const errors = []; page.on('pageerror', error => errors.push(error.message));
    await use(page); expect(errors).toEqual([]);
  },
});

async function login(page, host, remember = false) {
  await page.goto(host.url);
  await page.getByLabel('Host access token').fill(host.token);
  await page.getByLabel('Remember this browser').setChecked(remember);
  await page.getByRole('button', { name: 'Connect to workspace' }).click();
  await expect(page.locator('#app')).toBeVisible();
}
async function send(page, prompt) {
  await page.getByRole('textbox', { name: 'Message Builder' }).fill(prompt);
  await page.getByRole('button', { name: 'Send', exact: true }).click();
}
async function finished(page) { await expect(page.locator('#phase')).toHaveText('Answer saved'); }
async function drawer(page) {
  const menu = page.getByRole('button', { name: '☰ Chats' });
  if (await menu.isVisible()) await menu.click();
}
async function option(page, name) {
  await page.getByText('Conversation options', { exact: true }).click();
  await page.getByRole('button', { name, exact: true }).click();
}
module.exports = { test, expect, answer, write, login, send, finished, drawer, option };
