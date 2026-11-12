// Mock of the controller's UI API for `npm run dev`: a small cluster with one build in
// progress, so the app can be developed and reviewed without Kubernetes.
import type { IncomingMessage, ServerResponse } from 'node:http';
import type { Plugin } from 'vite';

const T0 = Date.now();
const iso = (msAgo: number) => new Date(Date.now() - msAgo).toISOString().replace(/\.\d+Z$/, 'Z');
const MIN = 60_000;
const H = 60 * MIN;
const D = 24 * H;

const SCRIPT: { title: string; internal?: boolean; cached?: boolean; ms: number; out?: string[]; error?: string }[] = [
  { title: 'load build definition from Dockerfile', internal: true, ms: 300 },
  { title: 'load metadata for docker.io/library/node:22-alpine', internal: true, ms: 900 },
  { title: 'load .dockerignore', internal: true, ms: 200 },
  { title: 'step 1/9 FROM docker.io/library/node:22-alpine@sha256:5340cbfc2df14331ab021555fdd9f83f072ce811488e705b0e736b11adeec4bb', cached: true, ms: 200 },
  { title: 'load build context', internal: true, ms: 700 },
  { title: 'step 2/9 WORKDIR /app', cached: true, ms: 100 },
  { title: 'step 3/9 COPY package.json package-lock.json ./', ms: 400 },
  {
    title: 'step 4/9 RUN --mount=type=secret,id=npmrc,target=/root/.npmrc npm ci',
    ms: 9000,
    out: [
      'npm warn deprecated inflight@1.0.6: This module is not supported, and leaks memory.',
      'npm warn deprecated glob@7.2.3: Glob versions prior to v9 are no longer supported',
      '',
      'added 812 packages, and audited 813 packages in 8s',
      '',
      '143 packages are looking for funding',
      '  run `npm fund` for details',
      '',
      'found 0 vulnerabilities',
    ],
  },
  { title: 'step 5/9 COPY . .', ms: 600 },
  {
    title: 'step 6/9 RUN npm run build',
    ms: 14000,
    out: [
      '',
      '> storefront@2.4.0 build',
      '> vite build',
      '',
      'vite v8.3.8 building for production...',
      'transforming...',
      '✓ 1284 modules transformed.',
      'rendering chunks...',
      'computing gzip size...',
      'dist/index.html                   0.61 kB │ gzip:   0.37 kB',
      'dist/assets/index-DiwrgTda.css   48.12 kB │ gzip:   9.04 kB',
      'dist/assets/index-C2PWchud.js   412.77 kB │ gzip: 131.20 kB',
      '✓ built in 11.84s',
    ],
  },
  { title: 'step 7/9 RUN npm prune --omit=dev', ms: 3000, out: ['', 'removed 388 packages, and audited 425 packages in 2s', 'found 0 vulnerabilities'] },
  { title: 'step 8/9 COPY --from=build /app/dist ./dist', ms: 500 },
  { title: 'step 9/9 USER node', ms: 100 },
  { title: 'exporting to image', ms: 5000 },
  { title: 'exporting cache to registry', ms: 4000 },
];
const TOTAL = SCRIPT.reduce((a, s) => a + s.ms, 0) + 4000;

/** Where the looping demo build is right now. */
function progress() {
  const loop = TOTAL + 6000;
  const since = T0 + Math.floor((Date.now() - T0) / loop) * loop;
  let t = Date.now() - since;
  t -= 4000; // pod startup
  if (t < 0) return { step: 'starting', since };
  for (const s of SCRIPT) {
    if (t < s.ms) return { step: s.title, since };
    t -= s.ms;
  }
  return { step: 'exporting cache to registry', since };
}

const sha = (seed: string) => (seed.repeat(40).match(/.{40}/) as RegExpMatchArray)[0];
const R = {
  storeNew: sha('9c4e1f7a02'),
  storeOld: sha('3f2c9e1ab7'),
  storeOlder: sha('b81d0c55e4'),
  api: sha('5d07aa31c9'),
  apiOld: sha('e2b9140f6d'),
  workerBad: sha('a61f3d9028'),
  workerGood: sha('07c5be4419'),
  docs: sha('d3e8f6027b'),
  fresh: sha('4b9a7c10ee'),
};
const dg = (seed: string) => `sha256:${seed.repeat(64).slice(0, 64)}`;

