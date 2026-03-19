"""FastAPI application — wires together chat, workers, and git monitor."""

from __future__ import annotations

import asyncio
import os
import re
from contextlib import asynccontextmanager
from typing import Any

from fastapi import FastAPI, HTTPException, WebSocket
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse
from git import Repo

from .chat import handle_chat
from .monitor import GitMonitor
from .workers import WorkerPool


# ---------------------------------------------------------------------------
# App state
# ---------------------------------------------------------------------------

class _State:
    repo: Repo
    repo_path: str
    monitor: GitMonitor
    workers: WorkerPool
    _stop: asyncio.Event
    _monitor_task: asyncio.Task
    model: str
    _main_chat_active: bool = False


state = _State()


def _main_system_prompt() -> str:
    snap = state.monitor.snapshot
    branches = ", ".join(snap.branches) or "none"
    active = [w.branch for w in state.workers.all()]
    return (
        f"You are an AI assistant helping manage the git repository at {state.repo_path}.\n"
        f"Current branches: {branches}\n"
        f"Branches with active sessions: {', '.join(active) or 'none'}\n\n"
        "You have access to the `claudulhu` CLI tool. Use it via bash to manage branches and worktrees.\n\n"
        "Available claudulhu commands:\n"
        "  claudulhu branch <description>   — generate a branch name from the description, create a\n"
        "                                     worktree at ~/.claudulhu/worktrees/{repo}/{branch}, and\n"
        "                                     launch a Claude Code session there with the task\n"
        "  claudulhu list                   — list all worktrees for this repo\n"
        "  claudulhu merge <name>           — merge the named worktree branch into the current branch\n"
        "  claudulhu remove <name>          — remove a worktree and force-delete its branch\n\n"
        "Worktrees are stored at ~/.claudulhu/worktrees/{repo_name}/.\n\n"
        "You can inspect code, answer questions, create branches, coordinate work, and run claudulhu\n"
        "commands directly. Be concise and precise."
    )


def _worktree_system_prompt(branch: str, worktree_path: str) -> str:
    snap = state.monitor.snapshot
    branches = ", ".join(snap.branches) or "none"
    return (
        f"You are an AI assistant working on branch '{branch}' "
        f"in the worktree at {worktree_path}.\n"
        f"Main repository: {state.repo_path}\n"
        f"All branches: {branches}\n\n"
        "You can inspect code, propose changes, make commits, and more. "
        "Be concise and precise."
    )


# ---------------------------------------------------------------------------
# Branch / worktree helpers
# ---------------------------------------------------------------------------

async def _generate_branch_name(task: str, taken: list[str]) -> str:
    prompt = (
        f"Generate a short, lowercase, hyphenated git branch name (2-5 words, no punctuation) "
        f"for this task: {task}. Reply with only the branch name, nothing else."
    )
    if taken:
        prompt += f" Do not use any of these: {', '.join(taken)}."
    proc = await asyncio.create_subprocess_exec(
        "claude", "-p", prompt,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    stdout, _ = await proc.communicate()
    name = stdout.decode().strip().lower().replace(" ", "-")
    return re.sub(r"[^a-z0-9-]", "", name)


def _create_worktree(repo, branch: str) -> str:
    repo_name = os.path.basename(repo.working_dir)
    worktrees_dir = os.path.expanduser(f"~/.claudulhu/worktrees/{repo_name}")
    os.makedirs(worktrees_dir, exist_ok=True)
    worktree_path = os.path.join(worktrees_dir, branch)
    repo.git.worktree("add", "-b", branch, worktree_path)
    return worktree_path


async def spawn_worker_from_task(task: str) -> dict:
    existing = [h.name for h in state.repo.heads]
    taken: list[str] = []
    for _ in range(5):
        branch = await _generate_branch_name(task, taken)
        if branch and branch not in existing:
            break
        taken.append(branch)
    else:
        raise RuntimeError("Could not generate a unique branch name after 5 attempts")

    worktree_path = await asyncio.to_thread(_create_worktree, state.repo, branch)
    print(f"[spawn] created branch={branch} worktree={worktree_path}")
    return {"branch": branch, "worktree_path": worktree_path}


# ---------------------------------------------------------------------------
# Lifespan
# ---------------------------------------------------------------------------

@asynccontextmanager
async def lifespan(app: FastAPI):
    state._stop = asyncio.Event()
    state._monitor_task = asyncio.create_task(
        state.monitor.run(state._stop), name="git-monitor"
    )
    yield
    state._stop.set()
    state._monitor_task.cancel()
    try:
        await state._monitor_task
    except asyncio.CancelledError:
        pass
    await state.workers.stop_all()


# ---------------------------------------------------------------------------
# App
# ---------------------------------------------------------------------------

app = FastAPI(title="claudulhud", lifespan=lifespan)

app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_methods=["*"],
    allow_headers=["*"],
)


