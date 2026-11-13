// Shapes of the controller's UI API (crates/gitimage-controller/src/ui/view.rs).

export type State = 'Building' | 'Failed' | 'Ready' | 'Pending';

export interface BuildRecord {
  revision: string;
  outcome: 'Built' | 'Reused' | 'Superseded' | 'Failed';
  digest?: string;
  sourceRevision?: string;
  reason?: string;
  message?: string;
  finishedAt: string;
}

export interface GitImage {
  namespace: string;
  name: string;
  url: string;
  ref: string;
  context: string;
  dockerfile: string;
  state: State;
  step?: string;
  buildingSince?: string;
  /** Commit of the running build; can be older than latestRevision. */
  buildingRevision?: string;
  failure?: { reason: string; message: string };
  revision?: string;
  latestRevision?: string;
  image?: string;
  webhookURL?: string;
  createdAt?: string;
  gatedPods: number;
  builds: BuildRecord[];
}

export interface Workload {
  kind: 'Deployment' | 'StatefulSet' | 'DaemonSet';
  name: string;
  containers: { container: string; revision?: string }[];
  manual: boolean;
  upToDate: boolean;
  replicas: number;
  ready: number;
  updated: number;
}

export interface GatedPod {
  name: string;
  containers: string[];
  owner?: string;
  createdAt?: string;
}

export interface Detail {
  gitimage: GitImage;
  workloads: Workload[];
  gatedPods: GatedPod[];
  warnings: string[];
}

export interface Me {
  user: string | null;
  authMode: 'oidc' | 'none';
}

export interface Step {
  id: string;
  title: string;
  /** The Dockerfile stage of a numbered step. */
  stage?: string;
  internal: boolean;
  started?: string;
  completed?: string;
  cached: boolean;
  error?: string;
}

/** Work inside a step that prints nothing: a layer being downloaded or unpacked. */
export interface Task {
  step: string;
  id: string;
  current: number;
  total: number;
  started?: string;
  completed?: string;
}

export interface BuildEvent {
  steps: Step[];
  logs: { step: string; text: string }[];
  tasks?: Task[];
}

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

/** Sends the browser to the sign-in page, coming back to the current page afterwards. */
export function signIn() {
  const here = location.pathname + location.search;
  location.assign(`/auth/login?return_to=${encodeURIComponent(here)}`);
}

export async function get<T>(path: string): Promise<T> {
  const res = await fetch(path, { headers: { accept: 'application/json' } });
  if (res.status === 401) {
    signIn();
    throw new ApiError(401, 'Signing in…');
  }
  if (!res.ok) {
    let message = `${res.status} ${res.statusText}`;
    try {
      message = (await res.json()).error ?? message;
    } catch {
      // not JSON; keep the status line
    }
    throw new ApiError(res.status, message);
  }
  return res.json();
}

export const short = (sha?: string) => (sha ? sha.slice(0, 7) : '');

/** `step 4/12 RUN npm ci` -> { n: 4, of: 12, text: 'RUN npm ci' } */
export function parseStep(step?: string): { n?: number; of?: number; text: string } {
  const m = step?.match(/^step (\d+)\/(\d+) (.*)$/s);
  if (!m) return { text: step ?? '' };
  return { n: Number(m[1]), of: Number(m[2]), text: m[3] };
}
