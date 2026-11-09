// Typed client for the Trove management API (/api/v1).

export type Format = "raw" | "maven" | "npm" | "pypi" | "cargo" | "docker" | "apt" | "yum";
export type RepoKind = "hosted" | "proxy" | "group";
export type Role = "viewer" | "deployer" | "admin";
export type WritePolicy = "allow_redeploy" | "allow_once" | "read_only";
export type VersionPolicy = "release" | "snapshot" | "mixed";

export const FORMATS: Format[] = ["maven", "npm", "pypi", "cargo", "docker", "apt", "yum", "raw"];
export const KINDS: RepoKind[] = ["hosted", "proxy", "group"];

/** Repository types implemented per format (mirrors the server). */
export function supportedKinds(format: Format): RepoKind[] {
  if (format === "apt" || format === "yum") return ["proxy"];
  if (format === "docker") return ["proxy", "group"];
  return KINDS;
}

export type OutboundMode = "inherit" | "direct" | "custom";

export interface OutboundProxy {
  mode: OutboundMode;
  url: string;
  username: string;
  password?: string;
  no_proxy: string;
}

export interface ProxyConfig {
  remote_url: string;
  metadata_max_age_secs: number;
  username: string;
  password?: string;
  offline: boolean;
  invalidated_at?: number;
  outbound?: OutboundProxy;
}

export interface RemoteTestResult {
  ok: boolean;
  url: string;
  status: number | null;
  elapsed_ms: number;
  message: string;
}

export type ContentDisposition = "attachment" | "inline";

export interface RepoConfig {
  write_policy: WritePolicy;
  proxy?: ProxyConfig;
  group?: { members: string[] };
  version_policy?: VersionPolicy;
  private?: boolean;
  content_disposition?: ContentDisposition;
}

export interface Repository {
  id: string;
  name: string;
  format: Format;
  kind: RepoKind;
  online: boolean;
  description: string;
  config: RepoConfig;
  created_at: number;
  updated_at: number;
  url: string;
  asset_count: number;
  total_size: number;
}

export interface Blob {
  sha256: string;
  sha1: string;
  md5: string;
  sha512: string;
  size: number;
}

export interface Asset {
  id: string;
  repo_id: string;
  path: string;
  content_type: string;
  blob: Blob;
  uploader: string;
  created_at: number;
  updated_at: number;
  last_downloaded_at: number | null;
  attributes: Record<string, unknown>;
  repository?: string;
  format?: Format;
  download_url?: string;
}

export interface Entry {
  name: string;
  path: string;
  is_dir: boolean;
  size: number;
  updated_at: number;
  asset?: Asset;
}

export interface User {
  id: string;
  username: string;
  display_name: string;
  email: string;
  role: Role;
  disabled: boolean;
  created_at: number;
}

export interface Token {
  id: string;
  name: string;
  kind: string;
  prefix: string;
  created_at: number;
  expires_at: number | null;
  last_used_at: number | null;
}

export interface AuditEvent {
  id: string;
  ts: number;
  actor: string;
  action: string;
  target: string;
  detail: string;
  client_ip: string;
}

export interface Stats {
  repositories: number;
  repositories_by_format: Record<string, number>;
  assets: number;
  logical_bytes: number;
  blobs: number;
  stored_bytes: number;
  storage: string;
  database: string;
  counters: Record<string, number>;
  recent: { id: string; path: string; repository: string; size: number; updated_at: number; uploader: string; download_url: string }[];
}

export interface Settings {
  anonymous_read: boolean;
  docker_default_repository: string;
  base_url: string;
  max_upload_mb: number;
  session_ttl_hours: number;
  storage: string;
  database: string;
}

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = { "x-trove-request": "1" };
  if (body !== undefined) headers["content-type"] = "application/json";
  const resp = await fetch(`/api/v1${path}`, {
    method,
    headers,
    credentials: "same-origin",
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (resp.status === 204) return undefined as T;
  const text = await resp.text();
  const data = text ? JSON.parse(text) : undefined;
  if (!resp.ok) {
    throw new ApiError(resp.status, (data && data.error) || resp.statusText);
  }
  return data as T;
}

const q = (params: Record<string, string | number | undefined>) => {
  const s = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v !== undefined && v !== "") s.set(k, String(v));
  const str = s.toString();
  return str ? `?${str}` : "";
};

