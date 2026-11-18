"""Run an OpenAI Agents SDK sandbox agent on Chainguard Workspaces, without a model API call.

A scripted model stands in for the LLM: it asks for one shell command, then answers. The command
really runs in a Chainguard Workspaces sandbox, which the runner creates and deletes.

    uv run python examples/demo.py

Needs a Chainguard login (`chainguard-sandbox login`, from the chainguard-sandbox-sdk package) and an
organization with Workspaces turned on (`chainguard-sandbox enable`). Choose the organization with
$CHAINCTL_SANDBOX_INSTANCE=XXXXXX if you belong to more than one.
"""

# --8<-- [start:demo]
import asyncio

from agents import RunConfig, Runner
from agents.sandbox import Manifest, SandboxAgent, SandboxRunConfig
from agents.sandbox.capabilities import Shell
from agents.sandbox.entries import File
from agents.testing import ScriptedModel, assistant_message, function_call

from chainguard_sandbox_openai_agents import ChainguardSandboxClient, ChainguardSandboxClientOptions


async def main() -> None:
    model = ScriptedModel(
        [
            [
                function_call(
                    "exec_command",
                    {"cmd": "cat notes/task.txt && python3 -c 'import platform; print(\"python\", platform.python_version())'"},
                    call_id="demo-command",
                )
            ],
            [assistant_message("Done.")],
        ]
    )
    agent = SandboxAgent(
        name="Chainguard demo",
        model=model,
        capabilities=[Shell()],
        # Files the agent starts with, written into the sandbox's /work.
        default_manifest=Manifest(root="/work", entries={"notes/task.txt": File(content=b"hello from the manifest\n")}),
    )
    async with ChainguardSandboxClient() as client:
        result = await Runner.run(
            agent,
            "Run the command.",
            run_config=RunConfig(
                tracing_disabled=True,
                sandbox=SandboxRunConfig(
                    client=client,
                    options=ChainguardSandboxClientOptions(packages=["bash", "coreutils", "python-3.13"]),
                ),
            ),
        )
    model.assert_complete()
    # The tool output is what ran in the sandbox.
    outputs = [str(item.output) for item in result.new_items if item.type == "tool_call_output_item"]
    print("\n".join(outputs))
    if not any("hello from the manifest" in o and "python 3.13" in o for o in outputs):
        raise SystemExit(f"the sandbox command did not produce the expected output: {outputs!r}")
    print("Chainguard Workspaces sandbox provider works")


if __name__ == "__main__":
    asyncio.run(main())
# --8<-- [end:demo]
