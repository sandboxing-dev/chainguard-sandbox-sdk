// Timings (ms) for the sandbox lifecycle, in one object so tests can shorten them.
export const timings = {
  /** How long "starting guest failed" may show before giving up on a host. */
  placementGraceMs: 30_000,
  /** Between GetSession calls while a sandbox starts. */
  startPollMs: 1000,
  /** How long kill waits for the session to disappear. */
  killWaitMs: 30_000,
  /** Between GetSession calls while a sandbox is being deleted. */
  killPollMs: 500,
};
