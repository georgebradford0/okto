"""Worker pool — one Claude subagent per branch/worktree."""

from __future__ import annotations

import asyncio
from dataclasses import dataclass, field

import claude_agent_sdk as sdk


WORKER_SYSTEM_PROMPT = (
    "You are a software engineering agent working exclusively in a git worktree. "
    "Implement the stated task, commit your changes, then report a concise summary. "
    "Stay focused on the branch's purpose; do not modify files outside your worktree."
)


@dataclass
class WorkerInfo:
    branch: str
    worktree_path: str
    task: str
    status: str = "starting"   # starting | running | done | error
    result: str | None = None

    def to_dict(self) -> dict:
        return {
            "branch": self.branch,
            "worktree_path": self.worktree_path,
            "task": self.task,
            "status": self.status,
            "result": self.result,
        }


class WorkerPool:
    def __init__(self, model: str = "claude-opus-4-6"):
        self.model = model
        self._workers: dict[str, WorkerInfo] = {}
        self._tasks: dict[str, asyncio.Task] = {}

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def spawn(self, branch: str, worktree_path: str, task: str) -> WorkerInfo:
        if self.is_running(branch):
            return self._workers[branch]
        info = WorkerInfo(branch=branch, worktree_path=worktree_path, task=task)
        self._workers[branch] = info
        self._tasks[branch] = asyncio.create_task(
            self._run(info), name=f"worker-{branch}"
        )
        return info

    async def stop(self, branch: str) -> None:
        t = self._tasks.pop(branch, None)
        if t and not t.done():
            t.cancel()
            try:
                await t
            except asyncio.CancelledError:
                pass
        self._workers.pop(branch, None)

    async def stop_all(self) -> None:
        for branch in list(self._workers):
            await self.stop(branch)

    def is_running(self, branch: str) -> bool:
        info = self._workers.get(branch)
        return info is not None and info.status in ("starting", "running")

    def get(self, branch: str) -> WorkerInfo | None:
        return self._workers.get(branch)

    def all(self) -> list[WorkerInfo]:
        return list(self._workers.values())

    # ------------------------------------------------------------------
    # Internal
    # ------------------------------------------------------------------

    async def _run(self, info: WorkerInfo) -> None:
        info.status = "running"
        opts = sdk.ClaudeAgentOptions(
            system_prompt=WORKER_SYSTEM_PROMPT,
            model=self.model,
            cwd=info.worktree_path,
            permission_mode="bypassPermissions",
        )
        try:
            lines: list[str] = []
            async for msg in sdk.query(prompt=info.task, options=opts):
                if isinstance(msg, sdk.AssistantMessage):
                    for block in msg.content:
                        if isinstance(block, sdk.TextBlock):
                            lines.append(block.text)
                elif isinstance(msg, sdk.ResultMessage):
                    info.result = msg.result or "\n".join(lines)
            if info.result is None:
                info.result = "\n".join(lines)
            info.status = "done"
        except asyncio.CancelledError:
            info.status = "error"
            info.result = "cancelled"
            raise
        except Exception as exc:
            info.status = "error"
            info.result = str(exc)
