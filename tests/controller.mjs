// What someone does on a Steam Deck without touching the screen: pick a game and play it, search with the
// on-screen keyboard, and make a game moddable. A fake gamepad is plugged into the page and its buttons are pressed
// one at a time, the way a thumb would; the page's own gamepad loop (requestAnimationFrame) reads them.
// tests/ui_smoke.mjs runs this; it returns a list of problems (empty = fine).
import { FOCUS_JS } from './keyboard.mjs';

export async function controllerJourneys({ ev, sleep, fake }) {
  const problems = [];
  const focus = () => ev(FOCUS_JS);
  const tap = async name => { await ev(`window.__padTap(${JSON.stringify(name)})`); };
  const sent = () => ev('window.__kbSent.splice(0)');
  const check = (J, f, what) => {
    if (f.tag === 'body') return;
    if (!f.named) problems.push(`[${J}] the gamepad stops on ${f.where}, which has no name a screen reader can say`);
    if (!f.shows) problems.push(`[${J}] ${f.where} ("${f.name}") does not show that it has focus`);
  };
  // press the direction that leads to the thing, like a thumb would, until it has focus
  const seek = async (J, sel, what, max = 50) => {
    const has = `(() => { const e = document.activeElement; return !!(e && e.matches && (${JSON.stringify(sel)} === '' || e.matches(${JSON.stringify(sel)}))); })()`;
    for (let i = 0; i < max; i++) {
      const dir = await ev(`(() => {
        const want = [...document.querySelectorAll(${JSON.stringify(sel)})].filter(e => { const r = e.getBoundingClientRect(); return r.width > 4 && r.height > 4; })[0];
        const cur = document.activeElement;
        if (!want) return 'none';
        if (cur === want) return 'here';
        if (!cur || cur === document.body) return 'a';
        const a = cur.getBoundingClientRect(), b = want.getBoundingClientRect();
        const dx = (b.left + b.width / 2) - (a.left + a.width / 2), dy = (b.top + b.height / 2) - (a.top + a.height / 2);
        // the other way only when the last press did not move the focus
        window.__tries = window.__lastCur === cur ? (window.__tries || 0) + 1 : 0;
        window.__lastCur = cur;
        const horizontal = Math.abs(dx) > Math.abs(dy);
        const first = horizontal ? (dx > 0 ? 'right' : 'left') : (dy > 0 ? 'down' : 'up');
        const second = horizontal ? (dy > 0 ? 'down' : 'up') : (dx > 0 ? 'right' : 'left');
        return window.__tries % 2 === 1 ? second : first;
      })()`);
      if (dir === 'here') { check(J, await focus()); return true; }
      if (dir === 'none') { problems.push(`[${J}] ${what} is not on the screen`); return false; }
      await tap(dir === 'a' ? 'down' : dir);
      if (process.env.SMOKE_PAD_TRACE) console.log(`[${J}] ${dir} -> ${(await focus()).where || 'body'} ${await ev(`(() => { const r = e => { const b = e.getBoundingClientRect(); return Math.round(b.left) + ',' + Math.round(b.top) + ' ' + Math.round(b.width) + 'x' + Math.round(b.height); }; const w = document.querySelector(${JSON.stringify(sel)}); return 'at ' + r(document.activeElement) + ' want ' + (w ? r(w) + ' vis=' + getComputedStyle(w).visibility + ' in ' + (w.closest('.pane') || {}).id : 'none') + ' win ' + innerWidth + 'x' + innerHeight; })()`)}`);
      check(J, await focus());
    }
    problems.push(`[${J}] the gamepad never reached ${what}`);
    return false;
  };
  const start = async () => {
    await ev(fake);
    await ev(`closeModals(); (typeof closePalette === "function" && closePalette()); (typeof oskClose === "function" && oskClose(false)); document.getElementById('notesPanel')?.classList.remove('open'); setTab('mods'); window.scrollTo(0, 0);
      window.__kbSent = []; if (!window.__kbIpc) { window.__kbIpc = ipcSend; ipcSend = (p, b) => { window.__kbSent.push([p, b || {}]); return Promise.resolve({}); }; }
      if (!window.__kbFetch) { window.__kbFetch = window.fetch; window.fetch = (u, o) => String(u).startsWith('/api/preflight') ? Promise.resolve(new Response(JSON.stringify({ appid: '1', ms: 1, problems: [] }))) : window.__kbFetch(u, o); }
      if (!window.__pad) {
        window.__pad = { id: 'Fake pad', index: 0, connected: true, mapping: 'standard', buttons: Array.from({ length: 17 }, () => ({ pressed: false, value: 0 })), axes: [0, 0, 0, 0] };
        navigator.getGamepads = () => [window.__pad];
        const idx = { a: 0, b: 1, x: 2, y: 3, lb: 4, rb: 5, start: 9, up: 12, down: 13, left: 14, right: 15 };
        const frames = n => new Promise(r => { let c = 0; const step = () => (++c >= n ? r() : requestAnimationFrame(step)); requestAnimationFrame(step); setTimeout(r, 700); }); window.__padTap = async (name) => { const b = window.__pad.buttons[idx[name]]; b.pressed = true; await frames(3); await new Promise(r => setTimeout(r, 90)); b.pressed = false; await frames(3); await new Promise(r => setTimeout(r, 40)); };
      }
      deckSet(false); deckSet(true); document.activeElement && document.activeElement.blur(); true`);
    await sleep(200);
  };

  // 1. pick a game and play it
  {
    const J = 'play';
    await start();
    if (await seek(J, '#gameList .game-item:not(.selected)', 'a game in the list')) {
      const name = (await focus()).name;
      await tap('a');
      const s = await sent();
      if (!s.some(([p]) => p === '/api/select')) problems.push(`[${J}] A on the game "${name}" did not choose it (${JSON.stringify(s).slice(0, 120)})`);
    }
    await sleep(300);
    if (await seek(J, '.btn-play:not(.play-caret)', 'the Play button')) {
      await tap('a');
      await sleep(250);
      const s = await sent();
      if (!s.some(([p]) => p === '/api/play')) problems.push(`[${J}] A on Play did not start the game (${JSON.stringify(s).slice(0, 120)})`);
    }
    await tap('x');
    await sleep(250);
    if (!(await sent()).some(([p]) => p === '/api/play')) problems.push(`[${J}] X did not start the game`);
  }

  // 2. search with the on-screen keyboard
  {
    const J = 'search';
    await start();
    await tap('y');
    await sleep(250);
    const opened = await ev(`({ palette: document.getElementById('paletteBack').classList.contains('open'), osk: document.getElementById('osk').classList.contains('open'), inKeys: !!document.activeElement.closest('#osk') })`);
    if (!opened || !opened.palette) problems.push(`[${J}] Y did not open the search`);
    else if (!opened.osk || !opened.inKeys) problems.push(`[${J}] the search opened without the on-screen keyboard under the gamepad (${JSON.stringify(opened)})`);
    else {
      const val = () => ev(`document.getElementById('paletteInput').value`);
      await tap('a');
      if ((await val()) !== 'q') problems.push(`[${J}] A on the first key did not type it (got "${await val()}")`);
      await tap('right');
      const f = await focus();
      if (f.name !== 'w') problems.push(`[${J}] right did not move to the next key (on "${f.name}")`);
      check(J, f);
      await tap('a');
      if ((await val()) !== 'qw') problems.push(`[${J}] typing a second letter gave "${await val()}"`);
      await tap('lb');
      await tap('a');
      if (!/^qw[A-Z]$/.test(await val())) problems.push(`[${J}] LB then A did not type a capital (got "${await val()}")`);
      await tap('x');
      await tap('y');
      if ((await val()) !== 'qw ') problems.push(`[${J}] X then Y should delete and add a space (got "${JSON.stringify(await val())}")`);
      await tap('x');
      await tap('x');
      await tap('x');
      if ((await val()) !== '') problems.push(`[${J}] X did not delete (got "${await val()}")`);
      // the keys all have names and the keyboard can be left with B
      for (const dir of ['down', 'down', 'right', 'up']) { await tap(dir); check(J, await focus()); }
      await tap('b');
      const afterB = await ev(`({ osk: document.getElementById('osk').classList.contains('open'), palette: document.getElementById('paletteBack').classList.contains('open'), onInput: document.activeElement.id === 'paletteInput' })`);
      if (afterB.osk || !afterB.palette || !afterB.onInput) problems.push(`[${J}] B should close only the keyboard and go back to the search box (${JSON.stringify(afterB)})`);
      await tap('b');
      if (await ev(`document.getElementById('paletteBack').classList.contains('open')`)) problems.push(`[${J}] a second B did not close the search`);
    }
    // the game search box: A opens the keyboard on it, the menu button finishes
    await start();
    if (await seek(J, '#searchInput', 'the game search box')) {
      await tap('a');
      if (!(await ev(`document.getElementById('osk').classList.contains('open')`))) problems.push(`[${J}] A on a text box did not open the on-screen keyboard`);
      await tap('a');
      if ((await ev(`document.getElementById('searchInput').value`)) !== 'q') problems.push(`[${J}] the key was not typed into the game search`);
      await tap('start');
      if (await ev(`document.getElementById('osk').classList.contains('open')`)) problems.push(`[${J}] the menu button did not finish typing`);
      await ev(`document.getElementById('searchInput').value = ''; renderGames(true)`);
    }
  }

  // 3. make a game moddable
  {
    const J = 'make moddable';
    await start();
    await tap('rb');
    await sleep(200);
    if (await ev(`document.querySelector('.tab.active')?.dataset.tab`) !== 'tools') problems.push(`[${J}] RB did not go to the Tools tab`);
    if (await seek(J, '.tool-hero', 'the Make this game moddable button')) {
      await tap('a');
      await sleep(900);
      let f = await focus();
      if (!f.dialog) { await tap('down'); f = await focus(); }
      if (!f.dialog) problems.push(`[${J}] the setup window opened but the gamepad is not in it (focus on ${f.where || f.tag})`);
      if (await ev(`!document.getElementById('setupModal').classList.contains('open')`)) problems.push(`[${J}] A did not open the setup window`);
      else if (await seek(J, '#setupBody .wl-foot .btn', 'a button in the setup window')) {
        await tap('a');
        await sleep(500);
        const s = await sent();
        const closed = await ev(`!document.getElementById('setupModal').classList.contains('open')`);
        if (!s.some(([p]) => p === '/api/full_setup') && !closed) problems.push(`[${J}] A on the setup button did nothing (${JSON.stringify(s).slice(0, 120)})`);
      }
      await tap('b');
      await sleep(250);
      if (await ev(`document.getElementById('setupModal').classList.contains('open')`)) problems.push(`[${J}] B did not close the setup window`);
      const back = await focus();
      if (back.tag === 'body') problems.push(`[${J}] after closing, the gamepad is nowhere`);
    }
  }
  await ev(`deckSet(false); true`);
  return problems;
}
