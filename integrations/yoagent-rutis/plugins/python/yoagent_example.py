"""A Python plugin for yoagent-rutis: a tool, a deny policy and an output
redactor, registered as one handler.

Load it as the rutis-loader row ``py:yoagent_example`` from a Python runtime
whose module directory is this one, with ``yoagent`` shared in the loader's
catalog. The end-to-end tests do (``tests/languages_test.rs``). The hooks
and their JSON shapes are those of ``../yoagent.d.ts``.
"""

import re

KEY = re.compile(r"\bsk-[\w-]+")


class Handler:
    """Every hook is an ``async def`` method; leave out the ones you do not need."""

    def __init__(self, denied):
        self.denied = set(denied)

    async def tools(self, run):
        return [
            {
                "name": "py_reverse",
                "description": "Reverse a text",
                "parameters": {
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"],
                },
            }
        ]

    async def call_tool(self, call):
        return call["args"].get("text", "")[::-1]

    async def before_tool(self, call):
        if call["tool"] in self.denied:
            return {"deny": f"`{call['tool']}` is disabled by the py-example plugin"}
        return None

    async def after_tool(self, call, output):
        if not KEY.search(output["text"]):
            return None
        return {"text": KEY.sub("[key]", output["text"])}


inject = ["yoagent"]
Config = {
    "type": "object",
    "properties": {"denied": {"type": "array", "items": {"type": "string"}}},
}


def apply(ctx, config):
    yoagent = ctx.use("yoagent")
    unregister = yoagent.register("py-example", Handler((config or {}).get("denied", ["rm"])))
    ctx.effect(unregister)