# ------------------------------------------------------------------
# WebSocket  /chat  (ephemeral repo-level session)
# ------------------------------------------------------------------

@app.websocket("/chat")
async def chat_ws(websocket: WebSocket):
    if state._main_chat_active:
        await websocket.close(code=4009, reason="A main chat session is already active")
        return

    state._main_chat_active = True
    try:
        await handle_chat(
            websocket=websocket,
            session_id="__main__",
            repo_path=state.repo_path,
            model=state.model,
            system_prompt=_main_system_prompt(),
            on_spawn_worker=spawn_worker_from_task,
        )
    finally:
        state._main_chat_active = False


# ------------------------------------------------------------------
# WebSocket  /workers/{branch}  (ephemeral per-branch session)
# ------------------------------------------------------------------

@app.websocket("/workers/{branch}")
async def worker_ws(websocket: WebSocket, branch: str):
    snap = state.monitor.snapshot
    if branch not in snap.branches:
        await websocket.close(code=4004, reason=f"Branch '{branch}' not found")
        return

    if state.workers.get(branch) is not None:
        await websocket.close(code=4009, reason=f"Branch '{branch}' already has an active session")
        return

    wt_path = snap.branches[branch].worktree_path
    if not wt_path or not os.path.isdir(wt_path):
        await websocket.close(code=4004, reason=f"No worktree for branch '{branch}'")
        return

    state.workers.register(branch, wt_path, websocket)
    try:
        await handle_chat(
            websocket=websocket,
            session_id=branch,
            repo_path=wt_path,
            model=state.model,
            system_prompt=_worktree_system_prompt(branch, wt_path),
        )
    finally:
        state.workers.deregister(branch)


# ------------------------------------------------------------------
# REST  /branches
# ------------------------------------------------------------------

@app.get("/branches")
async def list_branches() -> JSONResponse:
    snap = state.monitor.snapshot
    return JSONResponse(snap.to_summary())


# ------------------------------------------------------------------
# REST  /workers
# ------------------------------------------------------------------

@app.get("/workers")
async def list_workers() -> JSONResponse:
    return JSONResponse([w.to_dict() for w in state.workers.all()])


@app.get("/workers/{branch}")
async def get_worker(branch: str) -> JSONResponse:
    w = state.workers.get(branch)
    if w is None:
        raise HTTPException(404, f"No active session for branch '{branch}'")
    return JSONResponse(w.to_dict())


@app.delete("/workers/{branch}")
async def stop_worker(branch: str) -> JSONResponse:
    w = state.workers.get(branch)
    if w is None:
        raise HTTPException(404, f"No active session for branch '{branch}'")
    await state.workers.disconnect(branch)
    return JSONResponse({"disconnected": branch})


# ------------------------------------------------------------------
# Health
# ------------------------------------------------------------------

@app.get("/health")
async def health() -> dict[str, Any]:
    snap = state.monitor.snapshot
    return {
        "repo": state.repo_path,
        "branches": len(snap.branches),
        "active_sessions": len(state.workers.all()),
        "model": state.model,
    }
