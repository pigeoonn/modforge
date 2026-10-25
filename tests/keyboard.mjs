// Three things a person does most, done with the keyboard alone, the way someone who cannot use a mouse (or who
// uses a screen reader) would: pick a game and play it, find a mod and install it, and fix a crash.
// Every place the keyboard stops on must have a name a screen reader can say and must show that it has focus.
// tests/ui_smoke.mjs runs this; it returns a list of problems (empty = fine).

const KEYS = {
  Tab: { key: 'Tab', code: 'Tab', windowsVirtualKeyCode: 9 },
  Enter: { key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13, text: '\r' },
  Escape: { key: 'Escape', code: 'Escape', windowsVirtualKeyCode: 27 },
  ArrowDown: { key: 'ArrowDown', code: 'ArrowDown', windowsVirtualKeyCode: 40 },
  ArrowUp: { key: 'ArrowUp', code: 'ArrowUp', windowsVirtualKeyCode: 38 },
};
const SHIFT = 8, CTRL = 2;

// what the page answers about the focused element: its name (as a screen reader would say it) and whether focus shows
export const FOCUS_JS = `(() => {
  const e = document.activeElement;
  if (!e || e === document.body) return { tag: 'body' };
  const letters = s => /[\\p{L}\\p{N}]/u.test(s || '');
  const ref = id => { const n = document.getElementById(id); return n ? n.textContent : ''; };
  let name = e.getAttribute('aria-label') || (e.getAttribute('aria-labelledby') ? e.getAttribute('aria-labelledby').split(/\\s+/).map(ref).join(' ') : '');
  if (!letters(name) && e.labels && e.labels.length) name = [...e.labels].map(l => l.textContent).join(' ');
  if (!letters(name) && e.matches('input, textarea')) name = e.getAttribute('placeholder') || e.getAttribute('title') || '';
  if (!letters(name) && e.tagName === 'SELECT') name = e.getAttribute('title') || '';
  if (!letters(name)) name = e.innerText || e.textContent || '';
  if (!letters(name)) name = e.getAttribute('title') || [...e.querySelectorAll('img[alt]')].map(i => i.alt).join(' ');
  const cs = getComputedStyle(e);
  const shows = (cs.outlineStyle !== 'none' && parseFloat(cs.outlineWidth) > 0) || (cs.boxShadow && cs.boxShadow !== 'none') || e.matches('input, textarea, select');
  const where = e.tagName.toLowerCase() + (e.id ? '#' + e.id : '') + (typeof e.className === 'string' && e.className ? '.' + e.className.trim().split(/\\s+/).slice(0, 2).join('.') : '');
  return { tag: e.tagName, where, name: name.replace(/\\s+/g, ' ').trim().slice(0, 80), named: letters(name), shows, dialog: !!e.closest('.modal-backdrop.open'), dialogId: (e.closest('.modal-backdrop.open') || {}).id || '' };
})()`;

