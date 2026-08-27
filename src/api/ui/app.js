'use strict';

(() => {
  const byId = id => document.querySelector(`#${id}`);
  const sessionsNode = byId('sessions');
  const sessionCount = byId('session-count');
  const emptyState = byId('empty-state');
  const terminalPanel = byId('terminal-panel');
  const terminalNode = byId('terminal');
  const terminalTitle = byId('terminal-title');
  const terminalMeta = byId('terminal-meta');
  const connectionStatus = byId('connection-status');
  const connectionDot = byId('connection-dot');
  const activityStatus = byId('activity-status');
  const closeSession = byId('close-session');
  const secretButton = byId('secret-input');
  const secretOverlay = byId('secret-overlay');
  const secretReason = byId('secret-reason');
  const keyBar = byId('key-bar');
  const keyBarToggle = byId('key-bar-toggle');
  const viewport = window.visualViewport || null;
  const encoder = new TextEncoder();
  const MIN_TERMINAL_DIMENSION = 2;
  const SHORT_VIEWPORT_HEIGHT_PX = 560;
  const CTRL_MESSAGE = 'Ctrl armed for the next key';
  const SIGNAL_LABELS = {
    interrupt: 'Ctrl-C',
    terminate: 'TERM',
    kill: 'KILL',
  };
  const SPECIAL_KEYS = {
    Enter: 'return',
    Backspace: 'backspace',
    Tab: 'tab',
    Escape: 'escape',
    ArrowLeft: 'arrow_left',
    ArrowDown: 'arrow_down',
    ArrowUp: 'arrow_up',
    ArrowRight: 'arrow_right',
  };
  const KEY_BAR_ACTIONS = {
    esc: 'escape',
    tab: 'tab',
    left: 'arrow_left',
    down: 'arrow_down',
    up: 'arrow_up',
    right: 'arrow_right',
  };

  let sessions = [];
  let selectedId = '';
  let initialSessionId =
    new URLSearchParams(window.location.search).get('initial_session_id') || '';
  let sessionRuntime = null;
  let resizeTimer = null;
  let inputTimer = null;
  let pendingInput = [];
  let inputChain = Promise.resolve();
  let screenSeq = 0;
  let screenModes = {application_cursor: false, bracketed_paste: false};
  let ctrlArmed = false;
  let keyBarChosen = false;
  let composing = false;

  const input = document.createElement('textarea');
  input.className = 'terminal-input';
  input.setAttribute('aria-label', 'Terminal input');
  input.setAttribute('autocapitalize', 'none');
  input.setAttribute('autocomplete', 'off');
  input.setAttribute('autocorrect', 'off');
  input.setAttribute('spellcheck', 'false');
  terminalNode.append(input);

  function call(action) {
    return sessionRuntime.call(action);
  }

  /* The connection word is AFUI's own, so the wording is all this page
   * supplies; the dot carries the same word so that it takes the baseline's
   * colour instead of a second palette that could quietly disagree with it. */
  const showConnection = afui.connection(connectionStatus, {
    connecting: 'Connecting',
    live: 'Live',
    reconnecting: 'Reconnecting',
    closed: 'Disconnected',
  });
  const showConnectionDot = afui.connection(connectionDot);

  /* This page's head, and the one it may be inside.
   *
   * A framed page otherwise carries two: the frame's, which already has whose
   * program this is and which terminal the session was opened on, and this
   * page's own underneath saying it again. Being told there is a head out
   * there means dropping this one and sending up only what the frame cannot
   * know — which session is on screen, and how the runtime behind it is
   * going. */
  const pageHead = afui.frame({
    onHead: head => {
      document.body.dataset.frameHead = head ? 'true' : 'false';
    },
  });

  /* The activity line is afterminal's own word on `data-state`, written
   * through the kernel rather than by hand: this was three identical lines
   * sitting two lines below `afui.connection`, and a copy of a primitive is a
   * copy that stops tracking it. */
  const activity = afui.status(activityStatus);

  function setActivity(text = '', state = '') {
    activity.set(text, state);
  }

  function setProcessBusy(busy) {
    terminalPanel.setAttribute('aria-busy', busy ? 'true' : 'false');
    for (const control of document.querySelectorAll(
      '[data-signal], #secret-input, #close-session',
    )) {
      control.disabled = busy;
    }
  }

  function closeProcessMenu(control) {
    const menu = control?.closest('details');
    if (menu) menu.open = false;
  }

  function bytesToBase64(bytes) {
    let binary = '';
    const block = 0x8000;
    for (let index = 0; index < bytes.length; index += block) {
      binary += String.fromCharCode(...bytes.subarray(index, index + block));
    }
    return window.btoa(binary);
  }

  function queueText(text) {
    if (!selectedId || !text) return;
    pendingInput.push(encoder.encode(text));
    if (!inputTimer) inputTimer = window.setTimeout(flushInput, 8);
  }

  function flushInput() {
    inputTimer = null;
    if (!selectedId || !pendingInput.length) return;
    const sessionId = selectedId;
    const size = pendingInput.reduce((total, chunk) => total + chunk.length, 0);
    const bytes = new Uint8Array(size);
    let offset = 0;
    for (const chunk of pendingInput) {
      bytes.set(chunk, offset);
      offset += chunk.length;
    }
    pendingInput = [];
    inputChain = inputChain
      .then(() => call({
        type: 'input',
        session_id: sessionId,
        data_base64: bytesToBase64(bytes),
      }))
      .catch(error => {
        setActivity(`Input failed: ${error.message}`, 'error');
      });
  }

  function sendAction(action, ctrl = false) {
    if (!selectedId) return;
    const sessionId = selectedId;
    // Text is batched behind an 8ms timer and an action is not, so an action
    // chained straight onto inputChain overtakes whatever was typed just
    // before it: Return arriving ahead of its own line ran an empty prompt and
    // left the command to land on the next one, and Backspace arriving ahead
    // of a character deleted what was there before it. Flush first, so the
    // PTY sees them in the order the person produced them.
    flushInput();
    inputChain = inputChain
      .then(() => call({
        type: 'input_action',
        session_id: sessionId,
        action,
        ctrl,
      }))
      .catch(error => {
        setActivity(`Input failed: ${error.message}`, 'error');
      });
  }

  function ctrlByte(text) {
    if ([...text].length !== 1) return null;
    const code = text.codePointAt(0);
    if (code === 0x20 || (code >= 0x40 && code <= 0x5f) ||
        (code >= 0x61 && code <= 0x7a)) {
      return String.fromCharCode(code & 0x1f);
    }
    if (code === 0x3f) return '\x7f';
    return null;
  }

  function setCtrlArmed(armed) {
    ctrlArmed = armed;
    for (const button of document.querySelectorAll('[data-key="ctrl"]')) {
      button.dataset.armed = armed ? 'true' : 'false';
      button.setAttribute('aria-pressed', armed ? 'true' : 'false');
    }
    if (armed) setActivity(CTRL_MESSAGE);
    else if (activityStatus.textContent === CTRL_MESSAGE) setActivity();
  }

  function focusInput() {
    input.focus({preventScroll: true});
  }

  function colorValue(color, fallback) {
    if (!color || color.kind === 'default') return fallback;
    if (color.kind === 'rgb') return `rgb(${color.red} ${color.green} ${color.blue})`;
    const index = color.index;
    const basic = [
      '#11141b', '#ff6b76', '#71e6a4', '#f1cb75',
      '#78a9ff', '#c99bff', '#61dce6', '#d9dee8',
      '#697386', '#ff8d96', '#93f4bc', '#ffe39a',
      '#9fc1ff', '#debaff', '#8aebf1', '#ffffff',
    ];
    if (index < 16) return basic[index];
    if (index < 232) {
      const value = index - 16;
      const channel = part => part === 0 ? 0 : 55 + part * 40;
      return `rgb(${channel(Math.floor(value / 36))} ${channel(Math.floor(value / 6) % 6)} ${channel(value % 6)})`;
    }
    const gray = 8 + (index - 232) * 10;
    return `rgb(${gray} ${gray} ${gray})`;
  }

  function renderScreen(screen) {
    if (!screen || screen.secret_input) {
      terminalNode.querySelectorAll('.terminal-screen').forEach(node => node.remove());
      return;
    }
    if (screen.seq < screenSeq) return;
    screenSeq = screen.seq;
    screenModes = screen.modes || screenModes;
    const grid = document.createElement('div');
    grid.className = 'terminal-screen';
    grid.setAttribute('role', 'textbox');
    grid.setAttribute('aria-readonly', 'true');
    grid.style.setProperty('--terminal-cols', String(screen.cols));
    grid.style.setProperty('--terminal-rows', String(screen.rows));
    for (let rowIndex = 0; rowIndex < screen.cells.length; rowIndex += 1) {
      const row = document.createElement('div');
      row.className = 'terminal-row';
      row.dataset.row = String(rowIndex);
      for (const cell of screen.cells[rowIndex]) {
        const span = document.createElement('span');
        span.className = 'terminal-cell';
        span.textContent = cell.text;
        span.style.color = colorValue(
          cell.inverse ? cell.background : cell.foreground,
          cell.inverse ? '#0b0d12' : '#e8ebf2',
        );
        span.style.backgroundColor = colorValue(
          cell.inverse ? cell.foreground : cell.background,
          cell.inverse ? '#e8ebf2' : '#0b0d12',
        );
        span.style.fontWeight = cell.bold ? '700' : '400';
        span.style.fontStyle = cell.italic ? 'italic' : 'normal';
        span.style.textDecoration = cell.underline ? 'underline' : 'none';
        span.style.opacity = cell.dim ? '.65' : '1';
        span.style.setProperty('--cell-width', String(cell.width));
        if (screen.cursor.visible && screen.cursor.row === rowIndex &&
            Number(row.dataset.columns || 0) <= screen.cursor.col) {
          const columns = Number(row.dataset.columns || 0);
          if (screen.cursor.col < columns + cell.width) span.dataset.cursor = 'true';
        }
        row.dataset.columns =
          String(Number(row.dataset.columns || 0) + Number(cell.width || 1));
        row.append(span);
      }
      grid.append(row);
    }
    const old = terminalNode.querySelector('.terminal-screen');
    if (!old || old.children.length !== grid.children.length) {
      if (old) old.replaceWith(grid);
      else terminalNode.prepend(grid);
    } else {
      // Replace only the rows that actually changed. Swapping the whole screen
      // on every update tore out any selection inside it the instant the next
      // frame arrived — a blinking cursor alone was enough — so output could be
      // read but never copied. A row the update did not touch keeps its nodes,
      // and a selection anchored in one survives.
      const rows = Array.from(grid.children);
      for (let index = 0; index < rows.length; index += 1) {
        const previous = old.children[index];
        if (!previous.isEqualNode(rows[index])) previous.replaceWith(rows[index]);
      }
    }
    positionInputAtCursor();
    terminalMeta.textContent =
      `${selectedId} · ${screen.cols}×${screen.rows}${screen.alt_screen ? ' · alternate screen' : ''}`;
    const cursorCell = grid.querySelector('.terminal-cell[data-cursor="true"]');
    if (cursorCell) {
      terminalNode.scrollTop = Math.max(
        0,
        cursorCell.offsetTop + cursorCell.offsetHeight - terminalNode.clientHeight + 6,
      );
    }
  }

  function renderSecretInput(session) {
    const active = Boolean(session?.secret_input);
    terminalPanel.dataset.secret = active ? 'true' : 'false';
    secretOverlay.hidden = !active;
    secretReason.textContent = active
      ? 'The screen stays blank while you enter the secret.'
      : '';
    secretButton.textContent = active ? 'End private input' : 'Private input';
    secretButton.dataset.active = active ? 'true' : 'false';
    secretButton.setAttribute('aria-pressed', active ? 'true' : 'false');
  }

  function renderSessions() {
    sessionsNode.replaceChildren();
    sessionCount.textContent = String(sessions.length);
    for (const session of sessions) {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'session';
      button.dataset.selected = session.session_id === selectedId ? 'true' : 'false';
      button.setAttribute(
        'aria-pressed',
        session.session_id === selectedId ? 'true' : 'false',
      );
      const label = document.createElement('span');
      label.className = 'session-label';
      label.textContent = session.title || session.session_id;
      const detail = document.createElement('span');
      detail.className = 'session-detail';
      detail.textContent = session.secret_input
        ? `${session.status} · secret input`
        : `${session.status} · ${session.cols}×${session.rows}`;
      button.append(label, detail);
      button.addEventListener('click', () => selectSession(session.session_id));
      sessionsNode.append(button);
    }
  }

  function selectSession(sessionId) {
    if (selectedId === sessionId) {
      focusInput();
      return;
    }
    selectedId = sessionId;
    screenSeq = 0;
    setCtrlArmed(false);
    setActivity();
    const session = sessions.find(item => item.session_id === sessionId);
    renderSessions();
    terminalPanel.hidden = !session;
    emptyState.hidden = Boolean(session);
    if (!session) return;
    terminalTitle.textContent = session.title || session.session_id;
    pageHead.title(terminalTitle.textContent);
    renderSecretInput(session);
    renderScreen(session.screen);
    // Only measuring geometry needs a frame. AFUI connected independently of
    // paint and already delivered this retained screen.
    window.requestAnimationFrame(() => {
      measureAndResize();
      focusInput();
    });
  }

  function applyState(state) {
    if (state?.type === 'error') {
      setActivity(state.message || 'Terminal state is unavailable', 'error');
      return;
    }
    if (state?.type !== 'snapshot') return;
    sessions = Array.isArray(state.sessions) ? state.sessions : [];
    const selected = sessions.find(item => item.session_id === selectedId);
    if (!selected) {
      selectedId = '';
      const initial = sessions.find(item => item.session_id === initialSessionId);
      initialSessionId = '';
      if (initial) selectSession(initial.session_id);
      else if (sessions.length) selectSession(sessions[0].session_id);
      else {
        terminalNode.querySelectorAll('.terminal-screen').forEach(node => node.remove());
        terminalPanel.hidden = true;
        emptyState.hidden = false;
        pageHead.title(null);
      }
    } else {
      terminalTitle.textContent = selected.title || selected.session_id;
      pageHead.title(terminalTitle.textContent);
      renderSecretInput(selected);
      renderScreen(selected.screen);
    }
    renderSessions();
  }


  /* Park the input on the cursor cell.
   *
   * The field is one transparent pixel that never moved, so macOS put the IME
   * candidate window wherever that pixel sat — the terminal's top-left corner —
   * and the half-typed word appeared nowhere at all. A caret is what an IME
   * anchors to, so the field has to be where the cursor is.
   *
   * Called after every render because the cursor moves with the output, and
   * again as a composition updates, since the text being composed changes how
   * wide the field needs to be. */
  function positionInputAtCursor() {
    const cursor = terminalNode.querySelector('.terminal-cell[data-cursor="true"]');
    if (!cursor) return;
    const cell = cursor.getBoundingClientRect();
    const frame = terminalNode.getBoundingClientRect();
    input.style.left = `${cell.left - frame.left + terminalNode.scrollLeft}px`;
    input.style.top = `${cell.top - frame.top + terminalNode.scrollTop}px`;
  }

  /* A composed character is usually wide, and the field has to hold it or the
   * preedit is clipped mid-word. Counted rather than measured: a measurement
   * would need a reflow per keystroke to say what two columns already say. */
  function sizeInputForComposition(text) {
    let columns = 0;
    for (const character of text) {
      columns += /[\u1100-\u115f\u2e80-\ua4cf\ac00-\ud7a3\uf900-\ufaff\ufe30-\ufe6f\uff00-\uff60\uffe0-\uffe6]/
        .test(character) ? 2 : 1;
    }
    input.style.width = `${Math.max(columns + 1, 2)}ch`;
  }

  function submitInputValue() {
    if (!input.value) return;
    let text = input.value;
    input.value = '';
    if (ctrlArmed) {
      setCtrlArmed(false);
      text = ctrlByte(text) ?? text;
    }
    queueText(text);
  }

  input.addEventListener('compositionstart', () => {
    composing = true;
    input.classList.add('is-composing');
    positionInputAtCursor();
    sizeInputForComposition(input.value);
  });
  input.addEventListener('compositionupdate', event => {
    positionInputAtCursor();
    sizeInputForComposition(event.data || input.value);
  });
  input.addEventListener('compositionend', event => {
    composing = false;
    input.classList.remove('is-composing');
    input.style.width = '';
    if (event.data) {
      input.value = '';
      queueText(event.data);
    } else {
      submitInputValue();
    }
  });
  input.addEventListener('beforeinput', event => {
    if (composing || event.isComposing) return;
    if (event.inputType === 'insertFromPaste') {
      event.preventDefault();
      const text = event.dataTransfer?.getData('text/plain') || event.data || '';
      queueText(screenModes.bracketed_paste ? `\x1b[200~${text}\x1b[201~` : text);
    }
  });
  input.addEventListener('input', event => {
    if (composing || event.isComposing) return;
    // The commit already went out from `compositionend`, and this event must
    // not send the same text twice. It is recognised by its own inputType
    // rather than by a flag set at commit time and cleared by whatever came
    // next: Chromium fires no input event after compositionend at all, so that
    // flag survived to eat the following keystroke — which on a Chinese
    // keyboard is the space after a word.
    if (event.inputType === 'insertCompositionText') {
      input.value = '';
      return;
    }
    submitInputValue();
  });
  input.addEventListener('keyup', event => {
    // iOS IMEs commonly report 229 and only update the textarea by keyup.
    if (event.keyCode === 229 && !composing) submitInputValue();
  });
  input.addEventListener('keydown', event => {
    const action = SPECIAL_KEYS[event.key];
    if (action) {
      event.preventDefault();
      const modified = ctrlArmed || event.ctrlKey;
      setCtrlArmed(false);
      sendAction(action, modified);
      return;
    }
    if (event.ctrlKey && !event.metaKey && !event.altKey && event.key.length === 1) {
      const text = ctrlByte(event.key);
      if (text !== null) {
        event.preventDefault();
        queueText(text);
      }
    }
  });
  input.addEventListener('paste', event => {
    event.preventDefault();
    const text = event.clipboardData?.getData('text/plain') || '';
    queueText(screenModes.bracketed_paste ? `\x1b[200~${text}\x1b[201~` : text);
  });
  terminalNode.addEventListener('click', () => {
    // A terminal cell is the most likely place a phone user taps, and focusing
    // the off-screen input is what opens the soft keyboard.
    //
    // It is also what threw away a selection the moment the mouse came up.
    // Moving focus to the textarea collapses the document selection, so a drag
    // across output selected it and the click ending that drag cleared it —
    // output could be read and never copied. This used to claim the click
    // arrived too late to matter; it does not.
    //
    // A tap has nothing selected, so the two gestures separate cleanly.
    const selection = document.getSelection();
    if (
      selection &&
      !selection.isCollapsed &&
      selection.anchorNode &&
      terminalNode.contains(selection.anchorNode)
    ) {
      return;
    }
    focusInput();
  });

  function pressKey(name) {
    if (name === 'ctrl') {
      setCtrlArmed(!ctrlArmed);
      focusInput();
      return;
    }
    const action = KEY_BAR_ACTIONS[name];
    if (action) {
      const modified = ctrlArmed;
      setCtrlArmed(false);
      sendAction(action, modified);
    }
    focusInput();
  }

  function showKeyBar(show) {
    keyBar.hidden = !show;
    keyBarToggle.setAttribute('aria-expanded', show ? 'true' : 'false');
    keyBarToggle.dataset.active = show ? 'true' : 'false';
    keyBarToggle.textContent = show ? 'Hide keys' : 'Show keys';
  }

  function applyViewport() {
    const height = viewport ? viewport.height : window.innerHeight;
    document.documentElement.style.setProperty('--viewport-height', `${height}px`);
    document.documentElement.dataset.shortViewport =
      height < SHORT_VIEWPORT_HEIGHT_PX ? 'true' : 'false';
    measureAndResize();
  }

  function measureAndResize() {
    if (!selectedId) return;
    if (resizeTimer) window.clearTimeout(resizeTimer);
    const sessionId = selectedId;
    resizeTimer = window.setTimeout(() => {
      const probe = document.createElement('span');
      probe.className = 'terminal-measure';
      probe.textContent = 'M';
      terminalNode.append(probe);
      const rect = probe.getBoundingClientRect();
      probe.remove();
      if (!rect.width || !rect.height) return;
      const cols = Math.floor(terminalNode.clientWidth / rect.width);
      const rows = Math.floor(terminalNode.clientHeight / rect.height);
      if (cols < MIN_TERMINAL_DIMENSION || rows < MIN_TERMINAL_DIMENSION) return;
      call({type: 'resize', session_id: sessionId, rows, cols}).catch(error => {
        setActivity(`Resize failed: ${error.message}`, 'error');
      });
    }, 80);
  }

  async function toggleSecretInput() {
    if (!selectedId) return;
    const active = terminalPanel.dataset.secret === 'true';
    setProcessBusy(true);
    setActivity(active ? 'Ending private input…' : 'Starting private input…', 'pending');
    try {
      await call({
        type: 'secret_input',
        session_id: selectedId,
        action: active ? 'end' : 'start',
      });
      setActivity(active ? 'Private input ended' : 'Private input is on');
    } catch (error) {
      setActivity(`Private input failed: ${error.message}`, 'error');
    } finally {
      setProcessBusy(false);
    }
  }

  async function sendSignal(signal, control) {
    if (!selectedId) return;
    if (signal === 'kill' && !window.confirm(`Force-kill ${selectedId}?`)) return;
    closeProcessMenu(control);
    const label = SIGNAL_LABELS[signal] || signal;
    setProcessBusy(true);
    setActivity(`Sending ${label}…`, 'pending');
    try {
      await call({type: 'signal', session_id: selectedId, signal});
      setActivity(`${label} sent`);
    } catch (error) {
      setActivity(`${label} failed: ${error.message}`, 'error');
    } finally {
      setProcessBusy(false);
    }
  }

  async function closeSelectedSession() {
    if (!selectedId || !window.confirm(`Close terminal session ${selectedId}?`)) return;
    const sessionId = selectedId;
    closeProcessMenu(closeSession);
    setProcessBusy(true);
    setActivity('Closing session…', 'pending');
    try {
      await call({type: 'close', session_id: sessionId});
    } catch (error) {
      setActivity(`Close failed: ${error.message}`, 'error');
    } finally {
      setProcessBusy(false);
    }
  }

  for (const button of document.querySelectorAll('[data-signal]')) {
    button.addEventListener('click', () => sendSignal(button.dataset.signal, button));
  }
  for (const button of document.querySelectorAll('[data-key]')) {
    button.addEventListener('mousedown', event => event.preventDefault());
    button.addEventListener('click', () => pressKey(button.dataset.key));
  }
  closeSession.addEventListener('click', closeSelectedSession);
  secretButton.addEventListener('click', toggleSecretInput);
  keyBarToggle.addEventListener('mousedown', event => event.preventDefault());
  keyBarToggle.addEventListener('click', () => {
    keyBarChosen = true;
    showKeyBar(keyBar.hidden);
    focusInput();
  });

  const compact = window.matchMedia('(pointer: coarse), (max-width: 760px)');
  showKeyBar(compact.matches);
  compact.addEventListener('change', event => {
    if (!keyBarChosen) showKeyBar(event.matches);
  });
  window.addEventListener('resize', applyViewport);
  if (viewport) {
    viewport.addEventListener('resize', applyViewport);
    viewport.addEventListener('scroll', applyViewport);
  }
  applyViewport();
  sessionRuntime = afui.connect({
    onState: applyState,
    onConnectionState: state => {
      showConnection(state);
      showConnectionDot(state);
      // The same word, in the head this page may be inside. AFUI's own
      // vocabulary goes with it, so a frame dresses it exactly as the
      // baseline dresses the line above.
      pageHead.status(connectionStatus.textContent, state);
    },
  });
})();
