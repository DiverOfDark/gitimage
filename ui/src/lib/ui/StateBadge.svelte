<script lang="ts">
  import type { State } from '../api';

  let { state, label }: { state: State; label?: string } = $props();

  const styles: Record<State, string> = {
    Building: 'bg-building-soft text-building',
    Failed: 'bg-failed-soft text-failed',
    Ready: 'bg-ready-soft text-ready',
    Pending: 'bg-pending-soft text-pending',
  };
  const dots: Record<State, string> = {
    Building: 'bg-building live-dot',
    Failed: 'bg-failed',
    Ready: 'bg-ready',
    Pending: 'bg-pending',
  };
</script>

<span class="inline-flex items-center gap-1.5 rounded-full px-2.5 py-0.5 text-[0.8125rem] font-medium {styles[state]}">
  <span class="size-1.5 rounded-full {dots[state]}"></span>
  {label ?? state}
</span>
