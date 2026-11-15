"""Short aliases for the generated protobuf modules.

They're generated from proto/sandbox/runtime/session/v1/session.proto in this repository: the subset of
the Chainguard Workspaces Sessions API the simple sandbox API uses (compatible with chainctl 0.2.398).
It isn't a published, supported API.
"""

from chainguard_sandbox.workspaces._gen.sandbox.runtime.session.v1 import session_pb2 as session
from chainguard_sandbox.workspaces._gen.sandbox.runtime.session.v1 import session_pb2_grpc as session_grpc

__all__ = ["session", "session_grpc"]
