import datetime

from google.protobuf import duration_pb2 as _duration_pb2
from google.protobuf import empty_pb2 as _empty_pb2
from google.protobuf import field_mask_pb2 as _field_mask_pb2
from google.protobuf import timestamp_pb2 as _timestamp_pb2
from google.protobuf.internal import containers as _containers
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class Phase(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    PHASE_UNSPECIFIED: _ClassVar[Phase]
    CREATING: _ClassVar[Phase]
    ATTACHED: _ClassVar[Phase]
    IDLE: _ClassVar[Phase]
    PARKED: _ClassVar[Phase]
    REAPED: _ClassVar[Phase]

class IdlePolicy(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    IDLE_POLICY_UNSPECIFIED: _ClassVar[IdlePolicy]
    PARK_WHEN_IDLE: _ClassVar[IdlePolicy]
    KEEP_RUNNING: _ClassVar[IdlePolicy]
    AUTO_HOLD: _ClassVar[IdlePolicy]

class Provenance(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    PROVENANCE_UNSPECIFIED: _ClassVar[Provenance]
    AUTHORED: _ClassVar[Provenance]
    MIGRATED: _ClassVar[Provenance]
    SYNTHESIZED: _ClassVar[Provenance]
PHASE_UNSPECIFIED: Phase
CREATING: Phase
ATTACHED: Phase
IDLE: Phase
PARKED: Phase
REAPED: Phase
IDLE_POLICY_UNSPECIFIED: IdlePolicy
PARK_WHEN_IDLE: IdlePolicy
KEEP_RUNNING: IdlePolicy
AUTO_HOLD: IdlePolicy
PROVENANCE_UNSPECIFIED: Provenance
AUTHORED: Provenance
MIGRATED: Provenance
SYNTHESIZED: Provenance

class Session(_message.Message):
    __slots__ = ("name", "display_name", "short_id", "owner", "create_time", "source", "workspace", "phase", "park_after", "expire_time", "message", "guest_started_time", "workspace_pending", "held_until", "idle", "region", "max_resource_class", "park_request")
    class ParkRequest(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
        __slots__ = ()
        PARK_REQUEST_UNSPECIFIED: _ClassVar[Session.ParkRequest]
        PARK_REQUEST_PARK: _ClassVar[Session.ParkRequest]
        PARK_REQUEST_RESTART: _ClassVar[Session.ParkRequest]
    PARK_REQUEST_UNSPECIFIED: Session.ParkRequest
    PARK_REQUEST_PARK: Session.ParkRequest
    PARK_REQUEST_RESTART: Session.ParkRequest
    NAME_FIELD_NUMBER: _ClassVar[int]
    DISPLAY_NAME_FIELD_NUMBER: _ClassVar[int]
    SHORT_ID_FIELD_NUMBER: _ClassVar[int]
    OWNER_FIELD_NUMBER: _ClassVar[int]
    CREATE_TIME_FIELD_NUMBER: _ClassVar[int]
    SOURCE_FIELD_NUMBER: _ClassVar[int]
    WORKSPACE_FIELD_NUMBER: _ClassVar[int]
    PHASE_FIELD_NUMBER: _ClassVar[int]
    PARK_AFTER_FIELD_NUMBER: _ClassVar[int]
    EXPIRE_TIME_FIELD_NUMBER: _ClassVar[int]
    MESSAGE_FIELD_NUMBER: _ClassVar[int]
    GUEST_STARTED_TIME_FIELD_NUMBER: _ClassVar[int]
    WORKSPACE_PENDING_FIELD_NUMBER: _ClassVar[int]
    HELD_UNTIL_FIELD_NUMBER: _ClassVar[int]
    IDLE_FIELD_NUMBER: _ClassVar[int]
    REGION_FIELD_NUMBER: _ClassVar[int]
    MAX_RESOURCE_CLASS_FIELD_NUMBER: _ClassVar[int]
    PARK_REQUEST_FIELD_NUMBER: _ClassVar[int]
    name: str
    display_name: str
    short_id: str
    owner: str
    create_time: _timestamp_pb2.Timestamp
    source: Source
    workspace: Workspace
    phase: Phase
    park_after: _timestamp_pb2.Timestamp
    expire_time: _timestamp_pb2.Timestamp
    message: str
    guest_started_time: _timestamp_pb2.Timestamp
    workspace_pending: bool
    held_until: _timestamp_pb2.Timestamp
    idle: IdlePolicy
    region: str
    max_resource_class: SessionResourceLimit
    park_request: Session.ParkRequest
    def __init__(self, name: _Optional[str] = ..., display_name: _Optional[str] = ..., short_id: _Optional[str] = ..., owner: _Optional[str] = ..., create_time: _Optional[_Union[datetime.datetime, _timestamp_pb2.Timestamp, _Mapping]] = ..., source: _Optional[_Union[Source, _Mapping]] = ..., workspace: _Optional[_Union[Workspace, _Mapping]] = ..., phase: _Optional[_Union[Phase, str]] = ..., park_after: _Optional[_Union[datetime.datetime, _timestamp_pb2.Timestamp, _Mapping]] = ..., expire_time: _Optional[_Union[datetime.datetime, _timestamp_pb2.Timestamp, _Mapping]] = ..., message: _Optional[str] = ..., guest_started_time: _Optional[_Union[datetime.datetime, _timestamp_pb2.Timestamp, _Mapping]] = ..., workspace_pending: _Optional[bool] = ..., held_until: _Optional[_Union[datetime.datetime, _timestamp_pb2.Timestamp, _Mapping]] = ..., idle: _Optional[_Union[IdlePolicy, str]] = ..., region: _Optional[str] = ..., max_resource_class: _Optional[_Union[SessionResourceLimit, _Mapping]] = ..., park_request: _Optional[_Union[Session.ParkRequest, str]] = ...) -> None: ...

class SessionResourceLimit(_message.Message):
    __slots__ = ("cpus", "memory_mib", "disk_mib")
    CPUS_FIELD_NUMBER: _ClassVar[int]
    MEMORY_MIB_FIELD_NUMBER: _ClassVar[int]
    DISK_MIB_FIELD_NUMBER: _ClassVar[int]
    cpus: int
    memory_mib: int
    disk_mib: int
    def __init__(self, cpus: _Optional[int] = ..., memory_mib: _Optional[int] = ..., disk_mib: _Optional[int] = ...) -> None: ...

class Workspace(_message.Message):
    __slots__ = ("label", "repository", "revision", "document", "provenance")
    LABEL_FIELD_NUMBER: _ClassVar[int]
    REPOSITORY_FIELD_NUMBER: _ClassVar[int]
    REVISION_FIELD_NUMBER: _ClassVar[int]
    DOCUMENT_FIELD_NUMBER: _ClassVar[int]
    PROVENANCE_FIELD_NUMBER: _ClassVar[int]
    label: str
    repository: str
    revision: str
    document: bytes
    provenance: Provenance
    def __init__(self, label: _Optional[str] = ..., repository: _Optional[str] = ..., revision: _Optional[str] = ..., document: _Optional[bytes] = ..., provenance: _Optional[_Union[Provenance, str]] = ...) -> None: ...

class Source(_message.Message):
    __slots__ = ("repository", "revision", "fallback_document", "fallback_used")
    REPOSITORY_FIELD_NUMBER: _ClassVar[int]
    REVISION_FIELD_NUMBER: _ClassVar[int]
    FALLBACK_DOCUMENT_FIELD_NUMBER: _ClassVar[int]
    FALLBACK_USED_FIELD_NUMBER: _ClassVar[int]
    repository: str
    revision: str
    fallback_document: bytes
    fallback_used: bool
    def __init__(self, repository: _Optional[str] = ..., revision: _Optional[str] = ..., fallback_document: _Optional[bytes] = ..., fallback_used: _Optional[bool] = ...) -> None: ...

class CreateSessionRequest(_message.Message):
    __slots__ = ("parent", "session")
    PARENT_FIELD_NUMBER: _ClassVar[int]
    SESSION_FIELD_NUMBER: _ClassVar[int]
    parent: str
    session: Session
    def __init__(self, parent: _Optional[str] = ..., session: _Optional[_Union[Session, _Mapping]] = ...) -> None: ...

class GetSessionRequest(_message.Message):
    __slots__ = ("name",)
    NAME_FIELD_NUMBER: _ClassVar[int]
    name: str
    def __init__(self, name: _Optional[str] = ...) -> None: ...

class ListSessionsRequest(_message.Message):
    __slots__ = ("parent", "page_size", "page_token", "owner")
    PARENT_FIELD_NUMBER: _ClassVar[int]
    PAGE_SIZE_FIELD_NUMBER: _ClassVar[int]
    PAGE_TOKEN_FIELD_NUMBER: _ClassVar[int]
    OWNER_FIELD_NUMBER: _ClassVar[int]
    parent: str
    page_size: int
    page_token: str
    owner: str
    def __init__(self, parent: _Optional[str] = ..., page_size: _Optional[int] = ..., page_token: _Optional[str] = ..., owner: _Optional[str] = ...) -> None: ...

class ListSessionsResponse(_message.Message):
    __slots__ = ("sessions", "next_page_token")
    SESSIONS_FIELD_NUMBER: _ClassVar[int]
    NEXT_PAGE_TOKEN_FIELD_NUMBER: _ClassVar[int]
    sessions: _containers.RepeatedCompositeFieldContainer[Session]
    next_page_token: str
    def __init__(self, sessions: _Optional[_Iterable[_Union[Session, _Mapping]]] = ..., next_page_token: _Optional[str] = ...) -> None: ...

class UpdateSessionRequest(_message.Message):
    __slots__ = ("session", "update_mask")
    SESSION_FIELD_NUMBER: _ClassVar[int]
    UPDATE_MASK_FIELD_NUMBER: _ClassVar[int]
    session: Session
    update_mask: _field_mask_pb2.FieldMask
    def __init__(self, session: _Optional[_Union[Session, _Mapping]] = ..., update_mask: _Optional[_Union[_field_mask_pb2.FieldMask, _Mapping]] = ...) -> None: ...

class DeleteSessionRequest(_message.Message):
    __slots__ = ("name",)
    NAME_FIELD_NUMBER: _ClassVar[int]
    name: str
    def __init__(self, name: _Optional[str] = ...) -> None: ...

class HeartbeatSessionRequest(_message.Message):
    __slots__ = ("name", "hold_for")
    NAME_FIELD_NUMBER: _ClassVar[int]
    HOLD_FOR_FIELD_NUMBER: _ClassVar[int]
    name: str
    hold_for: _duration_pb2.Duration
    def __init__(self, name: _Optional[str] = ..., hold_for: _Optional[_Union[datetime.timedelta, _duration_pb2.Duration, _Mapping]] = ...) -> None: ...

class ExecRequest(_message.Message):
    __slots__ = ("name", "argv", "stdin", "deadline")
    NAME_FIELD_NUMBER: _ClassVar[int]
    ARGV_FIELD_NUMBER: _ClassVar[int]
    STDIN_FIELD_NUMBER: _ClassVar[int]
    DEADLINE_FIELD_NUMBER: _ClassVar[int]
    name: str
    argv: _containers.RepeatedScalarFieldContainer[str]
    stdin: bytes
    deadline: _duration_pb2.Duration
    def __init__(self, name: _Optional[str] = ..., argv: _Optional[_Iterable[str]] = ..., stdin: _Optional[bytes] = ..., deadline: _Optional[_Union[datetime.timedelta, _duration_pb2.Duration, _Mapping]] = ...) -> None: ...

class ExecResponse(_message.Message):
    __slots__ = ("stdout", "stderr", "exited")
    class Exited(_message.Message):
        __slots__ = ("code", "timed_out")
        CODE_FIELD_NUMBER: _ClassVar[int]
        TIMED_OUT_FIELD_NUMBER: _ClassVar[int]
        code: int
        timed_out: bool
        def __init__(self, code: _Optional[int] = ..., timed_out: _Optional[bool] = ...) -> None: ...
    STDOUT_FIELD_NUMBER: _ClassVar[int]
    STDERR_FIELD_NUMBER: _ClassVar[int]
    EXITED_FIELD_NUMBER: _ClassVar[int]
    stdout: bytes
    stderr: bytes
    exited: ExecResponse.Exited
    def __init__(self, stdout: _Optional[bytes] = ..., stderr: _Optional[bytes] = ..., exited: _Optional[_Union[ExecResponse.Exited, _Mapping]] = ...) -> None: ...

class AttachRequest(_message.Message):
    __slots__ = ("open", "workspace_chunk")
    class Open(_message.Message):
        __slots__ = ("name", "resume")
        NAME_FIELD_NUMBER: _ClassVar[int]
        RESUME_FIELD_NUMBER: _ClassVar[int]
        name: str
        resume: AttachRequest.Resume
        def __init__(self, name: _Optional[str] = ..., resume: _Optional[_Union[AttachRequest.Resume, _Mapping]] = ...) -> None: ...
    class Resume(_message.Message):
        __slots__ = ("incarnation", "offset", "pty_id")
        INCARNATION_FIELD_NUMBER: _ClassVar[int]
        OFFSET_FIELD_NUMBER: _ClassVar[int]
        PTY_ID_FIELD_NUMBER: _ClassVar[int]
        incarnation: int
        offset: int
        pty_id: int
        def __init__(self, incarnation: _Optional[int] = ..., offset: _Optional[int] = ..., pty_id: _Optional[int] = ...) -> None: ...
    class WorkspaceChunk(_message.Message):
        __slots__ = ("data", "last", "format", "encoding")
        class Format(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
            __slots__ = ()
            FORMAT_UNSPECIFIED: _ClassVar[AttachRequest.WorkspaceChunk.Format]
            FULL_TAR: _ClassVar[AttachRequest.WorkspaceChunk.Format]
            DELTA_TAR: _ClassVar[AttachRequest.WorkspaceChunk.Format]
        FORMAT_UNSPECIFIED: AttachRequest.WorkspaceChunk.Format
        FULL_TAR: AttachRequest.WorkspaceChunk.Format
        DELTA_TAR: AttachRequest.WorkspaceChunk.Format
        class Encoding(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
            __slots__ = ()
            ENCODING_UNSPECIFIED: _ClassVar[AttachRequest.WorkspaceChunk.Encoding]
            GZIP: _ClassVar[AttachRequest.WorkspaceChunk.Encoding]
        ENCODING_UNSPECIFIED: AttachRequest.WorkspaceChunk.Encoding
        GZIP: AttachRequest.WorkspaceChunk.Encoding
        DATA_FIELD_NUMBER: _ClassVar[int]
        LAST_FIELD_NUMBER: _ClassVar[int]
        FORMAT_FIELD_NUMBER: _ClassVar[int]
        ENCODING_FIELD_NUMBER: _ClassVar[int]
        data: bytes
        last: bool
        format: AttachRequest.WorkspaceChunk.Format
        encoding: AttachRequest.WorkspaceChunk.Encoding
        def __init__(self, data: _Optional[bytes] = ..., last: _Optional[bool] = ..., format: _Optional[_Union[AttachRequest.WorkspaceChunk.Format, str]] = ..., encoding: _Optional[_Union[AttachRequest.WorkspaceChunk.Encoding, str]] = ...) -> None: ...
    OPEN_FIELD_NUMBER: _ClassVar[int]
    WORKSPACE_CHUNK_FIELD_NUMBER: _ClassVar[int]
    open: AttachRequest.Open
    workspace_chunk: AttachRequest.WorkspaceChunk
    def __init__(self, open: _Optional[_Union[AttachRequest.Open, _Mapping]] = ..., workspace_chunk: _Optional[_Union[AttachRequest.WorkspaceChunk, _Mapping]] = ...) -> None: ...

class AttachResponse(_message.Message):
    __slots__ = ("detached", "opened", "workspace_ready")
    class Opened(_message.Message):
        __slots__ = ("rows", "cols", "session_name", "workspace_pending", "workspace_encodings")
        ROWS_FIELD_NUMBER: _ClassVar[int]
        COLS_FIELD_NUMBER: _ClassVar[int]
        SESSION_NAME_FIELD_NUMBER: _ClassVar[int]
        WORKSPACE_PENDING_FIELD_NUMBER: _ClassVar[int]
        WORKSPACE_ENCODINGS_FIELD_NUMBER: _ClassVar[int]
        rows: int
        cols: int
        session_name: str
        workspace_pending: bool
        workspace_encodings: _containers.RepeatedScalarFieldContainer[AttachRequest.WorkspaceChunk.Encoding]
        def __init__(self, rows: _Optional[int] = ..., cols: _Optional[int] = ..., session_name: _Optional[str] = ..., workspace_pending: _Optional[bool] = ..., workspace_encodings: _Optional[_Iterable[_Union[AttachRequest.WorkspaceChunk.Encoding, str]]] = ...) -> None: ...
    class WorkspaceReady(_message.Message):
        __slots__ = ("bytes", "elapsed")
        BYTES_FIELD_NUMBER: _ClassVar[int]
        ELAPSED_FIELD_NUMBER: _ClassVar[int]
        bytes: int
        elapsed: _duration_pb2.Duration
        def __init__(self, bytes: _Optional[int] = ..., elapsed: _Optional[_Union[datetime.timedelta, _duration_pb2.Duration, _Mapping]] = ...) -> None: ...
    class Detached(_message.Message):
        __slots__ = ()
        def __init__(self) -> None: ...
    DETACHED_FIELD_NUMBER: _ClassVar[int]
    OPENED_FIELD_NUMBER: _ClassVar[int]
    WORKSPACE_READY_FIELD_NUMBER: _ClassVar[int]
    detached: AttachResponse.Detached
    opened: AttachResponse.Opened
    workspace_ready: AttachResponse.WorkspaceReady
    def __init__(self, detached: _Optional[_Union[AttachResponse.Detached, _Mapping]] = ..., opened: _Optional[_Union[AttachResponse.Opened, _Mapping]] = ..., workspace_ready: _Optional[_Union[AttachResponse.WorkspaceReady, _Mapping]] = ...) -> None: ...