function gitimages() {
  const p = progress();
  return [
    {
      namespace: 'shop', name: 'storefront', url: 'git@github.com:acme/storefront.git', ref: 'main',
      context: '.', dockerfile: 'Dockerfile', state: 'Building', step: p.step,
      buildingSince: new Date(p.since).toISOString(),
      revision: R.storeOld, latestRevision: R.storeNew,
      image: `harbor.example.com/builds/shop/storefront@${dg('71ab')}`,
      createdAt: iso(41 * D), gatedPods: 0,
      builds: [
        { revision: R.storeOld, outcome: 'Built', digest: dg('71ab'), finishedAt: iso(26 * MIN) },
        { revision: R.storeOlder, outcome: 'Built', digest: dg('c0de'), finishedAt: iso(2 * D) },
      ],
    },
    {
      namespace: 'shop', name: 'api', url: 'git@github.com:acme/shop.git', ref: 'main',
      context: 'services/api', dockerfile: 'Dockerfile', state: 'Ready',
      revision: R.api, latestRevision: R.api,
      image: `harbor.example.com/builds/shop/api@${dg('5d07')}`,
      createdAt: iso(41 * D), gatedPods: 0,
      builds: [
        { revision: R.api, outcome: 'Reused', digest: dg('5d07'), sourceRevision: R.apiOld, finishedAt: iso(3 * H) },
        { revision: R.apiOld, outcome: 'Built', digest: dg('5d07'), finishedAt: iso(5 * D) },
      ],
    },
    {
      namespace: 'shop', name: 'worker', url: 'git@github.com:acme/shop.git', ref: 'main',
      context: 'services/worker', dockerfile: 'Dockerfile', state: 'Failed',
      failure: {
        reason: 'BuildFailed',
        message: `build of ${R.workerBad.slice(0, 7)} failed: step 5/8 RUN cargo build --release: process "/bin/sh -c cargo build --release" did not complete successfully: exit code: 101; push a new commit or change gitimage.dev/rebuild`,
      },
      revision: R.workerGood, latestRevision: R.workerBad,
      image: `harbor.example.com/builds/shop/worker@${dg('07c5')}`,
      createdAt: iso(40 * D), gatedPods: 0,
      builds: [
        { revision: R.workerBad, outcome: 'Failed', reason: 'BuildFailed', message: 'step 5/8 RUN cargo build --release: process "/bin/sh -c cargo build --release" did not complete successfully: exit code: 101', finishedAt: iso(12 * MIN) },
        { revision: R.workerGood, outcome: 'Built', digest: dg('07c5'), finishedAt: iso(4 * D) },
      ],
    },
    {
      namespace: 'docs', name: 'handbook', url: 'https://gitea.example.com/team/handbook.git', ref: 'v2.3.0',
      context: '.', dockerfile: 'Dockerfile', state: 'Ready',
      revision: R.docs, latestRevision: R.docs,
      image: `harbor.example.com/builds/docs/handbook@${dg('d3e8')}`,
      createdAt: iso(9 * D), gatedPods: 0,
      builds: [{ revision: R.docs, outcome: 'Built', digest: dg('d3e8'), finishedAt: iso(9 * D) }],
    },
    {
      namespace: 'sandbox', name: 'preview-1482', url: 'git@github.com:acme/storefront.git', ref: 'feat/checkout-v2',
      context: '.', dockerfile: 'Dockerfile', state: 'Pending',
      latestRevision: R.fresh, createdAt: iso(40_000), gatedPods: 2, builds: [],
    },
  ];
}

function detail(ns: string, name: string) {
  const gi = gitimages().find((g) => g.namespace === ns && g.name === name);
  if (!gi) return null;
  const withHook = { ...gi, webhookURL: name === 'storefront' ? 'https://gitimage.example.com/hooks/k3J9xQ2mT7vB1nR8' : undefined };
  const workloads =
    name === 'storefront'
      ? [
          { kind: 'Deployment', name: 'storefront', containers: [{ container: 'web', revision: R.storeOld }], manual: false, upToDate: true, replicas: 3, ready: 3, updated: 3 },
          { kind: 'Deployment', name: 'storefront-canary', containers: [{ container: 'web', revision: R.storeOlder }], manual: true, upToDate: false, replicas: 1, ready: 1, updated: 1 },
        ]
      : name === 'worker'
        ? [{ kind: 'StatefulSet', name: 'worker', containers: [{ container: 'worker', revision: R.workerGood }], manual: false, upToDate: true, replicas: 2, ready: 2, updated: 2 }]
        : name === 'preview-1482'
          ? [{ kind: 'Deployment', name: 'preview-1482', containers: [{ container: 'web' }], manual: false, upToDate: false, replicas: 2, ready: 0, updated: 0 }]
          : [{ kind: 'Deployment', name, containers: [{ container: 'app', revision: gi.revision }], manual: false, upToDate: true, replicas: 2, ready: 2, updated: 2 }];
  const gatedPods =
    name === 'preview-1482'
      ? [
          { name: 'preview-1482-6d5f8b7c9-x2k4p', containers: ['web'], owner: 'ReplicaSet/preview-1482-6d5f8b7c9', createdAt: iso(38_000) },
          { name: 'preview-1482-6d5f8b7c9-q8n7m', containers: ['web'], owner: 'ReplicaSet/preview-1482-6d5f8b7c9', createdAt: iso(38_000) },
        ]
      : [];
  return { gitimage: withHook, workloads, gatedPods, warnings: [] };
}

