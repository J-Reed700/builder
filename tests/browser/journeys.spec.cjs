const { readFile } = require('node:fs/promises');
const { join } = require('node:path');
const { test, expect, answer, write, login, send, finished, drawer, option } = require('./fixture.cjs');

test('login errors recover, remembered sessions survive reload, and disconnect clears access', async ({ page, host }) => {
  await page.goto(host.url); await page.getByLabel('Host access token').fill('invalid');
  await page.getByRole('button', { name: 'Connect to workspace' }).click();
  await expect(page.getByRole('alert')).toContainText('token');
  await page.getByLabel('Host access token').fill(host.token);
  await page.getByRole('button', { name: 'Connect to workspace' }).click();
  await expect(page.locator('#app')).toBeVisible(); await page.reload();
  await expect(page.locator('#app')).toBeVisible();
  await drawer(page); await page.getByRole('button', { name: 'Disconnect', exact: true }).click();
  await page.reload(); await expect(page.getByLabel('Host access token')).toBeVisible();
  expect(host.requests).toHaveLength(0);
});

test('a Unicode conversation is durable across page reload and exports the actual saved transcript', async ({ page, host }) => {
  await login(page, host, true); const prompt = 'Keep these exact lines: 🦀\n中文 é 👨‍👩‍👧‍👦';
  await send(page, prompt); await finished(page);
  await expect(page.locator('#messages')).toContainText(prompt);
  expect(host.requests[0].messages.at(-1).content).toBe(prompt);
  await page.reload(); await expect(page.locator('#messages')).toContainText(prompt);
  await option(page, 'Export transcript'); await expect(page.getByRole('dialog')).toBeVisible();
  await page.getByText('Text version', { exact: true }).click();
  const data = JSON.parse(await page.getByLabel('Exported transcript JSON').inputValue());
  expect(data.entries.some(e => e.message.role === 'user' && e.message.content === prompt)).toBe(true);
  expect(data.entries.some(e => e.message.content === 'Fixture answer saved.')).toBe(true);
  const downloadPromise = page.waitForEvent('download');
  await page.getByRole('link', { name: 'Download JSON' }).click(); const download = await downloadPromise;
  expect(JSON.parse(await readFile(await download.path(), 'utf8'))).toEqual(data);
  await page.getByRole('button', { name: 'Done', exact: true }).click();
  await expect(page.locator('#export-dialog')).not.toBeVisible();
});

for (const allow of [true, false]) {
  test(`an actual file write is ${allow ? 'approved once' : 'denied'} from the browser`, async ({ page, host }) => {
    host.replies.push({ message: write('result.txt', 'approved content') }, { message: answer('Tool decision saved.') });
    await login(page, host); await send(page, 'Write result.txt');
    await expect(page.locator('#approval')).toBeVisible();
    await expect(page.locator('#action')).toContainText('result.txt');
    await expect(readFile(join(host.workspace, 'result.txt'), 'utf8')).rejects.toMatchObject({ code: 'ENOENT' });
    await page.getByRole('button', { name: allow ? 'Allow once' : 'Deny', exact: true }).click();
    if (allow) await finished(page);
    else { await expect(page.locator('#phase')).toHaveText('Stopped · inspect history'); await expect(page.locator('#chat-error')).toContainText('Blocked'); }
    await expect(page.locator('#approval')).not.toBeVisible();
    if (allow) expect(await readFile(join(host.workspace, 'result.txt'), 'utf8')).toBe('approved content');
    else await expect(readFile(join(host.workspace, 'result.txt'), 'utf8')).rejects.toMatchObject({ code: 'ENOENT' });
    expect(host.requests).toHaveLength(2);
    const sessions = await host.api('sessions'); const transcript = await host.api(`sessions/${sessions.sessions[0].id}/messages`);
    expect(transcript.entries.filter(e => e.message.role === 'tool')).toHaveLength(1);
  });
}

test('read-only permission refuses an actual file mutation without offering approval', async ({ page, host }) => {
  host.replies.push({ message: write('forbidden.txt', 'no') }, { message: answer('Mutation denied.') });
  await login(page, host); await page.getByLabel('Permissions').selectOption('read-only');
  await send(page, 'Try to write'); await expect(page.locator('#phase')).toHaveText('Stopped · inspect history');
  await expect(page.locator('#chat-error')).toContainText('Blocked');
  await expect(page.locator('#approval')).not.toBeVisible();
  await expect(readFile(join(host.workspace, 'forbidden.txt'))).rejects.toMatchObject({ code: 'ENOENT' });
});

