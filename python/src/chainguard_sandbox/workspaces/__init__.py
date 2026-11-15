"""chainguard_sandbox.workspaces: the lower-level client behind the Simple Chainguard Sandbox SDK.

It talks to the Chainguard Workspaces Sessions API
(chainguard.sandbox.runtime.session.v1.Sessions at sessions.chainbox.dev), using only the calls the
simple sandbox API needs. Most programs only need the top-level chainguard_sandbox package.

Unofficial: not affiliated with or supported by Chainguard. Compatible with chainctl 0.2.398.
"""

from chainguard_sandbox.workspaces.auth import (
    Cached,
    ChainctlToken,
    IdentityExchange,
    StaticToken,
    TokenSource,
    default_token_source,
    env_id_token,
    file_id_token,
    github_actions_id_token,
    jwt_expiry,
)
from chainguard_sandbox.workspaces.client import AmbiguousError, AuthTimeoutError, Client, NotFoundError
from chainguard_sandbox.workspaces.exec import ExecResult, ExitError, OutcomeUnknownError, is_guest_held
from chainguard_sandbox.workspaces.files import tar_dir, tar_gz_dir
from chainguard_sandbox.workspaces.first_attach import FirstAttachError, complete_first_attach
from chainguard_sandbox.workspaces.login import (
    PLATFORM_AUDIENCE,
    AccountNotFoundError,
    BrowserLoginToken,
    FileLoginStore,
    LoginExpiredError,
    LoginRequiredError,
    LoginResult,
    browser_login,
    default_login_audiences,
    default_login_file,
    logout,
    refresh_login,
    token_claims,
)
from chainguard_sandbox.workspaces.orgs import (
    AmbiguousOrganizationError,
    NoOrganizationError,
    Organization,
    detect_instance,
    list_organizations,
    platform_token_source,
)
from chainguard_sandbox.workspaces.sessions import (
    DEFAULT_ENVIRONMENT,
    DEFAULT_JOB,
    instance_name,
    is_session_name,
    normalize_github_url,
    repository_session,
    repository_url,
    workspace_session,
)

__all__ = [
    "AccountNotFoundError", "AmbiguousError", "AmbiguousOrganizationError", "AuthTimeoutError", "BrowserLoginToken",
    "Cached", "ChainctlToken", "Client", "DEFAULT_ENVIRONMENT", "DEFAULT_JOB", "ExecResult", "ExitError",
    "FileLoginStore", "FirstAttachError", "IdentityExchange", "LoginExpiredError", "LoginRequiredError", "LoginResult",
    "NoOrganizationError", "NotFoundError", "Organization", "OutcomeUnknownError", "PLATFORM_AUDIENCE", "StaticToken",
    "TokenSource", "browser_login", "complete_first_attach", "default_login_audiences", "default_login_file",
    "default_token_source", "detect_instance", "env_id_token", "file_id_token", "github_actions_id_token",
    "instance_name", "is_guest_held", "is_session_name", "jwt_expiry", "list_organizations", "logout",
    "normalize_github_url", "platform_token_source", "refresh_login", "repository_session", "repository_url",
    "tar_dir", "tar_gz_dir", "token_claims", "workspace_session",
]
