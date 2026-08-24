const state = { screen: 'home', daemon: false, hotkey: false, theme: 'dark' };
const screens = [...document.querySelectorAll('[data-screen]')];
const toast = document.querySelector('#toast');

function notify(message) {
  toast.textContent = message;
  toast.hidden = false;
  clearTimeout(notify.timer);
  notify.timer = setTimeout(() => { toast.hidden = true; }, 2600);
}

function showScreen(name) {
  state.screen = name;
  screens.forEach(screen => { screen.hidden = screen.dataset.screen !== name; });
  document.querySelectorAll('.tab').forEach(tab => {
    if (tab.dataset.screenTarget === name) tab.setAttribute('aria-current', 'page');
    else tab.removeAttribute('aria-current');
  });
  const heading = document.querySelector(`[data-screen="${name}"] h1`);
  if (heading) requestAnimationFrame(() => heading.focus({ preventScroll: true }));
  window.scrollTo(0, 0);
}

function refreshReadiness() {
  const ready = 3 + Number(state.daemon) + Number(state.hotkey);
  document.querySelector('#panel-progress').textContent = `${ready} of 5 ready`;
  document.querySelector('#card-progress').textContent = `${ready} of 5 ready · ${ready === 5 ? 'Ready to verify' : 'Finish setup to preview from anywhere'}`;
  document.querySelector('#verdict-title').textContent = ready === 5 ? 'Ready for a live test' : `${5 - ready} ${5 - ready === 1 ? 'step' : 'steps'} left`;
  document.querySelector('#verdict-copy').textContent = ready === 5 ? 'The checks pass. Verify the whole chain with a real file selection.' : 'Resolve the issues below; you can leave and come back at any time.';
  document.querySelector('#try-now').hidden = ready !== 5;
}

document.addEventListener('click', event => {
  const target = event.target.closest('button');
  if (!target) return;
  if (target.dataset.screenTarget) return showScreen(target.dataset.screenTarget);
  const action = target.dataset.action;
  if (action === 'open-file') notify('Native Open dialog would open.');
  if (action === 'preview') notify('File opened in preview. This window is now in App mode.');
  if (action === 'start-daemon') {
    state.daemon = true;
    document.querySelector('#daemon-check').remove();
    refreshReadiness();
    notify('Background service started.');
  }
  if (action === 'register-hotkey') {
    state.hotkey = true;
    target.closest('.check').remove();
    refreshReadiness();
    notify('Ctrl+Alt+Space registered for this session.');
  }
  if (action === 'toggle-passes') {
    const passed = document.querySelector('#passed');
    passed.hidden = !passed.hidden;
    target.setAttribute('aria-expanded', String(!passed.hidden));
    target.lastElementChild.textContent = passed.hidden ? '+' : '−';
  }
  if (action === 'toggle-autostart') {
    const next = target.getAttribute('aria-checked') !== 'true';
    target.setAttribute('aria-checked', String(next));
    notify(next ? 'sekio will start at login.' : 'Start at login turned off.');
  }
  if (action === 'coverage') notify('Linux selection coverage guide would open.');
  if (action === 'details') notify('Technical report is available in the complete source example.');
  if (action === 'try-now') showScreen('verification');
  if (action === 'verify') {
    document.querySelector('#verification-content').innerHTML = '<div class="verification__signal">✓</div><h2 tabindex="-1">Preview resolved</h2><p>The full path responded and this result will remain available.</p><dl class="result"><dt>Path</dt><dd>/home/mira/Documents/roadmap.md</dd><dt>From</dt><dd>Nautilus clipboard</dd><dt>Resolved in</dt><dd>8 ms</dd></dl><button class="ds-button ds-button--primary" data-action="finish-verification">Done</button>';
    document.querySelector('#verification-content h2').focus();
  }
  if (action === 'finish-verification') {
    document.querySelector('#verified-record').hidden = false;
    document.querySelector('#readiness-card').hidden = true;
    showScreen('readiness');
    notify('Hotkey preview verified.');
  }
});

document.addEventListener('click', event => {
  const place = event.target.closest('[data-place]');
  if (!place) return;
  document.querySelectorAll('[data-place]').forEach(item => item.removeAttribute('aria-current'));
  place.setAttribute('aria-current', 'page');
  document.querySelector('#place-title').textContent = place.dataset.place;
});

document.querySelector('#theme-toggle').addEventListener('click', event => {
  state.theme = state.theme === 'dark' ? 'light' : 'dark';
  document.documentElement.dataset.theme = state.theme;
  event.currentTarget.setAttribute('aria-label', `Switch to ${state.theme === 'dark' ? 'light' : 'dark'} theme`);
});

document.addEventListener('keydown', event => {
  if (event.key === 'Escape' && state.screen !== 'home') showScreen('home');
  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'b') { event.preventDefault(); showScreen('browser'); }
  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'o') { event.preventDefault(); notify('Native Open dialog would open.'); }
});

refreshReadiness();