test('a lost run acknowledgement preserves the draft and never duplicates a committed user message', async ({ page, host }) => {
  await login(page, host);
  let posts = 0;
  await page.route('**/api/run', async route => { posts++; await route.fetch(); await route.abort('connectionfailed'); });
  await send(page, 'only once');
  await expect(page.getByRole('alert')).toContainText('Check this chat before sending again');
  await expect(page.getByRole('textbox', { name: 'Message Builder' })).toHaveValue('only once');
  await expect.poll(() => host.requests.length).toBe(1);
  const sessions = await host.api('sessions'); const transcript = await host.api(`sessions/${sessions.sessions[0].id}/messages`);
  expect(transcript.entries.filter(e => e.message.role === 'user')).toHaveLength(1);
  // Observe several real polling cycles, using saved history as the UI barrier.
  await drawer(page); await page.locator('#sessions button').first().click();
  await expect(page.locator('#messages')).toContainText('only once');
  await finished(page); expect(posts).toBe(1);
});

test('rapid Enter presses during a running request cannot duplicate a turn', async ({ page, host }) => {
  host.replies.push({ hold: true }); await login(page, host); await send(page, 'single prompt');
  await expect.poll(() => host.requests.length).toBe(1);
  const prompt = page.getByRole('textbox', { name: 'Message Builder' }); await prompt.fill('wait for the first');
  await prompt.press('Enter'); await prompt.press('Enter');
  await expect(page.getByRole('button', { name: 'Send', exact: true })).toBeDisabled();
  host.release(); await finished(page); expect(host.requests).toHaveLength(1);
});

test('pausing a held model request preserves history and retry completes it once', async ({ page, host }) => {
  host.replies.push({ hold: true }); await login(page, host); await send(page, 'pause and retry');
  await expect.poll(() => host.requests.length).toBe(1);
  await page.getByRole('button', { name: 'Pause run', exact: true }).click();
  await expect(page.locator('#phase')).toHaveText('Paused · history saved');
  await page.getByRole('button', { name: 'Retry saved turn', exact: true }).click();
  await finished(page); expect(host.requests).toHaveLength(2);
  const sessions = await host.api('sessions'); const transcript = await host.api(`sessions/${sessions.sessions[0].id}/messages`);
  expect(transcript.entries.filter(e => e.message.role === 'user')).toHaveLength(1);
});

test('provider failure exposes retry and cancel and a retry recovers the saved prompt', async ({ page, host }) => {
  host.replies.push({ status: 503 }); await login(page, host); await send(page, 'retry after failure');
  await expect(page.locator('#phase')).toHaveText('Stopped · inspect history');
  await expect(page.getByRole('button', { name: 'Cancel saved turn', exact: true })).toBeVisible();
  await page.getByRole('button', { name: 'Retry saved turn', exact: true }).click();
  await finished(page); expect(host.requests).toHaveLength(2);
});

test('rename, search, archive and restore operate on the persisted chat', async ({ page, host }) => {
  await login(page, host); await send(page, 'manage this chat'); await finished(page);
  await option(page, 'Rename'); await page.getByLabel('Chat name', { exact: true }).fill('A named conversation 🦀');
  await page.getByRole('button', { name: 'Save name', exact: true }).click();
  await expect(page.locator('#title')).toHaveText('A named conversation 🦀');
  await drawer(page); await page.getByLabel('Search chats').fill('does not exist');
  await expect(page.locator('#sessions')).toHaveText('No matching chats.');
  await page.getByLabel('Search chats').fill('named'); await page.locator('#sessions button').first().click();
  await option(page, 'Archive'); await expect(page.getByRole('textbox', { name: 'Message Builder' })).toBeDisabled();
  await drawer(page); await page.getByLabel('Chat list').selectOption('true');
  await page.locator('#sessions button').first().click(); await option(page, 'Restore chat');
  await expect(page.getByRole('textbox', { name: 'Message Builder' })).toBeEnabled();
  const sessions = await host.api('sessions'); expect(sessions.sessions[0].title).toBe('A named conversation 🦀');
});

test('rewind restores the prompt while retaining exportable archived history', async ({ page, host }) => {
  await login(page, host); await send(page, 'original instruction'); await finished(page);
  await option(page, 'Rewind last turn');
  await expect(page.getByRole('dialog')).toContainText('Files and commands already executed are not undone');
  await page.getByRole('button', { name: 'Rewind turn', exact: true }).click();
  await expect(page.getByRole('textbox', { name: 'Message Builder' })).toHaveValue('original instruction');
  await page.getByText('Conversation options', { exact: true }).click();
  await page.getByLabel('Include rewound messages').check();
  await expect(page.locator('#messages')).toContainText('REWOUND');
  expect(host.requests).toHaveLength(1);
});

test('selected subfolder scopes a real tool write and existing chats keep that folder', async ({ page, host }) => {
  host.replies.push({ message: write('scoped.txt', 'inside src') }, { message: answer('Scoped write saved.') });
  await login(page, host); await page.locator('#folder').click();
  await page.getByRole('button', { name: 'src/', exact: true }).click();
  await page.getByRole('button', { name: 'Use this folder', exact: true }).click();
  await page.getByLabel('Permissions').selectOption('trust'); await send(page, 'write in src'); await finished(page);
  expect(await readFile(join(host.workspace, 'src', 'scoped.txt'), 'utf8')).toBe('inside src');
  await expect(readFile(join(host.workspace, 'scoped.txt'))).rejects.toMatchObject({ code: 'ENOENT' });
  await expect(page.locator('#folder')).toBeDisabled(); await expect(page.locator('#folder')).toHaveText('src');
});