export async function keyboardJourneys({ send, ev, sleep, fake }) {
  const problems = [];
  const press = async (k, modifiers = 0) => {
    const d = KEYS[k] || { key: k, code: 'Key' + k.toUpperCase(), windowsVirtualKeyCode: k.toUpperCase().charCodeAt(0) };
    await send('Input.dispatchKeyEvent', { type: 'keyDown', modifiers, ...d, text: modifiers & CTRL ? undefined : d.text });
    await send('Input.dispatchKeyEvent', { type: 'keyUp', modifiers, ...d });
    await sleep(60);
  };
  const type = async s => { await send('Input.insertText', { text: s }); await sleep(120); };
  const focus = () => ev(FOCUS_JS);
  // Tab (or Shift+Tab) until the focused element matches, checking every stop on the way
  const tabTo = async (journey, test, what, max = 60, back = false) => {
    for (let i = 0; i < max; i++) {
      await press('Tab', back ? SHIFT : 0);
      const f = await focus();
      if (process.env.SMOKE_KEYS_TRACE) console.log(`[${journey}] ${i}: ${f.where || f.tag} "${f.name || ''}"`);
      if (f.tag === 'body') continue;
      if (!f.named) problems.push(`[${journey}] the keyboard stops on ${f.where}, which has no name a screen reader can say`);
      if (!f.shows) problems.push(`[${journey}] ${f.where} ("${f.name}") does not show that it has focus`);
      if (await ev(`(() => { const e = document.activeElement; return !!(${test}); })()`)) return f;
    }
    problems.push(`[${journey}] Tab never reached ${what}`);
    return null;
  };
  const sent = () => ev('window.__kbSent.splice(0)');
  const start = async () => {
    await ev(fake);
    await ev(`closeModals(); (typeof closePalette === "function" && closePalette()); document.getElementById('notesPanel')?.classList.remove('open'); setTab('mods'); window.scrollTo(0, 0);
      window.__kbSent = []; if (!window.__kbIpc) { window.__kbIpc = ipcSend; ipcSend = (p, b) => { window.__kbSent.push([p, b || {}]); return Promise.resolve({}); }; }
      if (!window.__kbFetch) { window.__kbFetch = window.fetch; window.fetch = (u, o) => String(u).startsWith('/api/preflight') ? Promise.resolve(new Response(JSON.stringify({ appid: '1', ms: 1, problems: [] }))) : window.__kbFetch(u, o); }
      document.body.tabIndex = -1; document.body.focus(); document.body.removeAttribute('tabindex'); true`);
    await sleep(150);
  };

  // 1. pick a game and play it
  {
    const J = 'play';
    await start();
    await press('f', CTRL);
    if (!(await ev(`document.activeElement.id === 'searchInput'`))) problems.push(`[${J}] Ctrl+F did not go to the game search`);
    await type('Unreal');
    await sleep(200);
    if (await tabTo(J, `e.closest('#gameList') && /Unreal Thing/.test(e.textContent)`, 'the game "Unreal Thing" in the list', 12)) {
      await press('Enter');
      const s = await sent();
      if (!s.some(([p, b]) => p === '/api/select' && b.index === 2)) problems.push(`[${J}] Enter on a game did not choose it (${JSON.stringify(s).slice(0, 120)})`);
    }
    await ev(`document.getElementById('searchInput').value = ''; renderGames(true)`);
    if (await tabTo(J, `e.matches('.btn-play') && !e.matches('.play-caret')`, 'the Play button', 80)) {
      await press('Enter');
      await sleep(250);
      const s = await sent();
      if (!s.some(([p]) => p === '/api/play')) problems.push(`[${J}] Enter on Play did not start the game (${JSON.stringify(s).slice(0, 120)})`);
    }
  }

  // 2. find a mod and install it
  {
    const J = 'install a mod';
    await start();
    await press('k', CTRL);
    if (!(await ev(`document.getElementById('paletteBack').classList.contains('open') && document.activeElement.id === 'paletteInput'`))) problems.push(`[${J}] Ctrl+K did not open the command palette with the cursor in it`);
    await type('Find mods');
    const picked = await ev(`(palItems[palSel] || {}).label || ''`);
    await press('Enter');
    await sleep(1000);
    const f = await focus();
    if (!f.dialog) problems.push(`[${J}] after opening the mod store ("${picked}") the keyboard is not in it (focus on ${f.where || f.tag})`);
    if (await tabTo(J, `e.matches('.s-card')`, 'a mod in the store', 40)) {
      // look at the mod's page, go back to the store, and open it again
      await press('Enter');
      await sleep(1000);
      if (!(await ev(`document.getElementById('modPageModal').classList.contains('open')`))) problems.push(`[${J}] Enter on a mod did not open its page`);
      await press('Escape');
      await sleep(250);
      const back = await focus();
      if (back.dialogId !== 'tsModal' || !/s-card/.test(back.where)) problems.push(`[${J}] Escape on the mod page did not go back to the mod in the store (focus on ${back.where || back.tag})`);
      await press('Enter');
      await sleep(1000);
      if (await tabTo(J, `e.matches('button') && /install|download|get|add/i.test((e.getAttribute('onclick') || '') + ' ' + e.textContent)`, 'an Install button on the mod page', 50)) {
        await press('Enter');
        await sleep(300);
        const s = await sent();
        if (!s.some(([p]) => /install|_get/.test(p))) problems.push(`[${J}] Enter on the Install button did nothing (${JSON.stringify(s).slice(0, 120)})`);
        const after = await focus();
        if (after.tag === 'body' && !(await ev(`!!document.querySelector('.modal-backdrop.open')`))) problems.push(`[${J}] after installing, the keyboard is nowhere (focus on the page itself)`);
      }
    }
    await press('Escape');
    await sleep(250);
    if (await ev(`!!document.querySelector('.modal-backdrop.open')`)) problems.push(`[${J}] Escape did not close the store`);
  }

  // 3. a game crashed: from the notification to a fix
  {
    const J = 'fix a crash';
    await start();
    await ev(`(() => { const g = appState.games[appState.selected_idx]; appState.crash = { appid: g.appid, game: g.name, at_ms: Date.now(), crashed: true, headline: 'Crash detected. Most likely culprit: CoolMod.', suspects: [{ id: 'LUA:CoolMod', name: 'CoolMod', mod_type: 'LUA', score: 12, reasons: ['UE4SS: attempt to call a nil value'] }], evidence: [], artifacts: 1, recent_updates: [], streak: 1, tries: [{ kind: 'culprit_again', id: 'LUA:CoolMod', name: 'CoolMod', library: false }, { kind: 'bisect' }] }; appState.notes.items[1].appid = g.appid; renderNotes(); })()`);
    if (await tabTo(J, `e.id === 'notesBtn'`, 'the notifications button', 40)) {
      await press('Enter');
      await sleep(200);
      if (await tabTo(J, `e.matches('.np-item') && /crash/i.test(e.textContent)`, 'the crash notification', 12)) {
        await press('Enter');
        await sleep(500);
        const f = await focus();
        if (!f.dialog) problems.push(`[${J}] the crash report opened but the keyboard is not in it (focus on ${f.where})`);
        if (await tabTo(J, `e.matches('button') && e.closest('.modal-backdrop.open') && !/close|✕/i.test(e.getAttribute('aria-label') || e.textContent)`, 'something to try in the crash report', 30)) {
          await press('Enter');
          await sleep(300);
          const s = await sent();
          if (!s.length && !(await ev(`document.querySelectorAll('.modal-backdrop.open').length > 0`))) problems.push(`[${J}] Enter on what to try did nothing`);
        }
      }
    }
    await ev(`appState.crash = null; closeModals()`);
  }
  // 4. "my mod does not work": ? opens Help, three answers lead to buttons that act
  {
    const J = 'help';
    await start();
    await press('?');
    await sleep(200);
    if (!(await ev(`document.getElementById('helpModal').classList.contains('open')`))) problems.push(`[${J}] ? did not open Help`);
    for (let i = 0; i < 3; i++) {
      if (!(await tabTo(J, `e.matches('#helpBody button[onclick^="helpPick"]')`, `an answer to question ${i + 1}`, 14))) break;
      await press('Enter');
      await sleep(150);
    }
    const rows = await ev(`document.querySelectorAll('#helpBody .disk-row').length`);
    if (rows < 3) problems.push(`[${J}] three answers gave ${rows} things to try`);
    else if (await tabTo(J, `e.matches('#helpBody .disk-row .btn')`, 'a thing to try', 14)) {
      await press('Enter');
      await sleep(300);
      if (await ev(`document.getElementById('helpModal').classList.contains('open')`)) problems.push(`[${J}] choosing something to try left Help open`);
    }
    await ev(`openHelp()`);
    await press('Escape');
    await sleep(150);
    if (await ev(`document.getElementById('helpModal').classList.contains('open')`)) problems.push(`[${J}] Escape did not close Help`);
  }
  await ev(`if (window.__kbIpc) { ipcSend = window.__kbIpc; window.__kbIpc = null; } if (window.__kbFetch) { window.fetch = window.__kbFetch; window.__kbFetch = null; } closeModals(); true`);
  return [...new Set(problems)];
}
