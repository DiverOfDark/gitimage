import assert from 'node:assert/strict';
import { test } from 'node:test';
import { count, emptyLines, feed, render } from './terminal.ts';

const lines = (...chunks: string[]) => {
  const l = emptyLines();
  for (const c of chunks) feed(l, c);
  return l;
};

test('CRLF ends a line instead of erasing it', () => {
  // apt and dpkg write "\r\n"; the old viewer kept only what followed the last "\r": nothing.
  const l = lines('Reading package lists... Done\r\nBuilding dependency tree\r\n');
  assert.deepEqual(l.done, ['Reading package lists... Done', 'Building dependency tree']);
  assert.equal(l.current, '');
});

test('a lone CR starts the line over', () => {
  const l = lines('(Reading database ... 5%\r(Reading database ... 100%\r(Reading database ... 26066 files)\n');
  assert.deepEqual(l.done, ['(Reading database ... 26066 files)']);
});

test('a CRLF split across chunks is one line ending', () => {
  assert.deepEqual(lines('Unpacking libssl3t64 ...\r', '\nSetting up libssl3t64\r\n').done, [
    'Unpacking libssl3t64 ...',
    'Setting up libssl3t64',
  ]);
  // ...and a CR at a chunk end followed by text is a restart.
  assert.deepEqual(lines('10%\r', '20%\r', 'done\n').done, ['done']);
});

test('a line may span chunks; the unfinished line is shown', () => {
  const l = lines('Compiling ser', 'de v1.0\nCompiling tok');
  assert.deepEqual(l.done, ['Compiling serde v1.0']);
  assert.equal(render(l), 'Compiling serde v1.0\nCompiling tok');
  assert.equal(count(l), 2);
});

test('colour codes are removed', () => {
  assert.deepEqual(lines('\x1b[1m\x1b[32m   Compiling\x1b[0m phos\n').done, ['   Compiling phos']);
});

test('render keeps the tail', () => {
  const l = lines('a\nb\nc\nd\n');
  assert.equal(render(l, 2), 'c\nd');
  assert.equal(render(l), 'a\nb\nc\nd');
});

test('the oldest lines are dropped beyond the cap', () => {
  const l = emptyLines();
  for (let i = 0; i < 1_500; i++) feed(l, `line ${i}\n`, 200);
  assert.ok(l.done.length <= 1_200);
  assert.equal(l.done.at(-1), 'line 1499');
  assert.equal(l.dropped + l.done.length, 1_500);
});
