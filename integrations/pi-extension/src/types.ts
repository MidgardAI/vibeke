// Minimal structural types for the parts of the pi/omp extension API we use.
// Deliberately local: the package must not import host packages at runtime,
// and `import type` would need them installed at build time.

export interface UiDialogOptions {
  signal?: AbortSignal;
  timeout?: number;
  [k: string]: unknown;
}

export interface UiContext {
  confirm?: (title: string, message: string, opts?: UiDialogOptions) => Promise<boolean>;
  select?: (title: string, options: string[], opts?: UiDialogOptions) => Promise<string | undefined>;
  input?: (title: string, placeholder?: string, opts?: UiDialogOptions) => Promise<string | undefined>;
  [k: string]: unknown;
}

export interface HostContext {
  cwd?: string;
  mode?: string;
  ui?: UiContext;
  model?: HostModel | null;
  modelRegistry?: {
    getAvailable?: () => HostModel[] | Promise<HostModel[]>;
    find?: (provider: string, id: string) => HostModel | undefined;
  };
  sessionManager?: {
    getSessionFile?: () => string | undefined;
    getSessionId?: () => string | undefined;
  };
  [k: string]: unknown;
}

export interface HostModel {
  id?: string;
  name?: string;
  provider?: string;
  [k: string]: unknown;
}

export type Handler = (event: any, ctx: HostContext) => unknown;

export interface HostApi {
  on(event: string, handler: Handler): unknown;
  version?: string;
  /** pi / omp: switch the session's model; false when the provider has no credentials. */
  setModel?: (model: HostModel) => Promise<boolean>;
  getCommands?: () => { name?: string; description?: string }[];
  [k: string]: unknown;
}
