<script lang="ts">
  import ArrowLeft from '@lucide/svelte/icons/arrow-left';
  import Copy from '@lucide/svelte/icons/copy';
  import { ApiError, get, parseStep, short, type BuildRecord, type Detail } from '../lib/api';
  import LogViewer from '../lib/LogViewer.svelte';
  import { detailHref, link } from '../lib/router.svelte';
  import { app } from '../lib/store.svelte';
  import { ago, clock, span, stopwatch } from '../lib/time.svelte';
  import Track from '../lib/Track.svelte';
  import Sha from '../lib/ui/Sha.svelte';
  import StateBadge from '../lib/ui/StateBadge.svelte';

  let { namespace, name, build }: { namespace: string; name: string; build: string | null } = $props();

  let detail = $state<Detail | null>(null);
  let error = $state<string | null>(null);
  let copied = $state<string | null>(null);

  async function load() {
    try {
      detail = await get<Detail>(`/api/gitimages/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}`);
      error = null;
    } catch (e) {
      if (!(e instanceof ApiError && e.status === 401)) error = (e as Error).message;
    }
  }

  // Reload when the live list reports a change of this GitImage, and every 10 s for
  // workloads and pods, which the list does not carry.
  const fromList = $derived(JSON.stringify(app.items?.find((g) => g.namespace === namespace && g.name === name) ?? null));
  $effect(() => {
    void fromList;
    load();
  });
  $effect(() => {
    const t = setInterval(load, 10_000);
    return () => clearInterval(t);
  });

  const gi = $derived(detail?.gitimage);
  const inProgress = $derived(gi?.state === 'Building' ? (gi.buildingRevision ?? gi.latestRevision) : undefined);
  /** The build whose log is shown: the one in the URL, else the running one, else the newest. */
  const selected = $derived(build ?? inProgress ?? gi?.builds[0]?.revision ?? gi?.latestRevision ?? null);
  // A rebuild of the commit on screen starts a new build with the same sha: the log viewer
  // must start over then, but not when that build merely finishes.
  let startedAt = '';
  const attempt = $derived.by(() => {
    if (inProgress && inProgress === selected && gi?.buildingSince) startedAt = gi.buildingSince;
    return startedAt;
  });
  const step = $derived(parseStep(gi?.step));
  const upToDate = $derived(detail?.workloads.filter((w) => w.upToDate).length ?? 0);
  const rolled = $derived(
    !detail || detail.workloads.length === 0
      ? 'No workloads'
      : `${upToDate} of ${detail.workloads.length} ${detail.workloads.length === 1 ? 'workload' : 'workloads'} current`,
  );

  const outcome: Record<BuildRecord['outcome'], string> = {
    Built: 'Built',
    Reused: 'Reused an image',
    Superseded: 'Cancelled by a newer commit',
    Failed: 'Failed',
  };

  async function copy(text: string) {
    await navigator.clipboard.writeText(text);
    copied = text;
    setTimeout(() => (copied = null), 1500);
  }
</script>

<a href="/" onclick={link} class="inline-flex items-center gap-1.5 text-sm text-muted-foreground hover:text-foreground">
  <ArrowLeft class="size-4" /> All images
</a>