test('untrusted model HTML renders as text without executing and code stays readable', async ({ page, host }) => {
  const content = '<img src=x onerror="window.pwned=true">\n<script>window.pwned=true</script>\n```js\nconst safe = "<b>";\n```';
  host.replies.push({ message: answer(content) }); await login(page, host); await send(page, 'show text'); await finished(page);
  await expect(page.locator('#messages')).toContainText('<script>window.pwned=true</script>');
  await expect(page.locator('#messages img, #messages script')).toHaveCount(0);
  expect(await page.evaluate(() => window.pwned)).toBeUndefined();
  await expect(page.locator('.code-block pre')).toHaveText('const safe = "<b>";');
});

test('drafts remain separate across saved chats and new conversations', async ({ page, host }) => {
  await login(page, host); await send(page, 'saved chat'); await finished(page);
  await page.getByRole('textbox', { name: 'Message Builder' }).fill('saved-chat draft');
  await page.getByRole('button', { name: 'New conversation', exact: true }).click();
  await page.getByRole('textbox', { name: 'Message Builder' }).fill('new-chat draft');
  await drawer(page); await page.locator('#sessions button').first().click();
  await expect(page.getByRole('textbox', { name: 'Message Builder' })).toHaveValue('saved-chat draft');
  await page.getByRole('button', { name: 'New conversation', exact: true }).click();
  await expect(page.getByRole('textbox', { name: 'Message Builder' })).toHaveValue('new-chat draft');
  expect(host.requests).toHaveLength(1);
});

test('canceling a paused turn saves the cancellation without another model request', async ({ page, host }) => {
  host.replies.push({ hold: true }); await login(page, host); await send(page, 'cancel this unfinished turn');
  await expect.poll(() => host.requests.length).toBe(1);
  await page.getByRole('button', { name: 'Pause run', exact: true }).click();
  await expect(page.locator('#phase')).toHaveText('Paused · history saved');
  await page.getByRole('button', { name: 'Cancel saved turn', exact: true }).click();
  await page.getByRole('button', { name: 'Cancel turn', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Retry saved turn', exact: true })).not.toBeVisible();
  const sessions = await host.api('sessions'); const detail = await host.api(`sessions/${sessions.sessions[0].id}`);
  expect(detail.pending).toBe(false); expect(host.requests).toHaveLength(1);
});

test('compaction queues the latest edited draft and preserves the original transcript', async ({ page, host }) => {
  await login(page, host);
  for (let turn = 0; turn < 4; turn++) {
    host.replies.push({ message: answer(`Evidence from turn ${turn}. `.repeat(100)) });
    await send(page, `Original instruction ${turn}`); await finished(page);
  }
  host.replies.push({ hold: true, message: answer('Earlier source evidence has been reviewed; continue with the latest instruction.') });
  await option(page, 'Compact context'); await page.getByRole('button', { name: 'Compact', exact: true }).click();
  await expect.poll(() => host.requests.length).toBe(5);
  await send(page, 'queued while compacting'); await expect(page.locator('#phase')).toHaveText('Queued · sends after compaction');
  await page.getByRole('textbox', { name: 'Message Builder' }).fill('edited queued instruction');
  expect(host.requests).toHaveLength(5); host.release();
  await expect.poll(() => host.requests.length).toBe(6); await finished(page);
  expect(host.requests[5].messages.at(-1).content).toBe('edited queued instruction');
  const sessions = await host.api('sessions'); const history = await host.api(`sessions/${sessions.sessions[0].id}/messages`);
  for (let turn = 0; turn < 4; turn++) {
    expect(history.entries.some(e => e.message.content === `Original instruction ${turn}`)).toBe(true);
  }
  expect(history.entries.filter(e => e.message.content === 'edited queued instruction')).toHaveLength(1);
});

test('long unbroken output fits the viewport and drawer keyboard focus stays contained', async ({ page, host }) => {
  host.replies.push({ message: answer('x'.repeat(500) + '\n```text\n' + 'y'.repeat(500) + '\n```') });
  await login(page, host); await send(page, 'long output'); await finished(page);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
  const menu = page.getByRole('button', { name: '☰ Chats' });
  if (await menu.isVisible()) {
    await menu.click(); await expect(page.getByLabel('Search chats')).toBeFocused();
    await page.getByRole('button', { name: 'Disconnect', exact: true }).focus(); await page.keyboard.press('Tab');
    await expect(page.getByRole('button', { name: '✕ Close', exact: true })).toBeFocused();
    await page.keyboard.press('Shift+Tab'); await expect(page.getByRole('button', { name: 'Disconnect', exact: true })).toBeFocused();
    await page.keyboard.press('Escape'); await expect(menu).toBeFocused();
    await expect(menu).toHaveAttribute('aria-expanded', 'false');
  }
});
