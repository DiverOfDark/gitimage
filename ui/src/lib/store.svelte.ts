import { ApiError, get, type GitImage, type Me } from './api';

/** Everything the pages share: who is signed in and the live list of GitImages. */
export const app = $state<{
  me: Me | null;
  items: GitImage[] | null;
  error: string | null;
  /** The event stream is connected. */
  live: boolean;
}>({ me: null, items: null, error: null, live: false });

let source: EventSource | null = null;

export async function start() {
  try {
    app.me = await get<Me>('/api/me');
    app.items = await get<GitImage[]>('/api/gitimages');
    app.error = null;
  } catch (e) {
    if (!(e instanceof ApiError && e.status === 401)) app.error = (e as Error).message;
    return;
  }
  source?.close();
  source = new EventSource('/api/events');
  source.addEventListener('gitimages', (e) => {
    app.items = JSON.parse((e as MessageEvent).data);
    app.live = true;
    app.error = null;
  });
  source.onerror = () => {
    app.live = false;
    // The browser reconnects by itself, but not through an expired session: this redirects
    // to sign-in on 401.
    get<Me>('/api/me').catch((e) => {
      if (!(e instanceof ApiError && e.status === 401)) app.error = 'Connection to GitImage lost. Retrying…';
    });
  };
}
