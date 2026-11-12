<script lang="ts">
  // The path of the newest commit: commit -> build -> image -> pods. The one drawing every
  // GitImage shares, small in the list and labelled on the detail page.
  import { parseStep, type GitImage } from './api';

  let { gi, labelled = false, rolled }: { gi: GitImage; labelled?: boolean; rolled?: string } = $props();

  const step = $derived(parseStep(gi.step));
  const current = $derived(!!gi.revision && gi.revision === gi.latestRevision);
  /** 0..1 of the build segment. */
  const fraction = $derived(
    gi.state === 'Building'
      ? step.n && step.of
        ? Math.max(0.06, (step.n - 0.5) / step.of)
        : step.text.startsWith('exporting')
          ? 0.96
          : 0.06
      : current
        ? 1
        : 0,
  );
  const tone = $derived(
    gi.state === 'Failed' ? 'failed' : gi.state === 'Building' ? 'building' : current ? 'ready' : 'pending',
  );
  const waiting = $derived(gi.gatedPods > 0);
  const fill = { failed: 'bg-failed', building: 'bg-building', ready: 'bg-ready', pending: 'bg-pending' };
  const text = { failed: 'text-failed', building: 'text-building', ready: 'text-ready', pending: 'text-pending' };
</script>

<div class="flex w-full items-center gap-1.5" aria-hidden={!labelled}>
  <!-- commit -->
  <span class="size-2.5 shrink-0 rounded-full {gi.latestRevision ? fill[tone] : 'bg-input'}"></span>
  <!-- build -->
  <span class="relative h-1 min-w-10 flex-1 overflow-hidden rounded-full bg-muted">
    {#if gi.state === 'Failed'}
      <span class="absolute inset-y-0 left-0 w-1/2 bg-failed"></span>
    {:else}
      <span
        class="absolute inset-y-0 left-0 rounded-full transition-[width] duration-700 {gi.state === 'Building' ? 'track-live' : fill[tone]}"
        style:width="{fraction * 100}%"
      ></span>
    {/if}
  </span>
  <!-- image -->
  <span
    class="size-2.5 shrink-0 rounded-[3px] {current ? fill[tone] : gi.state === 'Failed' ? 'border-2 border-failed' : 'border-2 border-input'}"
  ></span>
  <!-- pods -->
  <span class="h-1 w-6 shrink-0 rounded-full {current && !waiting ? fill[tone] : 'bg-muted'}"></span>
  <span class="flex shrink-0 gap-0.5">
    {#each [0, 1, 2] as i (i)}
      <span
        class="size-1.5 rounded-full {current && !waiting ? fill[tone] : waiting ? 'bg-building live-dot' : 'bg-input'}"
        style:animation-delay="{i * 0.2}s"
      ></span>
    {/each}
  </span>
</div>
{#if labelled}
  <div class="mt-2 grid grid-cols-[auto_1fr_auto] gap-x-3 text-[0.8125rem] whitespace-nowrap text-muted-foreground sm:grid-cols-[auto_1fr_auto_auto]">
    <span>Commit</span>
    <span class="text-center {text[tone]}">
      {#if gi.state === 'Building'}{step.n ? `Building, step ${step.n} of ${step.of}` : 'Building'}
      {:else if gi.state === 'Failed'}Build failed
      {:else if current}Built{:else}Not built yet{/if}
    </span>
    <span class="hidden sm:inline">Image</span>
    <span class="text-right sm:min-w-24">{rolled ?? 'Pods'}</span>
  </div>
{/if}
