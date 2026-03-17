"""Git repository monitor — polls for branch/worktree state changes."""

from __future__ import annotations

import asyncio
from dataclasses import dataclass, field
from typing import Awaitable, Callable

from git import Repo


@dataclass
class BranchState:
    name: str
    commit: str
    is_worktree: bool
    worktree_path: str | None = None


@dataclass
class RepoSnapshot:
    branches: dict[str, BranchState] = field(default_factory=dict)

    def to_summary(self) -> list[dict]:
        return [
            {
                "name": s.name,
                "commit": s.commit[:8],
                "worktree": s.worktree_path,
            }
            for s in self.branches.values()
        ]


BranchHandler = Callable[
    [str, BranchState | None, BranchState | None], Awaitable[None]
]


class GitMonitor:
    def __init__(self, repo: Repo, poll_interval: float = 10.0):
        self.repo = repo
        self.poll_interval = poll_interval
        self._snapshot = RepoSnapshot()
        self._handlers: list[BranchHandler] = []

    def on_branch_event(self, handler: BranchHandler) -> None:
        self._handlers.append(handler)

    def _worktree_paths(self) -> dict[str, str]:
        try:
            out = self.repo.git.worktree("list", "--porcelain")
        except Exception:
            return {}
        result: dict[str, str] = {}
        current: str | None = None
        for line in out.splitlines():
            if line.startswith("worktree "):
                current = line[len("worktree "):]
            elif line.startswith("branch refs/heads/") and current:
                result[line[len("branch refs/heads/"):]] = current
                current = None
        return result

    def _take_snapshot(self) -> RepoSnapshot:
        try:
            self.repo.git.fetch("--all", "--prune")
        except Exception:
            pass
        wt = self._worktree_paths()
        branches = {
            h.name: BranchState(
                name=h.name,
                commit=h.commit.hexsha,
                is_worktree=h.name in wt,
                worktree_path=wt.get(h.name),
            )
            for h in self.repo.heads
        }
        return RepoSnapshot(branches=branches)

    async def _emit(self, event: str, old: BranchState | None, new: BranchState | None) -> None:
        for h in self._handlers:
            await h(event, old, new)

    async def _diff(self, old: RepoSnapshot, new: RepoSnapshot) -> None:
        on, nn = set(old.branches), set(new.branches)
        for name in nn - on:
            await self._emit("added", None, new.branches[name])
        for name in on - nn:
            await self._emit("removed", old.branches[name], None)
        for name in on & nn:
            ob, nb = old.branches[name], new.branches[name]
            if ob.commit != nb.commit or ob.is_worktree != nb.is_worktree:
                await self._emit("updated", ob, nb)

    async def run(self, stop: asyncio.Event) -> None:
        self._snapshot = await asyncio.to_thread(self._take_snapshot)
        while not stop.is_set():
            await asyncio.sleep(self.poll_interval)
            try:
                snap = await asyncio.to_thread(self._take_snapshot)
                await self._diff(self._snapshot, snap)
                self._snapshot = snap
            except Exception as exc:
                print(f"[monitor] {exc}")

    @property
    def snapshot(self) -> RepoSnapshot:
        return self._snapshot
