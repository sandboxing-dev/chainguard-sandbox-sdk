// Connection to the Sessions service. Mirrors the Go SDK's workspaces/client.go.
import { createClient, type Client as ConnectClient, type Interceptor, type Transport } from "@connectrpc/connect";
import { createGrpcTransport } from "@connectrpc/connect-node";
import { Sessions } from "./gen/sandbox/runtime/session/v1/session_pb.js";
import { defaultTokenSource, ENV_AUTH_TOKEN, type TokenSource } from "./auth.js";
import { detectInstance, ENV_NO_DETECT_INSTANCE, noInstanceMessage, platformTokenSource } from "./orgs.js";

export const DEFAULT_URL = "https://sessions.chainbox.dev";
export const DEFAULT_WORK_DIR = "/work";
export const MAX_EXEC_STDIN = 16 << 20;
export const ENV_SESSIONS_URL = "CHAINCTL_SESSIONS_URL";
export const ENV_INSTANCE = "CHAINCTL_SANDBOX_INSTANCE";

export type SessionsClient = ConnectClient<typeof Sessions>;

export interface Options {
  /** Sandbox plane URL; defaults to $CHAINCTL_SESSIONS_URL, then DEFAULT_URL. http:// is plaintext (tests). */
  url?: string;
  /** Instance ID, used as "instances/{instance}" (your organization ID); defaults to $CHAINCTL_SANDBOX_INSTANCE. */
  instance?: string;
  /** Defaults to defaultTokenSource(). */
  tokenSource?: TokenSource;
  userAgent?: string;
  /** HTTP/2 keepalive ping interval (ms) for long attaches through NATs/proxies. */
  pingIntervalMs?: number;
  /**
   * Auto-detect the organization when neither `instance` nor $CHAINCTL_SANDBOX_INSTANCE is set
   * (default true; also off with $CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1). It lists your organizations on
   * first use and takes the only one.
   */
  detectInstance?: boolean;
  /** Account-API tokens for detection. Defaults to platformTokenSource() when tokenSource is unset and $CHAINCTL_AUTH_TOKEN is empty. */
  platformTokenSource?: TokenSource;
  /** Use a prebuilt transport instead (it must add its own credentials). */
  transport?: Transport;
}

export function instanceName(id: string): string {
  return id.startsWith("instances/") ? id : `instances/${id}`;
}

export class Client {
  readonly raw: SessionsClient;
  private instanceId: string;
  private detecting?: Promise<string>;
  private detectError?: string;
  private readonly platformTs?: TokenSource;

  constructor(opts: Options = {}) {
    let url = opts.url || process.env[ENV_SESSIONS_URL] || DEFAULT_URL;
    if (!url.includes("://")) url = `https://${url}`;
    const u = new URL(url);
    if (u.protocol !== "https:" && u.protocol !== "http:") throw new Error(`sessions URL ${url}: unsupported scheme ${u.protocol}`);
    this.instanceId = opts.instance || process.env[ENV_INSTANCE] || "";
    if (!this.instanceId) {
      if (opts.detectInstance === false || process.env[ENV_NO_DETECT_INSTANCE] === "1") this.detectError = "turned off";
      else if (opts.platformTokenSource) this.platformTs = opts.platformTokenSource;
      else if (!opts.tokenSource && !opts.transport && !process.env[ENV_AUTH_TOKEN]) this.platformTs = platformTokenSource();
      else this.detectError = "needs platformTokenSource when a custom token source or $CHAINCTL_AUTH_TOKEN is used";
    }
    let transport = opts.transport;
    if (!transport) {
      const ts = opts.tokenSource ?? defaultTokenSource();
      const ua = opts.userAgent ?? "chainguard-sandbox-sdk-ts";
      const auth: Interceptor = (next) => async (req) => {
        req.header.set("authorization", `Bearer ${await ts.token(req.signal)}`);
        req.header.set("user-agent", ua);
        return next(req);
      };
      transport = createGrpcTransport({
        baseUrl: `${u.protocol}//${u.host}`,
        interceptors: [auth],
        pingIntervalMs: opts.pingIntervalMs,
      });
    }
    this.raw = createClient(Sessions, transport);
  }

  /** "instances/{id}" used as the parent of sessions. */
  get instance(): string {
    return instanceName(this.instanceId);
  }

  /** @internal */
  /** "instances/<org>" if the organization is known (explicit, environment, or already detected). */
  parent(): string {
    if (!this.instanceId) throw new Error(noInstanceMessage(this.detectError ?? "not attempted yet (use parentName())"));
    return instanceName(this.instanceId);
  }

  /** "instances/<org>", auto-detecting the organization on first use if needed. */
  async parentName(signal?: AbortSignal): Promise<string> {
    if (!this.instanceId && this.platformTs && !this.detectError) {
      this.detecting ??= detectInstance(this.platformTs, signal);
      try {
        this.instanceId = await this.detecting;
      } catch (e) {
        this.detectError = e instanceof Error ? e.message : String(e);
      }
    }
    if (!this.instanceId) throw new Error(noInstanceMessage(this.detectError));
    return instanceName(this.instanceId);
  }
}
