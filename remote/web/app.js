'use strict';
const $ = id => document.getElementById(id);
let token = '', selected = null, states = [], info = null, nextBefore = null, nextSessions = null;
let polling = false, epoch = 0, connection = 0, historyKey = '', items = [], sessionItems = [], viewLimited = false;
let exportUrl = null;
let listVersion = 0, runsKey = '', searchTimer, dialogAction = null, dialogChat = null, dialogTip = null;
const drafts = new Map(), posting = new Set();
const draftKey = () => selected || 'new';
const state = () => states.find(s => s.session === selected) || {};
const active = s => ['running', 'awaiting_approval'].includes(s.phase);
const busy = () => active(state());
function requestId() { const b = crypto.getRandomValues(new Uint8Array(16)); b[6] = (b[6] & 15) | 64; b[8] = (b[8] & 63) | 128; const h = [...b].map(x => x.toString(16).padStart(2,'0')).join(''); return `${h.slice(0,8)}-${h.slice(8,12)}-${h.slice(12,16)}-${h.slice(16,20)}-${h.slice(20)}`; }
function error(message) { $('error').textContent = message; $('error').hidden = false; }
$('error').onclick = () => { $('error').hidden = true; };
async function api(path, body) {
  const version = connection;
  const response = await fetch('/api/' + path, { method: body ? 'POST' : 'GET', headers: { 'X-Builder-Token': token, ...(body ? { 'Content-Type': 'application/json' } : {}) }, body: body ? JSON.stringify(body) : undefined, signal: AbortSignal.timeout(12000), cache: 'no-store' });
  const value = await response.json().catch(() => ({ error: `Host returned HTTP ${response.status}` }));
  if (version !== connection) throw new Error('Connection changed');
  if (!response.ok) throw new Error(value.error || 'Request failed');
  return value;
}
function saveDraft() {
  const key = draftKey(), value = $('prompt').value;
  if (value && !drafts.has(key) && drafts.size >= 64) { error('64 unsent chat drafts are open. Send or clear a draft before opening another.'); return false; }
  if (value) drafts.set(key, value); else drafts.delete(key);
  return true;
}
const phaseLabel = phase => ({ idle: 'Ready', running: 'Working on your host', awaiting_approval: 'Waiting for approval', maintaining: 'Answer saved · updating memory', complete: 'Answer saved', paused: 'Paused · history saved', failed: 'Stopped · inspect history' })[phase] || 'Ready';
function controls() {
  const s = state(), pending = posting.has(draftKey()), archived = info?.archived;
  $('send').disabled = pending || busy() || archived;
  $('prompt').disabled = !!archived;
  $('retry').hidden = !selected || busy() || !info?.pending || archived; $('retry').disabled = pending;
  $('cancel-turn').hidden = $('retry').hidden; $('cancel-turn').disabled = pending;
  $('pause').hidden = !active(s) && s.phase !== 'maintaining';
  $('phase').textContent = archived ? 'Archived · restore this chat to continue' : pending ? 'Saving your request…' : phaseLabel(s.phase);
  $('activity').textContent = busy() ? s.notices?.at(-1) || '' : '';
  $('live').hidden = !s.preview; $('preview').textContent = (s.preview || '') + (s.preview_limited ? '\n[Preview limit reached. The complete committed response will appear in history.]' : '');
  $('approval').hidden = !s.approval;
  if (s.approval) $('action').textContent = s.approval.description;
  $('chat-error').hidden = !s.error; $('chat-error').textContent = s.error || '';
  $('empty').hidden = items.length > 0 || busy(); $('chat-tools').hidden = !selected;
  for (const id of ['rename', 'archive-chat', 'rewind', 'compact']) $(id).disabled = pending || busy() || s.phase === 'maintaining' || !info;
  $('rewind').disabled ||= !!archived || !items.some(e => e.active && e.message.role === 'user');
  $('compact').disabled ||= !!archived || !!info?.pending;
  $('archive-chat').textContent = archived ? 'Restore chat' : 'Archive';
  $('profile').disabled = !!selected || pending;
  if (info?.session?.profile) $('profile').value = info.session.profile;
  const count = states.filter(s => active(s) || s.phase === 'maintaining').length;
  $('running-count').textContent = `${count} of 4 chats running · drafts stay in this tab`;
}
function prose(content) {
  const div = document.createElement('div'); div.className = 'prose';
  const parts = content.split(/(^```[^\n]*\n[\s\S]*?^```\s*$)/gm);
  for (const part of parts) {
    if (!part) continue;
    if (part.startsWith('```')) {
      const first = part.indexOf('\n'), code = part.slice(first + 1).replace(/\n```\s*$/, '');
      const block = document.createElement('div'), heading = document.createElement('div'), label = document.createElement('span'), copy = document.createElement('button'), pre = document.createElement('pre');
      block.className = 'code-block'; heading.className = 'code-heading'; label.textContent = part.slice(3, first) || 'code'; copy.className = 'quiet'; copy.textContent = 'Copy'; pre.textContent = code;
      copy.onclick = async () => { try { await navigator.clipboard.writeText(code); copy.textContent = 'Copied'; } catch { error('Clipboard unavailable. Select the code and copy it manually.'); } };
      heading.append(label, copy); block.append(heading, pre); div.append(block);
    } else { const p = document.createElement('p'); p.textContent = part.trim(); div.append(p); }
  }
  return div;
}
function renderMessages() {
  const pane = document.querySelector('.conversation'), bottom = pane.scrollHeight - pane.scrollTop - pane.clientHeight < 90;
  const expanded = new Set([...$('messages').querySelectorAll('article:has(details[open])')].map(el => el.dataset.seq));
  const fragment = document.createDocumentFragment();
  for (const entry of items) {
    const m = entry.message, article = document.createElement('article'); article.className = 'message ' + m.role; article.dataset.seq = String(entry.seq);
    const label = document.createElement('div'); label.className = 'message-label'; label.textContent = (m.role === 'assistant' ? 'BUILDER' : m.role.toUpperCase()) + (entry.active ? '' : ' · REWOUND'); article.append(label);
    if (!entry.active) article.setAttribute('aria-disabled', 'true');
    const pre = document.createElement('pre'); pre.textContent = m.content || '';
    if (m.tool_calls?.length) pre.textContent += '\n' + m.tool_calls.map(c => c.function.name + '\n' + c.function.arguments).join('\n\n');
    if (m.role === 'tool' || m.role === 'system' || m.tool_calls?.length) { const details = document.createElement('details'), summary = document.createElement('summary'); summary.textContent = m.tool_calls?.map(c => c.function.name).join(', ') || (m.role === 'system' ? 'Workspace instructions' : 'Tool result'); details.open = expanded.has(String(entry.seq)); details.append(summary, pre); article.append(details); } else article.append(m.role === 'assistant' ? prose(m.content || '') : pre);
    fragment.append(article);
  }
  $('messages').replaceChildren(fragment); $('older').hidden = nextBefore === null || viewLimited; controls();
  if (bottom) pane.scrollTop = pane.scrollHeight;
}
async function history(older = false) {
  if (!selected || viewLimited) return;
  const id = selected, version = epoch, archive = $('archived').checked;
  const page = await api(`sessions/${id}/messages?include_archived=${archive}${older && nextBefore !== null ? '&before=' + nextBefore : ''}`);
  if (selected !== id || version !== epoch) return;
  const merged = new Map((older ? items : items.filter(e => e.seq < (page.entries[0]?.seq ?? Infinity))).map(e => [e.seq, e]));
  page.entries.forEach(e => merged.set(e.seq, e));
  const candidate = [...merged.values()].sort((a,b) => a.seq-b.seq);
  if (candidate.length > 500 || JSON.stringify(candidate).length > 8 * 1024 * 1024) { viewLimited = true; $('older').hidden = true; error('Transcript display limit reached. Reopen this chat for the latest page, or export it. All original messages remain saved.'); return; }
  const changed = candidate.length !== items.length || candidate.some((entry,i) => entry.seq !== items[i]?.seq || entry.active !== items[i]?.active);
  items = candidate;
  if (older || items.length === page.entries.length) nextBefore = page.next_before;
  if (changed) renderMessages(); else controls();
}
function renderSessions() {
  const fragment = document.createDocumentFragment();
  for (const s of sessionItems) {
    const button = document.createElement('button'), title = document.createElement('span'), detail = document.createElement('small'), run = states.find(r => r.session === s.id);
    title.className = 'chat-name'; title.textContent = s.title; button.title = s.title;
    detail.textContent = run ? phaseLabel(run.phase) : s.profile;
    if (drafts.has(s.id)) detail.textContent += ' · Draft';
    button.className = (s.id === selected ? 'selected ' : '') + (run?.approval ? 'attention' : '');
    button.setAttribute('aria-current', s.id === selected ? 'page' : 'false');
    button.append(title, detail); button.onclick = () => openSession(s.id, s.title); fragment.append(button);
  }
  if (!sessionItems.length) { const p = document.createElement('p'); p.textContent = 'No matching chats.'; fragment.append(p); }
  $('sessions').replaceChildren(fragment); $('more-sessions').hidden = nextSessions === null;
}
async function sessions(more = false) {
  const version = ++listVersion, search = $('search').value, archive = $('chat-filter').value;
  const page = await api(`sessions?offset=${more ? nextSessions : 0}&archived=${archive}&search=${encodeURIComponent(search)}`);
  if (version !== listVersion) return;
  nextSessions = page.next_offset;
  sessionItems = more ? [...sessionItems, ...page.sessions] : page.sessions;
  renderSessions();
}
async function metadata() {
  if (!selected) return;
  const id = selected, version = epoch, result = await api(`sessions/${id}`);
  if (id !== selected || version !== epoch) return;
  info = result; $('title').textContent = info.session.title; controls();
}
async function openSession(id, title) {
  if (!saveDraft()) return;
  selected = id; epoch++; info = null; viewLimited = false; items = []; nextBefore = null; historyKey = '';
  $('prompt').value = drafts.get(draftKey()) || ''; $('title').textContent = title || 'What are we building?';
  renderMessages(); renderSessions();
  try {
    if (id) { const version = epoch; await Promise.all([history(), metadata()]); if (version !== epoch) return; if (info?.draft && !drafts.has(id) && !$('prompt').value && drafts.size < 64) { drafts.set(id, info.draft); $('prompt').value = info.draft; } if (info?.draft_too_large) error('The saved composer draft exceeds the browser limit. Recover it on the host CLI.'); document.querySelector('.conversation').scrollTop = document.querySelector('.conversation').scrollHeight; }
  } catch (e) { error(e.message); }
  if (!id) $('prompt').focus();
}
async function refresh() {
  if (polling || !token) return;
  polling = true;
  try {
    const result = await api('status'); if (!token) return;
    states = result.states; $('workspace').textContent = result.workspace; $('workspace').title = result.workspace;
    $('permission').textContent = { ask: 'Changes need approval', 'read-only': 'Read only', trust: 'Auto · all tools approved' }[result.approval_mode];
    controls(); renderSessions();
    const allKey = states.map(s => s.run_id + ':' + s.phase).join('|');
    if (allKey !== runsKey) { runsKey = allKey; await sessions(); }
    const s = state(), key = [s.run_id, s.phase, s.notices?.length, selected].join('|');
    if (selected && (busy() || key !== historyKey)) { const version = epoch; await Promise.all([history(), metadata()]); if (version === epoch) historyKey = key; }
  } catch (e) { if (token) error(e.message + ' · No action is automatically resubmitted.'); } finally { polling = false; }
}
$('connect').onsubmit = async event => {
  event.preventDefault(); const version = ++connection; token = $('token').value.trim(); $('token').value = '';
  try {
    const [result, profiles] = await Promise.all([api('status'), api('profiles')]); states = result.states;
    $('profile').replaceChildren(...profiles.profiles.map(p => { const option = document.createElement('option'); option.value = p.name; option.textContent = `${p.name} · ${p.model}`; return option; })); $('profile').value = profiles.default_profile;
    $('login').hidden = true; $('app').hidden = false; $('error').hidden = true; await sessions();
    const latest = states.at(-1); if (latest?.session) await openSession(latest.session); else await openSession(null); await refresh();
  } catch (e) { if (version !== connection) return; token = ''; $('app').hidden = true; $('login').hidden = false; error(e.message); }
};
$('disconnect').onclick = () => { token = ''; connection++; epoch++; listVersion++; selected = null; items = []; states = []; info = null; sessionItems = []; drafts.clear(); posting.clear(); $('manage-dialog').close(); $('export-dialog').close(); $('app').hidden = true; $('login').hidden = false; $('messages').replaceChildren(); $('sessions').replaceChildren(); $('prompt').value = ''; $('error').hidden = true; };
$('new').onclick = () => openSession(null);
$('more-sessions').onclick = () => sessions(true).catch(e => error(e.message));
$('search').oninput = () => { clearTimeout(searchTimer); searchTimer = setTimeout(() => sessions().catch(e => error(e.message)), 250); };
$('chat-filter').onchange = () => sessions().catch(e => error(e.message));
$('older').onclick = () => history(true).catch(e => error(e.message));
$('archived').onchange = () => { epoch++; viewLimited = false; items = []; nextBefore = null; renderMessages(); history().catch(e => error(e.message)); };
$('prompt').oninput = () => { saveDraft(); renderSessions(); };
async function run(action) {
  const id = selected, key = draftKey(), version = connection, request_id = requestId();
  if (posting.has(key) || busy() || info?.archived) return;
  const submittedDraft = $('prompt').value;
  if (!saveDraft()) return; posting.add(key); controls(); $('error').hidden = true;
  async function accepted(result) {
    if (action.action === 'message' && drafts.get(key) === submittedDraft) { drafts.delete(key); if (draftKey() === key) $('prompt').value = ''; }
    if (selected === id) await openSession(result.session);
    await sessions(); await refresh();
  }
  try { await accepted(await api('run', { request_id, session: id, profile: id ? null : $('profile').value, operation: action })); }
  catch (e) {
    if (version !== connection) return;
    error(e.message + ' · Check this chat before sending again.');
    // A lost acknowledgement is recovered only by its exact request ID. Never replay a write.
    try { const result = await api('status'); states = result.states; const found = states.find(s => s.run_id === request_id); if (found) { await sessions(); await refresh(); } /* Keep the draft until the user inspects durable history. */ } catch { /* Original error remains visible. */ }
  } finally { posting.delete(key); controls(); }
}
$('composer').onsubmit = e => { e.preventDefault(); const prompt = $('prompt').value; if (prompt.trim()) run({ action: 'message', prompt }); };
$('prompt').onkeydown = e => { if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) { e.preventDefault(); $('composer').requestSubmit(); } };
$('retry').onclick = () => run({ action: 'retry' });
$('compact').onclick = () => showDialog('compact', 'Compact context?', 'Builder will summarize older context for this chat. The complete original transcript stays saved and exportable.', 'Compact');
$('pause').onclick = async () => { const run_id = state().run_id; try { await api('pause', { run_id }); await refresh(); } catch (e) { error(e.message); } };
async function approve(allow) { const s = state(); if (!s.approval) return; $('allow').disabled = $('deny').disabled = true; try { await api('approval', { run_id: s.run_id, approval_id: s.approval.id, allow }); await refresh(); } catch (e) { error(e.message); } finally { $('allow').disabled = $('deny').disabled = false; } }
$('allow').onclick = () => approve(true); $('deny').onclick = () => approve(false);
function showDialog(action, title, description, confirm) {
  dialogAction = action; dialogChat = selected; dialogTip = info?.tip;
  $('dialog-title').textContent = title; $('dialog-description').textContent = description; $('dialog-confirm').textContent = confirm;
  $('chat-name').hidden = $('rename-label').hidden = action !== 'rename'; $('chat-name').required = action === 'rename'; $('chat-name').value = info?.session.title || '';
  $('manage-dialog').showModal(); if (action === 'rename') $('chat-name').select();
}
$('rename').onclick = () => showDialog('rename', 'Name this chat', 'Choose a name that makes this conversation easy to find.', 'Save name');
$('rewind').onclick = () => showDialog('rewind', 'Rewind the last turn?', 'The latest user turn and its replies will be archived. Its prompt returns to the composer, replacing any unsent draft in this chat. Files and commands already executed are not undone.', 'Rewind turn');
$('cancel-turn').onclick = () => showDialog('cancel', 'Cancel the saved turn?', 'Close this unfinished turn without replaying tools. Completed effects remain; uncertain tool execution still requires inspection on the host.', 'Cancel turn');
$('dialog-cancel').onclick = () => $('manage-dialog').close();
async function changeChat(id, change) {
  const result = await api(`sessions/${id}`, change);
  states = states.filter(s => s.session !== id);
  if (change.action === 'rewind') { if (result.draft) drafts.set(id, result.draft); else drafts.delete(id); if (selected === id) $('prompt').value = result.draft || ''; if (!result.draft) error('The rewound prompt exceeds the browser limit. Recover it with the host CLI.'); }
  if (result.uncertain) error(`${result.uncertain} uncertain tool execution(s) need inspection on the host before continuing.`);
  if (selected === id) await openSession(id);
  await sessions();
}
$('manage-form').onsubmit = async e => {
  e.preventDefault(); const action = dialogAction, id = dialogChat, tip = dialogTip; $('manage-dialog').close();
  try { if (action === 'compact') { if (selected === id) await run({ action: 'compact' }); } else await changeChat(id, { action, ...(action === 'rename' ? { title: $('chat-name').value } : { expected_tip: tip }) }); } catch (e) { error(e.message + ' · Refresh this chat before trying again.'); }
};
$('archive-chat').onclick = async () => { const id = selected; try { await changeChat(id, { action: 'archive', archived: !info.archived }); } catch (e) { error(e.message); } };
$('export').onclick = async () => {
  const id = selected, title = info?.session.title || 'Builder chat', version = connection, include = $('archived').checked; $('export').disabled = true;
  try {
    let before = null, entries = [], bytes = 0;
    do { const page = await api(`sessions/${id}/messages?include_archived=${include}${before === null ? '' : '&before=' + before}`); entries = [...page.entries, ...entries]; bytes += JSON.stringify(page.entries).length; if (entries.length > 5000 || bytes > 32 * 1024 * 1024) throw new Error('Browser export limit reached (5,000 messages / 32 MiB). Use builder export on the host for the full transcript.'); before = page.next_before; } while (before !== null);
    const text = JSON.stringify({ title, session: id, includes_rewound: include, entries }, null, 2);
    if (exportUrl) URL.revokeObjectURL(exportUrl);
    exportUrl = URL.createObjectURL(new Blob([text], { type: 'application/json' }));
    $('download-transcript').href = exportUrl; $('download-transcript').download = `builder-${id}.json`;
    $('export-text').value = text; $('copy-transcript').textContent = 'Copy transcript'; $('export-dialog').showModal();
  } catch (e) { if (version === connection) error(e.message); } finally { $('export').disabled = false; }
};
$('close-export').onclick = () => $('export-dialog').close();
$('export-dialog').onclose = () => { if (exportUrl) URL.revokeObjectURL(exportUrl); exportUrl = null; $('download-transcript').removeAttribute('href'); $('export-text').value = ''; };
$('copy-transcript').onclick = async () => { try { await navigator.clipboard.writeText($('export-text').value); $('copy-transcript').textContent = 'Copied'; } catch { $('export-text').select(); error('Clipboard unavailable. Copy the selected transcript text manually.'); } };
document.querySelectorAll('[data-prompt]').forEach(button => { button.onclick = () => { $('prompt').value = button.dataset.prompt; saveDraft(); $('prompt').focus(); }; });
setInterval(refresh, 1200);