{#if error && !detail}
  <div class="mt-6 rounded-lg border border-failed/40 bg-failed-soft p-5 text-failed">
    <h1 class="font-semibold">{namespace}/{name} could not load</h1>
    <p class="mt-1 text-sm">{error}</p>
  </div>
{:else if !gi || !detail}
  <p class="mt-10 text-muted-foreground">Loading…</p>
{:else}
  <div class="mt-3 flex flex-wrap items-start justify-between gap-x-6 gap-y-3">
    <div class="min-w-0">
      <h1 class="text-2xl font-semibold tracking-tight">
        <span class="font-normal text-muted-foreground">{gi.namespace} /</span>
        {gi.name}
      </h1>
      <p class="mt-1 text-sm break-all text-muted-foreground">
        {gi.url}
        <span class="text-foreground">at {gi.ref}</span>{#if gi.context !== '.'}, context
          <span class="font-mono text-[0.8125rem]">{gi.context}</span>{/if}
      </p>
    </div>
    <div class="flex items-center gap-3">
      {#if gi.state === 'Building' && gi.buildingSince}
        <span class="font-mono text-sm text-muted-foreground">{stopwatch(clock() - Date.parse(gi.buildingSince))}</span>
      {/if}
      <StateBadge state={gi.state} />
    </div>
  </div>

  <!-- The path of the newest commit -->
  <div class="mt-5 rounded-lg border bg-card px-5 pt-5 pb-4">
    <Track {gi} labelled {rolled} />
    <dl class="mt-4 grid gap-x-8 gap-y-3 border-t pt-4 text-sm sm:grid-cols-2 lg:grid-cols-4">
      <div>
        <dt class="text-muted-foreground">Newest commit</dt>
        <dd><Sha sha={gi.latestRevision} /></dd>
      </div>
      <div class="min-w-0">
        <dt class="text-muted-foreground">{gi.state === 'Building' ? 'Now running' : 'Last result'}</dt>
        <dd class="truncate" title={gi.state === 'Building' ? step.text : undefined}>
          {#if gi.state === 'Building'}
            <span class="font-mono text-[0.8125rem]">{step.text || 'starting'}</span>
          {:else if gi.builds[0]}
            {outcome[gi.builds[0].outcome]} {ago(gi.builds[0].finishedAt)}
          {:else}
            No build yet
          {/if}
        </dd>
      </div>
      <div>
        <dt class="text-muted-foreground">Image pods get now</dt>
        <dd>
          {#if gi.revision}
            from <Sha sha={gi.revision} />
            {#if gi.revision !== gi.latestRevision}<span class="text-muted-foreground">(previous commit)</span>{/if}
          {:else}
            <span class="text-muted-foreground">none yet</span>
          {/if}
        </dd>
      </div>
      <div>
        <dt class="text-muted-foreground">Pods waiting for an image</dt>
        <dd class={detail.gatedPods.length ? 'text-building' : ''}>{detail.gatedPods.length}</dd>
      </div>
    </dl>
  </div>

  {#if gi.failure}
    <div class="mt-4 rounded-lg border border-failed/40 bg-failed-soft px-5 py-4 text-failed">
      <h2 class="font-semibold">{gi.failure.reason}</h2>
      <p class="mt-1 text-sm break-words">{gi.failure.message}</p>
      <p class="mt-3 text-sm">
        Retry without a new commit:
        <button
          type="button"
          class="mt-1 flex items-start gap-2 rounded bg-card/70 px-2 py-1 text-left font-mono text-[0.8125rem] break-all hover:bg-card sm:ml-1 sm:inline-flex"
          onclick={() => copy(`kubectl -n ${gi.namespace} annotate gi ${gi.name} gitimage.dev/rebuild=$(date +%s) --overwrite`)}
        >
          kubectl -n {gi.namespace} annotate gi {gi.name} gitimage.dev/rebuild=$(date +%s) --overwrite
          <Copy class="mt-0.5 size-3.5 shrink-0" />
        </button>
        {#if copied?.startsWith('kubectl')}<span class="ml-2">Copied</span>{/if}
      </p>
    </div>
  {/if}

  {#each detail.warnings as w (w)}
    <p class="mt-4 rounded-lg border border-building/40 bg-building-soft px-5 py-3 text-sm text-building">{w}</p>
  {/each}

  <div class="mt-5 grid items-start gap-5 lg:grid-cols-[minmax(0,1fr)_21rem]">
    <div class="min-w-0">
      {#if selected}
        {#key `${selected}:${attempt}`}
          <LogViewer {namespace} {name} revision={selected} />
        {/key}
      {:else}
        <div class="rounded-lg border bg-card px-6 py-16 text-center text-muted-foreground">
          No commit has been resolved yet, so there is no build to show.
        </div>
      {/if}
    </div>

    <aside class="space-y-5">
      <section class="rounded-lg border bg-card">
        <h2 class="border-b px-4 py-2.5 font-medium">Builds</h2>
        <ul class="divide-y">
          {#if inProgress}
            <li>
              <a
                href={detailHref(namespace, name, inProgress)}
                onclick={link}
                aria-current={selected === inProgress}
                class="flex items-center gap-3 px-4 py-2.5 text-sm hover:bg-muted/60 aria-[current=true]:bg-muted"
              >
                <span class="size-2 shrink-0 rounded-full bg-building live-dot"></span>
                <Sha sha={inProgress} />
                <span class="text-building">Building</span>
              </a>
            </li>
          {/if}
          {#each gi.builds as b (b.revision + b.finishedAt)}
            <li>
              <a
                href={detailHref(namespace, name, b.revision)}
                onclick={link}
                aria-current={selected === b.revision}
                class="flex items-center gap-3 px-4 py-2.5 text-sm hover:bg-muted/60 aria-[current=true]:bg-muted"
              >
                <span
                  class="size-2 shrink-0 rounded-full {b.outcome === 'Failed' ? 'bg-failed' : b.outcome === 'Superseded' ? 'bg-input' : 'bg-ready'}"
                ></span>
                <Sha sha={b.revision} />
                <span class="min-w-0 flex-1 truncate {b.outcome === 'Failed' ? 'text-failed' : ''}">
                  {outcome[b.outcome]}{#if b.sourceRevision}&nbsp;from {short(b.sourceRevision)}{/if}
                </span>
                <span class="shrink-0 text-muted-foreground">{ago(b.finishedAt)}</span>
              </a>
            </li>
          {/each}
          {#if !inProgress && gi.builds.length === 0}
            <li class="px-4 py-4 text-sm text-muted-foreground">No build has run yet.</li>
          {/if}
        </ul>
        <p class="border-t px-4 py-2.5 text-[0.8125rem] text-muted-foreground">
          The last five builds are kept. A log stays readable for one hour after its build ends.
        </p>
      </section>

      <section class="rounded-lg border bg-card">
        <h2 class="border-b px-4 py-2.5 font-medium">Workloads using this image</h2>
        {#if detail.workloads.length === 0}
          <p class="px-4 py-4 text-sm text-muted-foreground">
            None in {gi.namespace}. Add
            <code class="font-mono text-[0.8125rem] break-all">gitimage.dev/image.&lt;container&gt;: {gi.name}</code>
            to a pod template.
          </p>
        {:else}
          <ul class="divide-y">
            {#each detail.workloads as w (w.kind + w.name)}
              <li class="px-4 py-2.5 text-sm">
                <div class="flex items-baseline gap-2">
                  <span class="min-w-0 flex-1 truncate font-medium">{w.name}</span>
                  <span class="shrink-0 {w.ready < w.replicas ? 'text-building' : 'text-muted-foreground'}">
                    {w.ready}/{w.replicas} ready
                  </span>
                </div>
                <div class="mt-0.5 flex flex-wrap items-baseline gap-x-2 text-muted-foreground">
                  <span>{w.kind}</span>
                  {#each w.containers as c (c.container)}
                    <span>
                      {c.container} at
                      {#if c.revision}<Sha sha={c.revision} class={w.upToDate ? 'text-ready' : 'text-building'} />{:else}no
                        image yet{/if}
                    </span>
                  {/each}
                </div>
                {#if w.manual}
                  <div class="mt-1 text-[0.8125rem] text-muted-foreground">
                    Pinned: rollouts are manual{w.upToDate ? '' : ', so it stays on this commit'}.
                  </div>
                {/if}
              </li>
            {/each}
          </ul>
        {/if}
      </section>

      {#if detail.gatedPods.length}
        <section class="rounded-lg border border-building/40 bg-card">
          <h2 class="border-b px-4 py-2.5 font-medium">Pods waiting for an image</h2>
          <ul class="divide-y">
            {#each detail.gatedPods as p (p.name)}
              <li class="px-4 py-2.5 text-sm">
                <div class="truncate font-mono text-[0.8125rem]" title={p.name}>{p.name}</div>
                <div class="text-muted-foreground">
                  {p.containers.join(', ')}{#if p.createdAt}, waiting {span(clock() - Date.parse(p.createdAt))}{/if}
                </div>
              </li>
            {/each}
          </ul>
          <p class="border-t px-4 py-2.5 text-[0.8125rem] text-muted-foreground">
            They start by themselves as soon as the build succeeds.
          </p>
        </section>
      {/if}

      {#if gi.image || gi.webhookURL}
        <section class="rounded-lg border bg-card text-sm">
          {#if gi.image}
            <div class="px-4 py-3">
              <h2 class="font-medium">Image</h2>
              <button type="button" class="mt-1 flex w-full items-start gap-2 text-left text-muted-foreground hover:text-foreground" onclick={() => copy(gi.image!)}>
                <span class="min-w-0 flex-1 font-mono text-[0.8125rem] break-all">{gi.image}</span>
                <Copy class="mt-0.5 size-3.5 shrink-0" />
              </button>
              {#if copied === gi.image}<span class="text-[0.8125rem] text-ready">Copied</span>{/if}
            </div>
          {/if}
          {#if gi.webhookURL}
            <div class="border-t px-4 py-3">
              <h2 class="font-medium">Push hook</h2>
              <p class="mt-0.5 text-muted-foreground">Paste into your git host to build on push. Anyone with this URL can trigger a build.</p>
              <button type="button" class="mt-2 inline-flex items-center gap-1.5 rounded-md border border-input px-2.5 py-1 hover:bg-muted" onclick={() => copy(gi.webhookURL!)}>
                <Copy class="size-3.5" />
                {copied === gi.webhookURL ? 'Copied' : 'Copy hook URL'}
              </button>
            </div>
          {/if}
        </section>
      {/if}
    </aside>
  </div>
{/if}
