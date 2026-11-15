// Turning Workspaces on for an organization: workspacesStatus() and enableWorkspaces().
//
// Workspaces has to be turned on for an organization before sandboxes can be created in it. These
// do what `chainctl develop init --parent ORG` does for an organization that already exists (accept
// the terms, then turn the feature on), without chainctl.
import { type TokenSource } from "../auth";
import { detectInstance } from "../orgs";
import { PlatformError, acceptTerms, enableSandbox, missingWorkspacesTerms, workspacesEntitlement, type TermsDocument } from "../setup";
import { TermsNotAcceptedError } from "./errors";

export type { TermsDocument } from "../setup";

/** An organization's Workspaces setup. */
export interface WorkspacesStatus {
  /** The organization ID the status is for. */
  organization: string;
  /** Whether Workspaces is turned on. */
  enabled: boolean;
  /** Required legal documents not accepted yet. */
  missingTerms: TermsDocument[];
  /** Whether sandboxes can be created: enabled, and no terms missing. */
  ready: boolean;
  /** The organization's plan ("Starter", ...). */
  plan: string;
  /** The plan's limits on sandboxes existing at once (0: not reported). */
  maxSandboxes: number;
  maxSandboxesPerUser: number;
}

/** Options for workspacesStatus and enableWorkspaces. */
export interface SetupOpts {
  /**
   * The organization ID. If unset, $CHAINCTL_SANDBOX_INSTANCE is used, and failing that the only
   * organization you belong to.
   */
  instance?: string;
  /** Mints tokens for the platform API (default: the saved login). */
  tokenSource?: TokenSource;
}

/**
 * Whether Workspaces is turned on for the organization, and which required terms it still has to
 * accept. Changes nothing.
 */
export async function workspacesStatus(opts: SetupOpts = {}): Promise<WorkspacesStatus> {
  const org = opts.instance || process.env.CHAINCTL_SANDBOX_INSTANCE || (await detectInstance(opts.tokenSource));
  const o = { tokenSource: opts.tokenSource };
  const ent = await workspacesEntitlement(org, o);
  const missingTerms = await missingWorkspacesTerms(org, o);
  return {
    organization: org,
    enabled: ent.active,
    missingTerms,
    ready: ent.active && missingTerms.length === 0,
    plan: ent.planName,
    maxSandboxes: ent.maxSandboxes,
    maxSandboxesPerUser: ent.maxSandboxesPerUser,
  };
}

/**
 * Turn Workspaces on for the organization, so sandboxes can be created in it. It accepts the terms
 * Workspaces is offered under, then turns the feature on with the plan's allowance. It needs the
 * owner role, and does nothing if everything is already in place. It also turns on Chainguard
 * Checks, which the same switch controls.
 *
 * `acceptTerms`: accept, on the organization's behalf, the legal documents it has not accepted yet.
 * Pass true only when the person running your program has read and agreed to them
 * (workspacesStatus() lists them, with URLs). Without it, an organization with unaccepted terms
 * rejects with TermsNotAcceptedError.
 */
export async function enableWorkspaces(opts: SetupOpts & { acceptTerms?: boolean } = {}): Promise<WorkspacesStatus> {
  const st = await workspacesStatus(opts);
  if (st.ready) return st;
  const org = st.organization;
  const o = { tokenSource: opts.tokenSource };
  const accept = async (docs: TermsDocument[]) => {
    if (!opts.acceptTerms) throw new TermsNotAcceptedError(docs);
    await acceptTerms(org, docs.map((d) => d.id), o);
  };
  if (st.missingTerms.length) await accept(st.missingTerms);
  if (!st.enabled) {
    try {
      await enableSandbox(org, o);
    } catch (e) {
      if (!(e instanceof PlatformError) || e.missingTerms.length === 0) throw e;
      // The switch also covers Checks, whose terms the service may ask for too.
      await accept(e.missingTerms);
      await enableSandbox(org, o);
    }
  }
  return workspacesStatus({ ...opts, instance: org });
}