function sse(res: ServerResponse) {
  res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache', connection: 'keep-alive' });
  return (event: string, data: unknown) => res.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Replays the scripted build; `live` paces it like a real one, otherwise it arrives at once. */
async function streamLogs(req: IncomingMessage, res: ServerResponse, mode: 'live' | 'done' | 'failed' | 'gone' | 'waiting') {
  const send = sse(res);
  let open = true;
  req.on('close', () => (open = false));
  if (mode === 'gone') {
    send('end', { phase: 'Gone', message: 'No build pod for this commit. Build pods are removed one hour after the build ends.' });
    return res.end();
  }
  if (mode === 'waiting') {
    while (open) {
      send('status', { detail: 'buildkit: PodInitializing' });
      await sleep(2000);
    }
    return;
  }
  const pace = async (ms: number) => {
    if (mode === 'live') await sleep(ms);
  };
  send('status', { detail: 'waiting for the build pod' });
  await pace(1500);
  send('status', { detail: '' });
  send('text', { source: 'clone', text: 'INFO fetching url=git@github.com:acme/storefront.git ref=main' });
  await pace(900);
  send('text', { source: 'clone', text: `INFO checked out sha=${R.storeNew} files=1412` });
  await pace(600);
  let clock = Date.now() - (mode === 'live' ? 0 : 20 * MIN);
  const failAt = mode === 'failed' ? 9 : -1;
  for (const [i, s] of SCRIPT.entries()) {
    if (!open) return;
    const id = `sha256:${String(i).padStart(4, '0')}`;
    const started = new Date(clock).toISOString();
    const base = { id, title: failAt === i ? 'step 5/8 RUN cargo build --release' : s.title, internal: !!s.internal, cached: !!s.cached };
    send('build', { steps: [{ ...base, started }], logs: [] });
    const out = failAt === i
      ? ['   Compiling worker v0.9.2 (/app)', 'error[E0425]: cannot find value `retry_budget` in this scope', '  --> src/queue.rs:118:27', '    |', '118 |         if attempts > retry_budget {', '    |                       ^^^^^^^^^^^^ not found in this scope', '', 'error: could not compile `worker` (bin "worker") due to 1 previous error']
      : (s.out ?? []);
    for (const line of out) {
      if (!open) return;
      await pace(s.ms / (out.length + 1));
      send('build', { steps: [], logs: [{ step: id, text: line + '\n' }] });
    }
    await pace(out.length ? s.ms / (out.length + 1) : s.ms);
    clock += mode === 'live' ? 0 : s.ms;
    const completed = new Date(mode === 'live' ? Date.now() : clock).toISOString();
    if (failAt === i) {
      send('build', { steps: [{ ...base, started, completed, error: 'process "/bin/sh -c cargo build --release" did not complete successfully: exit code: 101' }], logs: [] });
      send('text', { source: 'buildkit', text: 'error: failed to solve: process "/bin/sh -c cargo build --release" did not complete successfully: exit code: 101' });
      send('end', { phase: 'Failed', message: 'container buildkit exited with code 1 (Error)' });
      return res.end();
    }
    send('build', { steps: [{ ...base, started, completed }], logs: [] });
  }
  send('end', { phase: 'Succeeded', message: null });
  res.end();
}

export function mockApi(): Plugin {
  return {
    name: 'gitimage-mock-api',
    configureServer(server) {
      server.middlewares.use(async (req, res, next) => {
        const url = new URL(req.url ?? '/', 'http://localhost');
        const json = (body: unknown, status = 200) => {
          res.writeHead(status, { 'content-type': 'application/json' });
          res.end(JSON.stringify(body));
        };
        if (url.pathname === '/api/me') return json({ user: 'kirill@example.com', authMode: 'oidc' });
        if (url.pathname === '/api/gitimages') return json(gitimages());
        if (url.pathname === '/api/events') {
          const send = sse(res);
          let last = '';
          const tick = () => {
            const now = JSON.stringify(gitimages());
            if (now !== last) send('gitimages', JSON.parse((last = now)));
          };
          tick();
          const timer = setInterval(tick, 1000);
          req.on('close', () => clearInterval(timer));
          return;
        }
        const m = url.pathname.match(/^\/api\/gitimages\/([^/]+)\/([^/]+)(\/logs)?$/);
        if (m) {
          const [, ns, name, logs] = m;
          if (!logs) {
            const d = detail(ns, name);
            return d ? json(d) : json({ error: `gitimages.gitimage.dev "${name}" not found` }, 404);
          }
          const rev = url.searchParams.get('revision');
          const mode =
            rev === R.storeNew ? 'live'
            : rev === R.workerBad ? 'failed'
            : rev === R.storeOld ? 'done'
            : rev === R.fresh ? 'waiting'
            : 'gone';
          return streamLogs(req, res, mode);
        }
        if (url.pathname === '/auth/logout') {
          res.writeHead(302, { location: '/signed-out' });
          return res.end();
        }
        if (url.pathname === '/auth/login') {
          res.writeHead(302, { location: url.searchParams.get('return_to') ?? '/' });
          return res.end();
        }
        next();
      });
    },
  };
}
