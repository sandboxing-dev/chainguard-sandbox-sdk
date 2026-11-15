// Turning Workspaces on for an organization: workspacesStatus() and enableWorkspaces().
//
// Workspaces has to be turned on for an organization before sandboxes can be created in it. These
// do what `chainctl develop init --parent ORG` does for an organization that already exists (accept
// the terms, link a GitHub account, then turn the feature on), without chainctl.
import { type TokenSource } from "../auth";
import { detectInstance } from "../orgs";
import {
  ENABLE_TERMS,
  GITHUB_APP_INSTALL_URL,
  PlatformError,
  WORKSPACES_TERMS,
  acceptTerms,
  acceptedTerms,
  authorizeGitHub,
  beginGitHubOAuth,
  enableSandbox,
  linkGitHubOrganization,
  linkedGitHubOrganizations,
  termsDocument,
  workspacesEntitlement,
  type TermsDocument,
} from "../setup";
import { GitHubAppNotInstalledError, GitHubLinkRequiredError, TermsNotAcceptedError } from "./errors";

export type { TermsDocument } from "../setup";

/** An organization's Workspaces setup. */
export interface WorkspacesStatus {
  /** The organization ID the status is for. */
  organization: string;
  /** Whether Workspaces is turned on. */
  enabled: boolean;
  /** Required legal documents not accepted yet. While Workspaces is off these are all the documents turning it on needs. */
  missingTerms: TermsDocument[];
  /** GitHub organizations or users linked to the organization. Turning Workspaces on needs at least one. */
  githubAccounts: string[];
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
  // Once Workspaces is on, only its own terms matter. While it is off, list everything turning it
  // on will need, so it can all be agreed to up front.
  const have = new Set(await acceptedTerms(org, o));
  const missingTerms = (ent.active ? WORKSPACES_TERMS : ENABLE_TERMS).filter((id) => !have.has(id)).map(termsDocument);
  // A member who may not list the links still gets the rest of the status.
  const githubAccounts = await linkedGitHubOrganizations(org, o).catch(() => [] as string[]);
  return {
    organization: org,
    enabled: ent.active,
    missingTerms,
    githubAccounts,
    ready: ent.active && missingTerms.length === 0,
    plan: ent.planName,
    maxSandboxes: ent.maxSandboxes,
    maxSandboxesPerUser: ent.maxSandboxesPerUser,
  };
}

/** Options for enableWorkspaces. */
export interface EnableOpts extends SetupOpts {
  /**
   * Accept, on the organization's behalf, the legal documents it has not accepted yet. Pass true
   * only when the person running your program has read and agreed to them (workspacesStatus() lists
   * them, with URLs). Without it, an organization with unaccepted terms rejects with TermsNotAcceptedError.
   */
  acceptTerms?: boolean;
  /** The GitHub organization or user to link (default: the GitHub user who authorizes in the browser). */
  githubAccount?: string;
  /** The loopback port for GitHub's redirect (default 8989). */
  githubPort?: number;
  /** false: don't start the GitHub authorization; reject with GitHubLinkRequiredError if a link is needed. */
  openBrowser?: boolean;
  /** Opens the GitHub page instead of the default browser launcher. */
  openUrl?: (url: string) => boolean | void | Promise<boolean | void>;
  /** Receives instructions during the GitHub authorization (default: written to stderr). */
  out?: (message: string) => void;
}

/**
 * Turn Workspaces on for the organization, so sandboxes can be created in it. It needs the owner
 * role, and does nothing if everything is already in place.
 *
 * 1. Accept the terms (only with `acceptTerms: true`).
 * 2. Link a GitHub account, if the organization has none: the service requires one. This opens a
 *    browser on GitHub, where you choose the account, and waits for you to finish (up to 10
 *    minutes). The Chainguard GitHub App must be installed on the account; if it is not,
 *    GitHubAppNotInstalledError gives the page to install it from.
 * 3. Turn the feature on with the plan's allowance. This also turns on Chainguard Checks, which the
 *    same switch controls.
 */
export async function enableWorkspaces(opts: EnableOpts = {}): Promise<WorkspacesStatus> {
  const st = await workspacesStatus(opts);
  if (st.ready) return st;
  const org = st.organization;
  const o = { tokenSource: opts.tokenSource };
  const accept = async (docs: TermsDocument[]) => {
    if (!opts.acceptTerms) throw new TermsNotAcceptedError(docs);
    await acceptTerms(org, docs.map((d) => d.id), o);
  };
  const linkGitHub = async () => {
    if (opts.openBrowser === false) {
      throw new GitHubLinkRequiredError(`organization ${org} has to link a GitHub account before Workspaces can be turned on; run enableWorkspaces() where it can open a browser, or \`chainguard-sandbox enable\``);
    }
    const start = await beginGitHubOAuth(o);
    const auth = await authorizeGitHub(start, { port: opts.githubPort, openUrl: opts.openUrl, out: opts.out });
    try {
      await linkGitHubOrganization(org, auth, opts.githubAccount, o);
    } catch (e) {
      if (e instanceof PlatformError && e.githubAppNotInstalled) throw new GitHubAppNotInstalledError(GITHUB_APP_INSTALL_URL, e.detail);
      throw e;
    }
  };
  if (st.missingTerms.length) await accept(st.missingTerms);
  if (!st.enabled) {
    if (st.githubAccounts.length === 0) await linkGitHub();
    try {
      await enableSandbox(org, o);
    } catch (e) {
      if (!(e instanceof PlatformError)) throw e;
      if (e.githubLinkRequired && st.githubAccounts.length > 0) await linkGitHub(); // the link could not be listed, or was removed meanwhile
      else if (e.missingTerms.length) await accept(e.missingTerms); // a document this package doesn't know about yet
      else throw e;
      await enableSandbox(org, o);
    }
  }
  return workspacesStatus({ ...opts, instance: org });
}
