<script lang="ts">
  import { short } from '../api';

  let { sha, class: cls = '' }: { sha?: string; class?: string } = $props();
</script>

{#if sha}
  <span class="font-mono text-[0.8125rem] {cls}" title={sha}>{short(sha)}</span>
{:else}
  <span class="text-muted-foreground">none</span>
{/if}
