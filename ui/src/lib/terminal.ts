/**
 * Turns a step's output chunks into lines the way a terminal shows them: `\r\n` and `\n` end a
 * line, a lone `\r` starts the current line over (progress bars). A `\r` at the end of a chunk
 * waits for the next one, since the `\n` of a `\r\n` may arrive in the next chunk.
 */
export interface Lines {
  /** Finished lines, without their line endings. */
  done: string[];
  /** The line being written. */
  current: string;
  /** The last chunk ended in `\r`. */
  cr: boolean;
  /** Finished lines dropped from the front to stay under `max`. */
  dropped: number;
}

export const emptyLines = (): Lines => ({ done: [], current: '', cr: false, dropped: 0 });

// eslint-disable-next-line no-control-regex
const ANSI = /\x1b\[[0-9;?]*[A-Za-z]/g;

export function feed(l: Lines, chunk: string, max = 20_000): void {
  const s = chunk.replace(ANSI, '');
  if (!s) return;
  let start = 0;
  if (l.cr) {
    l.cr = false;
    if (s.charCodeAt(0) === 10) {
      commit(l);
      start = 1;
    } else {
      l.current = '';
    }
  }
  for (let i = start; i < s.length; i++) {
    const c = s.charCodeAt(i);
    if (c === 10) {
      l.current += s.slice(start, i);
      commit(l);
      start = i + 1;
    } else if (c === 13) {
      l.current += s.slice(start, i);
      if (i + 1 === s.length) {
        l.cr = true;
      } else if (s.charCodeAt(i + 1) === 10) {
        commit(l);
        i++;
      } else {
        l.current = '';
      }
      start = i + 1;
    }
  }
  if (start < s.length) l.current += s.slice(start);
  // Trim in batches: shifting one line per push would copy the array every time.
  if (l.done.length > max + 1_000) {
    const n = l.done.length - max;
    l.done.splice(0, n);
    l.dropped += n;
  }
}

function commit(l: Lines) {
  l.done.push(l.current);
  l.current = '';
}

/** All lines, the one being written included when it has text. */
export const count = (l: Lines) => l.done.length + (l.current ? 1 : 0);

/** The last `tail` lines (all with `tail` undefined), joined for a `<pre>`. */
export function render(l: Lines, tail?: number): string {
  const all = l.current ? [...l.done, l.current] : l.done;
  return (tail === undefined ? all : all.slice(-tail)).join('\n');
}
