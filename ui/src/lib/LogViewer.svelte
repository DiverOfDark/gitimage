<script lang="ts">
  import ArrowDownToLine from '@lucide/svelte/icons/arrow-down-to-line';
  import Check from '@lucide/svelte/icons/check';
  import ChevronRight from '@lucide/svelte/icons/chevron-right';
  import LoaderCircle from '@lucide/svelte/icons/loader-circle';
  import X from '@lucide/svelte/icons/x';
  import { tick, untrack } from 'svelte';
  import { parseStep, type BuildEvent, type Step } from './api';
  import { clock, stopwatch } from './time.svelte';

  let {
    namespace,
    name,
    revision,
    wait = true,
  }: { namespace: string; name: string; revision: string; /** false for a build that already ended */ wait?: boolean } = $props();

  /** Keep the page responsive on very chatty steps: only the tail of a step's output is kept. */
  const MAX_CHARS = 400_000;

  type Row = Step & { text: string; cut: boolean };
  let rows = $state<Row[]>([]);
  let before = $state<string[]>([]); // clone output
  let after = $state<string[]>([]); // plain lines of the build container
  let status = $state('connecting');
  let end = $state<{ phase: string; message?: string | null } | null>(null);
  let lost = $state(false);
  let toggled = $state<Record<string, boolean>>({});
  let follow = $state(true);
  let pane: HTMLDivElement | undefined = $state();
  let attempt = $state(0);

  const clean = (s: string) =>
    s
      // eslint-disable-next-line no-control-regex
      .replace(/\x1b\[[0-9;?]*[A-Za-z]/g, '')
      .split('\n')
      .map((l) => l.slice(l.lastIndexOf('\r') + 1))
      .join('\n');

  function apply(ev: BuildEvent) {
    for (const s of ev.steps) {
      const row = rows.find((r) => r.id === s.id);
      if (row) {
        // BuildKit starts one step several times (cache lookup, then the real run): the newest
        // report says whether it runs now, the first start says how long it has taken.
        Object.assign(row, { ...s, started: row.started ?? s.started, completed: s.completed, error: s.error });
        continue;
      }
      // BuildKit reports steps as it reaches them; keep each stage together, in Dockerfile order.
      const { n, of } = parseStep(s.title);
      const fresh = { ...s, text: '', cut: false };
      const stage = (r: Row) => n !== undefined && parseStep(r.title).of === of;
      const later = rows.findIndex((r) => stage(r) && (parseStep(r.title).n ?? 0) > n!);
      const last = rows.reduce((at, r, i) => (stage(r) ? i : at), -1);
      if (later !== -1) rows.splice(later, 0, fresh);
      else if (last !== -1) rows.splice(last + 1, 0, fresh);
      else rows.push(fresh);
    }
    for (const l of ev.logs) {
      const row = rows.find((r) => r.id === l.step);
      if (!row) continue;
      row.text += clean(l.text);
      if (row.text.length > MAX_CHARS) {
        row.text = row.text.slice(row.text.indexOf('\n', row.text.length - MAX_CHARS / 2) + 1);
        row.cut = true;
      }
    }
  }

  $effect(() => {
    const url = `/api/gitimages/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}/logs?revision=${revision}${untrack(() => wait) ? '' : '&wait=false'}`;
    // `wait` is read once: a build that finishes on screen keeps its log.
    void attempt;
    untrack(() => {
      rows = [];
      before = [];
      after = [];
      status = 'connecting';
      end = null;
      lost = false;
      toggled = {};
      follow = true;
    });
    const es = new EventSource(url);
    const on = (type: string, fn: (data: any) => void) =>
      es.addEventListener(type, (e) => {
        fn(JSON.parse((e as MessageEvent).data));
        if (follow) tick().then(() => pane?.scrollTo({ top: pane.scrollHeight }));
      });
    on('status', (d) => (status = d.detail));
    on('text', (d) => (d.source === 'clone' ? before : after).push(d.text));
    on('build', apply);
    on('end', (d) => {
      end = d;
      es.close();
    });
    // EventSource would reconnect and replay the log from the top; stop instead.
    es.onerror = () => {
      es.close();
      if (!end) lost = true;
    };
    return () => es.close();
  });

  const running = (r: Row) => !!r.started && !r.completed && !end;
  /** The first build of an image has no cache to import; BuildKit reports that as an error. */
  const noCache = (r: Row) => !!r.error && r.title.startsWith('importing cache manifest');
  const failed = (r: Row) => !!r.error && !noCache(r);
  const isOpen = (r: Row) => toggled[r.id] ?? (running(r) || failed(r));
  const duration = (r: Row) =>
    r.started ? stopwatch((r.completed ? Date.parse(r.completed) : end ? Date.parse(r.started) : clock()) - Date.parse(r.started)) : '';

  function onscroll() {
    if (!pane) return;
    follow = pane.scrollHeight - pane.scrollTop - pane.clientHeight < 40;
  }
  const live = $derived(!end && !lost);
</script>

<section class="overflow-hidden rounded-lg bg-log text-log-foreground">
  <div class="flex h-11 items-center gap-3 border-b border-log-line px-4 text-sm">
    <h2 class="font-medium">Build log</h2>
    <span class="font-mono text-[0.8125rem] text-log-muted">{revision.slice(0, 7)}</span>
    <span class="ml-auto flex items-center gap-2 text-log-muted">
      {#if live}
        <span class="size-1.5 rounded-full bg-[#f0b24a] live-dot"></span>
        {status || 'Streaming'}
      {:else if end?.phase === 'Succeeded'}
        <span class="text-[#4ccb9a]">Finished</span>
      {:else if end?.phase === 'Failed'}
        <span class="text-[#ff8278]">Failed</span>
      {:else if lost}
        Connection lost
      {/if}
    </span>
    {#if live && !follow}
      <button
        type="button"
        class="flex items-center gap-1.5 rounded-md bg-log-raised px-2.5 py-1 text-log-foreground hover:bg-log-line"
        onclick={() => {
          follow = true;
          pane?.scrollTo({ top: pane.scrollHeight });
        }}
      >
        <ArrowDownToLine class="size-3.5" /> Follow
      </button>
    {/if}
  </div>

  <div bind:this={pane} {onscroll} class="max-h-[70vh] min-h-64 overflow-y-auto font-mono text-[0.8125rem] leading-[1.55]">
    {#if before.length}
      <pre class="border-b border-log-line px-4 py-2.5 whitespace-pre-wrap text-log-muted">{before.join('\n')}</pre>
    {/if}

    {#each rows as r (r.id)}
      {@const s = parseStep(r.title)}
      {@const open = isOpen(r)}
      <div class="border-b border-log-line/60">
        <button
          type="button"
          aria-expanded={open}
          disabled={!r.text && !r.error}
          onclick={() => (toggled[r.id] = !open)}
          class="flex w-full items-center gap-2.5 px-4 py-1.5 text-left enabled:hover:bg-log-raised {r.internal ? 'text-log-muted' : ''}"
        >
          <span class="flex size-4 shrink-0 items-center justify-center">
            {#if failed(r)}<X class="size-4 text-[#ff8278]" />
            {:else if r.completed}<Check class="size-3.5 {r.internal || r.cached ? 'text-log-muted' : 'text-[#4ccb9a]'}" />
            {:else if running(r)}<LoaderCircle class="size-3.5 animate-spin text-[#f0b24a] motion-reduce:animate-none" />
            {:else}<span class="size-1.5 rounded-full bg-log-muted"></span>{/if}
          </span>
          {#if s.n}
            <span class="w-10 shrink-0 text-log-muted">{s.n}/{s.of}</span>
          {/if}
          <span class="min-w-0 flex-1 truncate {failed(r) ? 'text-[#ff8278]' : ''}" title={s.text}>{s.text}</span>
          {#if noCache(r)}<span class="shrink-0 text-log-muted">no cache yet</span>{/if}
          {#if r.cached}<span class="shrink-0 text-log-muted">cached</span>{/if}
          <span class="w-12 shrink-0 text-right text-log-muted">{r.cached ? '' : duration(r)}</span>
          <ChevronRight
            class="size-3.5 shrink-0 text-log-muted transition-transform {open ? 'rotate-90' : ''} {r.text || r.error ? '' : 'invisible'}"
          />
        </button>
        {#if open && (r.text || r.error)}
          <div class="bg-log-raised/60 py-2 pr-4 pl-[2.6rem]">
            {#if r.cut}<p class="mb-1 text-log-muted">Earlier output of this step is not shown.</p>{/if}
            {#if r.text}<pre class="break-words whitespace-pre-wrap">{r.text}</pre>{/if}
            {#if r.error}<pre class="mt-1 break-words whitespace-pre-wrap {failed(r) ? 'text-[#ff8278]' : 'text-log-muted'}">{r.error}</pre>{/if}
          </div>
        {/if}
      </div>
    {/each}

    {#if after.length}
      <pre class="px-4 py-2.5 break-words whitespace-pre-wrap {end?.phase === 'Succeeded' ? 'text-log-muted' : 'text-[#ff8278]'}">{after.join('\n')}</pre>
    {/if}

    {#if end && end.phase !== 'Succeeded' && end.phase !== 'Failed' && end.phase !== 'Gone' && end.phase !== 'Error'}
      <p class="px-4 py-2.5 text-log-muted">The log ended while the pod was {end.phase.toLowerCase()}.</p>
    {:else if end && end.phase !== 'Succeeded' && end.phase !== 'Failed'}
      <div class="px-4 py-10 text-center font-sans text-sm text-log-muted">
        <p class="text-log-foreground">
          {end.phase === 'Gone' ? 'This log is no longer available' : 'The log could not be read'}
        </p>
        {#if end.message}<p class="mx-auto mt-1 max-w-md">{end.message}</p>{/if}
      </div>
    {:else if end?.phase === 'Failed' && end.message}
      <p class="px-4 py-2.5 text-log-muted">{end.message}</p>
    {:else if lost}
      <div class="px-4 py-8 text-center font-sans text-sm text-log-muted">
        <p>The log stream stopped before the build ended.</p>
        <button type="button" class="mt-3 rounded-md bg-log-raised px-3 py-1.5 text-log-foreground hover:bg-log-line" onclick={() => attempt++}>
          Load the log again
        </button>
      </div>
    {:else if live && !rows.length && !before.length}
      <p class="px-4 py-10 text-center font-sans text-sm text-log-muted">{status || 'Waiting for output'}…</p>
    {/if}
  </div>
</section>
