// A two-route history router; the server answers every unknown path with index.html.
let path = $state(location.pathname);
let search = $state(location.search);

function sync() {
  path = location.pathname;
  search = location.search;
}
window.addEventListener('popstate', sync);

export function navigate(to: string, replace = false) {
  if (replace) history.replaceState(null, '', to);
  else history.pushState(null, '', to);
  sync();
  if (!replace) window.scrollTo(0, 0);
}

/** For `<a href onclick={link}>`: client-side navigation, but modified clicks still open tabs. */
export function link(e: MouseEvent) {
  if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
  const a = (e.currentTarget as HTMLElement).closest('a');
  if (!a || a.origin !== location.origin) return;
  e.preventDefault();
  navigate(a.pathname + a.search);
}

export type Route =
  | { page: 'list' }
  | { page: 'detail'; namespace: string; name: string; revision: string | null }
  | { page: 'signed-out' };

export function route(): Route {
  const m = path.match(/^\/gi\/([^/]+)\/([^/]+)\/?$/);
  if (m) {
    return {
      page: 'detail',
      namespace: decodeURIComponent(m[1]),
      name: decodeURIComponent(m[2]),
      revision: new URLSearchParams(search).get('build'),
    };
  }
  if (path === '/signed-out') return { page: 'signed-out' };
  return { page: 'list' };
}

export const detailHref = (ns: string, name: string, build?: string) =>
  `/gi/${encodeURIComponent(ns)}/${encodeURIComponent(name)}${build ? `?build=${build}` : ''}`;
