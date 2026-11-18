"""Against the real Chainguard Workspaces service, opt in:

    CHAINGUARD_SANDBOX_LIVE=1 uv run pytest -q tests/test_live.py

Needs a Chainguard login and an organization with Workspaces on. The Starter plan allows one
sandbox per user, so the tests create one at a time, and delete each one.
"""

import io
import os
from typing import cast
from pathlib import Path

import pytest
from agents import RunConfig, Runner
from agents.sandbox import Manifest, SandboxAgent, SandboxRunConfig
from agents.sandbox.capabilities import Shell
from agents.sandbox.entries import File
from agents.sandbox.errors import ExecTimeoutError, InvalidManifestPathError, WorkspaceReadNotFoundError
from agents.testing import ScriptedModel, assistant_message, function_call
from chainguard_sandbox import Sandbox

from chainguard_sandbox_openai_agents import (
    ChainguardSandboxClient,
    ChainguardSandboxClientOptions,
    ChainguardSandboxSessionState,
)

def session_name(s) -> str:
    return cast(ChainguardSandboxSessionState, s.state).session_name


pytestmark = pytest.mark.skipif(os.environ.get("CHAINGUARD_SANDBOX_LIVE") != "1", reason="set CHAINGUARD_SANDBOX_LIVE=1")

OPTS = ChainguardSandboxClientOptions(packages=["bash", "coreutils"], name="agents-test")


async def test_runner_runs_a_tool_call_in_a_sandbox_and_deletes_it():
    model = ScriptedModel([
        [function_call("exec_command", {"cmd": "cat notes/task.txt; echo uname=$(uname -s)"}, call_id="c1")],
        [assistant_message("Done.")],
    ])
    agent = SandboxAgent(name="t", model=model, capabilities=[Shell()],
                         default_manifest=Manifest(root="/work", entries={"notes/task.txt": File(content=b"from the manifest\n")}))
    before = {i.sandbox_id for i in Sandbox.list()}
    async with ChainguardSandboxClient() as client:
        result = await Runner.run(agent, "go", run_config=RunConfig(
            tracing_disabled=True, sandbox=SandboxRunConfig(client=client, options=OPTS)))
    model.assert_complete()
    out = " ".join(str(i.output) for i in result.new_items if i.type == "tool_call_output_item")
    assert "from the manifest" in out and "uname=Linux" in out, out
    assert {i.sandbox_id for i in Sandbox.list()} <= before, "the runner deleted its sandbox"


async def test_session_operations_snapshot_and_resume():
    async with ChainguardSandboxClient() as client:
        # 1. A session: commands, files, refusals, then a workspace snapshot.
        s = await client.create(options=ChainguardSandboxClientOptions(packages=["bash", "coreutils"], env_vars={"MODE": "test"}))
        try:
            await s.start()
            assert await s.running()
            r = await s.exec("echo $MODE && pwd")
            assert r.stdout.decode().split() == ["test", "/work"], r
            await s.write(Path("data/in.txt"), io.BytesIO(b"\x00binary\xff"))
            assert (await s.read(Path("data/in.txt"))).read() == b"\x00binary\xff"
            with pytest.raises(WorkspaceReadNotFoundError):
                await s.read(Path("missing.txt"))
            with pytest.raises(InvalidManifestPathError):
                await s.read(Path("../etc/passwd"))
            with pytest.raises(ExecTimeoutError):
                await s.exec("sleep 10", timeout=2)
            snapshot = (await s.persist_workspace()).read()
            assert len(snapshot) > 0
        finally:
            await s.shutdown()

        # 2. A new sandbox restored from that snapshot.
        s2 = await client.create(options=OPTS)
        try:
            await s2.start()
            await s2.hydrate_workspace(io.BytesIO(snapshot))
            assert (await s2.read(Path("data/in.txt"))).read() == b"\x00binary\xff"
            # 3. Resume from saved state, in another client: the same sandbox, the workspace kept.
            payload = client.serialize_session_state(s2.state)
            async with ChainguardSandboxClient() as other:
                resumed = await other.resume(other.deserialize_session_state(payload))
                await resumed.start()
                assert session_name(resumed) == session_name(s2)
                assert (await resumed.read(Path("data/in.txt"))).read() == b"\x00binary\xff"
        finally:
            await s2.shutdown()
        assert not await s2.running()


async def test_pause_on_exit_keeps_the_sandbox_for_resume():
    async with ChainguardSandboxClient() as client:
        s = await client.create(options=ChainguardSandboxClientOptions(packages=["bash", "coreutils"], pause_on_exit=True))
        name = session_name(s)
        try:
            await s.start()
            await s.write(Path("kept.txt"), io.BytesIO(b"still here"))
            await s.shutdown()  # pauses instead of deleting
            payload = client.serialize_session_state(s.state)
            resumed = await client.resume(client.deserialize_session_state(payload))
            await resumed.start()
            assert session_name(resumed) == name
            assert (await resumed.read(Path("kept.txt"))).read() == b"still here"
        finally:
            Sandbox.kill(name)
