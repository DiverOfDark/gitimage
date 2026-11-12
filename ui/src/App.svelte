<script lang="ts">
  import { link, route } from './lib/router.svelte';
  import { app, start } from './lib/store.svelte';
  import Button from './lib/ui/Button.svelte';
  import Detail from './routes/Detail.svelte';
  import List from './routes/List.svelte';

  const r = $derived(route());
  if (route().page !== 'signed-out') start();
</script>

<header class="border-b bg-card">
  <div class="mx-auto flex h-14 max-w-7xl items-center gap-4 px-4 sm:px-6">
    <a href="/" onclick={link} class="flex items-center gap-2.5 font-semibold tracking-tight">
      <svg viewBox="0 0 32 32" class="size-6" aria-hidden="true">
        <rect width="32" height="32" rx="7" class="fill-primary" />
        <circle cx="9" cy="16" r="3.5" class="fill-primary-foreground" />
        <path d="M12.5 16h5" class="stroke-primary-foreground" stroke-width="2.5" />
        <rect x="17.5" y="10.5" width="9" height="11" rx="2" fill="none" class="stroke-primary-foreground" stroke-width="2.5" />
      </svg>
      GitImage
    </a>
    <div class="ml-auto flex items-center gap-3 text-sm">
      {#if app.me}
        <span
          class="hidden items-center gap-1.5 text-muted-foreground sm:flex"
          title={app.live ? 'Updates arrive as they happen' : 'Reconnecting'}
        >
          <span class="size-1.5 rounded-full {app.live ? 'bg-ready' : 'bg-input'}"></span>
          {app.live ? 'Live' : 'Offline'}
        </span>
      {/if}
      {#if app.me?.authMode === 'oidc'}
        <span class="hidden text-muted-foreground md:inline">{app.me.user}</span>
        <Button variant="outline" size="sm" href="/auth/logout">Sign out</Button>
      {/if}
    </div>
  </div>
</header>

<main class="mx-auto max-w-7xl px-4 py-6 sm:px-6">
  {#if r.page === 'signed-out'}
    <div class="mx-auto mt-24 max-w-sm text-center">
      <h1 class="text-xl font-semibold">You are signed out</h1>
      <p class="mt-2 text-muted-foreground">Your identity provider may still have a session open.</p>
      <Button class="mt-6" href="/auth/login?return_to=%2F">Sign in</Button>
    </div>
  {:else if app.error && !app.items}
    <div class="mx-auto mt-24 max-w-lg rounded-lg border border-failed/40 bg-failed-soft p-5 text-failed">
      <h1 class="font-semibold">GitImage could not load</h1>
      <p class="mt-1 text-sm">{app.error}</p>
      <Button class="mt-4" variant="outline" size="sm" onclick={() => location.reload()}>Try again</Button>
    </div>
  {:else if r.page === 'detail'}
    {#key `${r.namespace}/${r.name}`}
      <Detail namespace={r.namespace} name={r.name} build={r.revision} />
    {/key}
  {:else}
    <List />
  {/if}
</main>
