<script lang="ts">
  import type { Snippet } from 'svelte';
  import type { HTMLAnchorAttributes, HTMLButtonAttributes } from 'svelte/elements';

  type Props = {
    variant?: 'default' | 'outline' | 'ghost';
    size?: 'default' | 'sm';
    href?: string;
    children: Snippet;
  } & HTMLButtonAttributes &
    HTMLAnchorAttributes;

  let { variant = 'default', size = 'default', href, children, class: cls = '', ...rest }: Props = $props();

  const variants = {
    default: 'bg-primary text-primary-foreground hover:opacity-90',
    outline: 'border border-input bg-card hover:bg-muted',
    ghost: 'hover:bg-muted',
  };
  const sizes = { default: 'h-9 px-4', sm: 'h-8 px-3 text-sm' };
  const klass = $derived(
    `inline-flex items-center justify-center gap-2 rounded-md font-medium whitespace-nowrap transition-colors disabled:opacity-50 ${variants[variant]} ${sizes[size]} ${cls}`,
  );
</script>

{#if href}
  <a {href} class={klass} {...rest as HTMLAnchorAttributes}>{@render children()}</a>
{:else}
  <button type="button" class={klass} {...rest as HTMLButtonAttributes}>{@render children()}</button>
{/if}
