<script lang="ts">
  import Search from '@lucide/svelte/icons/search';
  import { parseStep, type GitImage, type State } from '../lib/api';
  import { detailHref, link } from '../lib/router.svelte';
  import { app } from '../lib/store.svelte';
  import { ago, clock, stopwatch } from '../lib/time.svelte';
  import Track from '../lib/Track.svelte';
  import Sha from '../lib/ui/Sha.svelte';
  import StateBadge from '../lib/ui/StateBadge.svelte';

  let query = $state('');
  let namespace = $state('');
  let only = $state<State | null>(null);

  const items = $derived(app.items ?? []);
  const namespaces = $derived([...new Set(items.map((g) => g.namespace))].sort());
  const scoped = $derived(
    items.filter(
      (g) =>
        (!namespace || g.namespace === namespace) &&
        (!query || `${g.namespace}/${g.name} ${g.url} ${g.ref}`.toLowerCase().includes(query.toLowerCase())),
    ),
  );
  const shown = $derived(only ? scoped.filter((g) => g.state === only) : scoped);
  const order: State[] = ['Building', 'Failed', 'Pending', 'Ready'];
  const counts = $derived(order.map((s) => ({ state: s, n: scoped.filter((g) => g.state === s).length })));

  const repo = (url: string) => url.replace(/^(https?:\/\/|ssh:\/\/)?(git@)?/, '').replace(/\.git$/, '');

  function summary(g: GitImage): string {
    if (g.state === 'Building') return parseStep(g.step).text || 'starting';
    if (g.state === 'Failed') return g.failure?.reason ?? 'Build failed';
    if (g.state === 'Pending') return g.gatedPods ? 'Waiting for the first build' : 'No build yet';
    const last = g.builds.find((b) => b.digest);
    return last ? `${last.outcome === 'Reused' ? 'Reused' : 'Built'} ${ago(last.finishedAt)}` : 'Ready';
  }
</script>

<div class="flex flex-wrap items-end justify-between gap-4">
  <div>
    <h1 class="text-2xl font-semibold tracking-tight">Images</h1>
    <p class="mt-0.5 text-muted-foreground">
      {#if app.items}
        {items.length} {items.length === 1 ? 'GitImage' : 'GitImages'} in {namespaces.length}
        {namespaces.length === 1 ? 'namespace' : 'namespaces'}
      {:else}
        Loading…
      {/if}
    </p>
  </div>
  <div class="flex flex-wrap items-center gap-2">
    <label class="relative">
      <span class="sr-only">Search</span>
      <Search class="pointer-events-none absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" />
      <input
        bind:value={query}
        placeholder="Search name, repo or ref"
        class="h-9 w-60 rounded-md border border-input bg-card pr-3 pl-8 text-sm placeholder:text-muted-foreground"
      />
    </label>
    <label>
      <span class="sr-only">Namespace</span>
      <select bind:value={namespace} class="h-9 rounded-md border border-input bg-card px-2.5 text-sm">
        <option value="">All namespaces</option>
        {#each namespaces as ns (ns)}<option>{ns}</option>{/each}
      </select>
    </label>
  </div>
</div>

<div class="mt-5 flex flex-wrap gap-2" role="group" aria-label="Filter by state">
  {#each counts as c (c.state)}
    <button
      type="button"
      aria-pressed={only === c.state}
      disabled={c.n === 0}
      onclick={() => (only = only === c.state ? null : c.state)}
      class="flex items-baseline gap-2 rounded-md border px-3 py-1.5 text-sm transition-colors disabled:opacity-45
        {only === c.state ? 'border-foreground bg-card' : 'bg-card hover:border-input'}"
    >
      <span class="text-base font-semibold">{c.n}</span>
      <span class="text-muted-foreground">{c.state.toLowerCase()}</span>
    </button>
  {/each}
</div>

<div class="mt-4 overflow-hidden rounded-lg border bg-card">
  {#if app.items && items.length === 0}
    <div class="px-6 py-16 text-center">
      <h2 class="font-semibold">No GitImages you can see</h2>
      <p class="mx-auto mt-1 max-w-md text-muted-foreground">
        Create one with <code class="font-mono text-sm">kubectl apply</code>, or ask for permission to list
        <code class="font-mono text-sm">gitimages.gitimage.dev</code> in your namespaces.
      </p>
    </div>
  {:else if app.items && shown.length === 0}
    <p class="px-6 py-12 text-center text-muted-foreground">Nothing matches these filters.</p>
  {:else}
    <ul class="divide-y">
      {#each shown as g (`${g.namespace}/${g.name}`)}
        <li>
          <a
            href={detailHref(g.namespace, g.name)}
            onclick={link}
            class="grid grid-cols-[1fr_auto] items-center gap-x-6 gap-y-2 px-4 py-3.5 transition-colors hover:bg-muted/60 sm:px-5 lg:grid-cols-[minmax(0,15rem)_minmax(0,16rem)_minmax(0,1fr)_7.5rem]"
          >
            <div class="min-w-0">
              <div class="truncate font-medium">{g.name}</div>
              <div class="truncate text-sm text-muted-foreground">{g.namespace}</div>
            </div>
            <div class="hidden min-w-0 lg:block">
              <div class="flex items-center gap-2">
                <span class="truncate text-sm">{g.ref}</span>
                <Sha sha={g.latestRevision} class="text-muted-foreground" />
              </div>
              <div class="truncate text-sm text-muted-foreground">
                {repo(g.url)}{g.context !== '.' ? ` / ${g.context}` : ''}
              </div>
            </div>
            <div class="col-span-2 row-start-2 min-w-0 lg:col-span-1 lg:row-start-auto">
              <Track gi={g} />
              <div class="mt-1.5 flex items-baseline gap-2 text-sm">
                <span
                  class="truncate {g.state === 'Failed' ? 'text-failed' : g.state === 'Building' ? 'font-mono text-[0.8125rem]' : 'text-muted-foreground'}"
                >
                  {summary(g)}
                </span>
                {#if g.gatedPods}
                  <span class="shrink-0 text-building">
                    {g.gatedPods} {g.gatedPods === 1 ? 'pod' : 'pods'} waiting
                  </span>
                {/if}
              </div>
            </div>
            <div class="justify-self-end text-right">
              {#if g.state === 'Building'}
                {@const s = parseStep(g.step)}
                <StateBadge state={g.state} label={s.n ? `Step ${s.n}/${s.of}` : 'Building'} />
                {#if g.buildingSince}
                  <div class="mt-1 font-mono text-[0.8125rem] text-muted-foreground">
                    {stopwatch(clock() - Date.parse(g.buildingSince))}
                  </div>
                {/if}
              {:else}
                <StateBadge state={g.state} />
              {/if}
            </div>
          </a>
        </li>
      {/each}
    </ul>
  {/if}
</div>
