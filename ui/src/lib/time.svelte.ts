// One shared clock, so every "3 min ago" on the page ticks together.
let now = $state(Date.now());
if (typeof window !== 'undefined') setInterval(() => (now = Date.now()), 1000);

export function clock() {
  return now;
}

/** `42 s`, `3 min`, `2 h`, `5 d` */
export function span(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s} s`;
  if (s < 3600) return `${Math.floor(s / 60)} min`;
  if (s < 86400) return `${Math.floor(s / 3600)} h`;
  return `${Math.floor(s / 86400)} d`;
}

/** `1:05` for a running or finished duration. */
export function stopwatch(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  const m = Math.floor(s / 60);
  return m >= 60
    ? `${Math.floor(m / 60)}:${String(m % 60).padStart(2, '0')}:${String(s % 60).padStart(2, '0')}`
    : `${m}:${String(s % 60).padStart(2, '0')}`;
}

export function ago(iso?: string): string {
  if (!iso) return '';
  return `${span(now - Date.parse(iso))} ago`;
}
