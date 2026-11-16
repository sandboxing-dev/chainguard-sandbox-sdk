// Session lifecycle: the unary Sessions calls the simple sandbox API uses. Mirrors the Go SDK's
// workspaces/sessions.go.
import { create } from "@bufbuild/protobuf";
import { FieldMaskSchema, durationFromMs } from "@bufbuild/protobuf/wkt";
import {
  IdlePolicy,
  Provenance,
  SessionSchema,
  Session_ParkRequest,
  type Session,
} from "./gen/sandbox/runtime/session/v1/session_pb.js";
import type { Client } from "./client.js";
import { normalizeGitHubUrl } from "./url.js";
import defaultEnvironmentText from "./default_environment.js";

export type { Session };

/** The document chainctl 0.2.398 synthesizes when a directory has no .chainguard/ files. */
export const DEFAULT_ENVIRONMENT: Uint8Array = new TextEncoder().encode(defaultEnvironmentText);
/** The check in DEFAULT_ENVIRONMENT; the plane selects it itself. */
export const DEFAULT_JOB = "develop";

export class NotFoundError extends Error {}
export class AmbiguousError extends Error {}

export function isSessionName(s: string): boolean {
  const p = s.split("/");
  return p.length === 4 && p[0] === "instances" && p[2] === "sessions" && !!p[1] && !!p[3];
}

/** A session opened from a repository URL (default branch, or revision). */
/** Normalize "github.com/o/r", "git@github.com:o/r.git", … to "https://github.com/o/r". */
export function repositoryUrl(repo: string): string {
  const u = normalizeGitHubUrl(repo);
  // Bare "host/path" gets https://; scp-style non-GitHub remotes are left as given.
  return u.includes("://") || /^[^/@]*@?[^/:]+:/.test(u) ? u : `https://${u}`;
}

/**
 * A session opened from a repository URL, like `chainctl develop github.com/org/repo`,
 * shaped like chainctl's request: an https repository URL (the plane can't fetch the
 * bare "github.com/…" form), idle AUTO_HOLD, and DEFAULT_ENVIRONMENT as the fallback
 * for repos without .chainguard/ files. revision pins a commit; empty lets the plane
 * use the default branch's HEAD (verified live).
 */
export function repositorySession(repository: string, revision = ""): Session {
  return create(SessionSchema, {
    idle: IdlePolicy.AUTO_HOLD,
    environment: { case: "source", value: { repository: repositoryUrl(repository), revision, fallbackDocument: DEFAULT_ENVIRONMENT } },
  });
}

/**
 * A session whose /work tree is uploaded on the first attach, like `chainctl develop ./dir`.
 * The simple sandbox API uses it with an empty tree (see completeFirstAttach). label names the
 * workspace; document is the environment document, or undefined for DEFAULT_ENVIRONMENT (SYNTHESIZED).
 */
export function workspaceSession(label: string, repository = "", revision = "", document?: Uint8Array): Session {
  return create(SessionSchema, {
    idle: IdlePolicy.AUTO_HOLD,
    environment: {
      case: "workspace",
      value: {
        label,
        repository,
        revision,
        document: document ?? DEFAULT_ENVIRONMENT,
        provenance: document ? Provenance.AUTHORED : Provenance.SYNTHESIZED,
      },
    },
  });
}

export async function createSession(c: Client, s: Session): Promise<Session> {
  return c.raw.createSession({ parent: (await c.parentName()), session: s });
}

export async function getSession(c: Client, name: string, signal?: AbortSignal): Promise<Session> {
  return c.raw.getSession({ name }, { signal });
}

export interface ListOptions {
  /** Another user's sessions (needs the owner role). */
  owner?: string;
  pageSize?: number;
}

export async function listSessions(c: Client, o: ListOptions = {}): Promise<Session[]> {
  const out: Session[] = [];
  let pageToken = "";
  for (;;) {
    const resp = await c.raw.listSessions({ parent: (await c.parentName()), pageSize: o.pageSize ?? 0, owner: o.owner ?? "", pageToken });
    out.push(...resp.sessions);
    if (!resp.nextPageToken) return out;
    pageToken = resp.nextPageToken;
  }
}

/** Exact: session ID, short ID, or display name. */
function exactMatch(s: Session, ref: string): boolean {
  return s.shortId === ref || s.name.split("/").pop() === ref || (!!s.displayName && s.displayName === ref);
}

/** chainctl's owner-qualified form: "owner/<display name>" (one owner segment). */
function ownerMatch(s: Session, ref: string): boolean {
  const i = ref.indexOf("/");
  return !!s.displayName && i > 0 && ref.slice(i + 1) === s.displayName;
}

/** Suffix: "quiet-harbor" or "app/quiet-harbor" against display name "app/quiet-harbor". */
function suffixMatch(s: Session, ref: string): boolean {
  return !!s.displayName && s.displayName.endsWith("/" + ref);
}

/**
 * Finds a session by full name, short ID, or display name ("quiet-harbor",
 * "app/quiet-harbor", "owner/app/quiet-harbor"). Tiers, first unique hit
 * wins: exact (ID, short ID, display name), then owner-qualified, then suffix.
 */
export async function resolveSession(c: Client, ref: string): Promise<Session> {
  if (isSessionName(ref)) return getSession(c, ref);
  const all = await listSessions(c);
  for (const pick of [exactMatch, ownerMatch, suffixMatch]) {
    const hits = all.filter((s) => pick(s, ref));
    if (hits.length === 1) return hits[0]!;
    if (hits.length > 1) throw new AmbiguousError(`chainguard-sandbox: reference matches more than one session: "${ref}"`);
  }
  throw new NotFoundError(`chainguard-sandbox: no matching session: "${ref}"`);
}

export async function deleteSession(c: Client, name: string): Promise<void> {
  await c.raw.deleteSession({ name });
}

export async function updateSession(c: Client, s: Session, ...paths: string[]): Promise<Session> {
  return c.raw.updateSession({ session: s, updateMask: create(FieldMaskSchema, { paths }) });
}

export async function parkSession(c: Client, name: string): Promise<Session> {
  return updateSession(c, create(SessionSchema, { name, parkRequest: Session_ParkRequest.PARK }), "park_request");
}

/** Marks the session in use; holdForMs > 0 keeps it running that long after detach. */
export async function heartbeatSession(c: Client, name: string, holdForMs = 0): Promise<Session> {
  return c.raw.heartbeatSession({ name, holdFor: holdForMs > 0 ? durationFromMs(holdForMs) : undefined });
}
