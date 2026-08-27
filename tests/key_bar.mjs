#!/usr/bin/env node
// Static contract checks for the stock renderer's mobile input bridge.
//
// Byte encoding for semantic actions is tested beside the Rust route. This
// file keeps the browser half honest without pretending Node's tiny DOM is
// Safari's composition engine.

import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {fileURLToPath} from 'node:url';
import {dirname, join} from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const app = readFileSync(join(here, '../src/api/ui/app.js'), 'utf8');
const page = readFileSync(join(here, '../src/api/ui/page.html.j2'), 'utf8');

for (const key of ['ctrl', 'esc', 'tab', 'left', 'down', 'up', 'right']) {
  assert.match(page, /data-key="\{\{ key\.name \}\}"/);
  if (key === 'ctrl') assert.match(app, /name === 'ctrl'/);
  else assert.match(app, new RegExp(`\\b${key}: '[a-z_]+'`));
}

for (const action of [
  'return',
  'backspace',
  'tab',
  'escape',
  'arrow_left',
  'arrow_down',
  'arrow_up',
  'arrow_right',
]) {
  assert.ok(app.includes(`'${action}'`), `missing semantic action ${action}`);
}

assert.match(app, /addEventListener\('beforeinput'/);
assert.match(app, /addEventListener\('input'/);
assert.match(app, /addEventListener\('compositionstart'/);
assert.match(app, /addEventListener\('compositionend'/);
assert.match(app, /event\.keyCode === 229/);
assert.match(app, /insertFromPaste/);
assert.match(app, /bracketed_paste/);
assert.match(app, /MIN_TERMINAL_DIMENSION/);
assert.match(app, /rows < MIN_TERMINAL_DIMENSION/);
assert.match(app, /cols < MIN_TERMINAL_DIMENSION/);
assert.match(app, /setProperty\('--viewport-height'/);
assert.match(app, /cursorCell\.offsetTop/);
assert.ok(!app.includes('window.Terminal'), 'stock UI still constructs xterm');
assert.ok(!app.includes('base64ToBytes'), 'stock UI still consumes raw PTY bytes');

process.stdout.write('terminal input bridge: 12 checks passed\n');