/** Path inside a repository, percent-encoded per segment. */
export function repositoryPath(repo: string, path: string): string {
  return `/repository/${encodeURIComponent(repo)}/${path.split("/").filter(Boolean).map(encodeURIComponent).join("/")}`;
}

/** Upload one file with progress; resolves with the HTTP status. */
export function uploadFile(repo: string, path: string, file: File, onProgress: (loaded: number) => void): Promise<number> {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open("PUT", repositoryPath(repo, path));
    xhr.setRequestHeader("x-trove-request", "1");
    xhr.withCredentials = true;
    xhr.upload.onprogress = (e) => onProgress(e.loaded);
    xhr.onload = () => resolve(xhr.status);
    xhr.onerror = () => reject(new Error("network error"));
    xhr.send(file);
  });
}

export async function deletePath(repo: string, path: string): Promise<void> {
  const resp = await fetch(repositoryPath(repo, path), { method: "DELETE", headers: { "x-trove-request": "1" }, credentials: "same-origin" });
  if (!resp.ok && resp.status !== 204) throw new ApiError(resp.status, (await resp.text()).trim() || resp.statusText);
}

export const api = {
  status: () => request<{ version: string; anonymous_read: boolean }>("GET", "/status"),
  stats: () => request<Stats>("GET", "/stats"),

  login: (username: string, password: string) => request<{ user: User }>("POST", "/auth/login", { username, password }),
  logout: () => request<void>("POST", "/auth/logout"),
  me: () => request<{ user: User; anonymous_read: boolean }>("GET", "/auth/me"),
  changePassword: (current: string, next: string) => request<void>("PUT", "/auth/me/password", { current, new: next }),

  repos: () => request<Repository[]>("GET", "/repositories"),
  repo: (name: string) => request<Repository>("GET", `/repositories/${encodeURIComponent(name)}`),
  createRepo: (body: Partial<Repository>) => request<Repository>("POST", "/repositories", body),
  updateRepo: (name: string, body: Partial<Repository>) => request<Repository>("PUT", `/repositories/${encodeURIComponent(name)}`, body),
  deleteRepo: (name: string) => request<void>("DELETE", `/repositories/${encodeURIComponent(name)}`),
  testRemote: (body: { format: Format; proxy: ProxyConfig; name?: string }) =>
    request<RemoteTestResult>("POST", "/remote-test", body),
  invalidateCache: (name: string) => request<void>("POST", `/repositories/${encodeURIComponent(name)}/invalidate-cache`),
  browse: (name: string, path: string) =>
    request<{ path: string; entries: Entry[] }>("GET", `/repositories/${encodeURIComponent(name)}/browse${q({ path })}`),

  search: (params: { q: string; repo?: string; limit?: number; offset?: number }) =>
    request<{ items: Asset[]; total: number }>("GET", `/assets${q(params)}`),
  asset: (id: string) => request<Asset>("GET", `/assets/${id}`),
  deleteAsset: (id: string) => request<void>("DELETE", `/assets/${id}`),

  users: () => request<User[]>("GET", "/users"),
  createUser: (body: { username: string; password: string; display_name: string; email: string; role: Role }) =>
    request<User>("POST", "/users", body),
  updateUser: (id: string, body: Partial<User> & { password?: string }) => request<User>("PUT", `/users/${id}`, body),
  deleteUser: (id: string) => request<void>("DELETE", `/users/${id}`),

  tokens: () => request<Token[]>("GET", "/tokens"),
  createToken: (name: string, expires_in_days: number) =>
    request<{ token: Token; secret: string }>("POST", "/tokens", { name, expires_in_days }),
  deleteToken: (id: string) => request<void>("DELETE", `/tokens/${id}`),

  settings: () => request<Settings>("GET", "/system/settings"),
  updateSettings: (body: Partial<Settings>) => request<Settings>("PUT", "/system/settings", body),
  gc: () => request<{ deleted_blobs: number; freed_bytes: number; expired_tokens: number }>("POST", "/system/gc"),
  audit: (params: { actor?: string; action?: string; limit?: number; offset?: number }) =>
    request<{ items: AuditEvent[]; total: number }>("GET", `/system/audit${q(params)}`),
};
