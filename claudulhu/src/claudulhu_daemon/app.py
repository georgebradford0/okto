"""FastAPI application — wires together chat, workers, and git monitor."""

from __future__ import annotations

import asyncio
import os
import uuid
from contextlib import asynccontextmanager
from typing import Any

from fastapi import FastAPI, HTTPException, WebSocket
from fastapi.responses import JSONResponse
from git import InvalidGitRepositoryError, Repo
from pydantic import BaseModel

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


state = _State()


def _build_system_prompt() -> str:
    snap = state.monitor.snapshot
    branches = ", ".join(snap.branches) or "none"
    active = [w.branch for w in state.workers.all() if w.status in ("starting", "running")]
    return (
        f"You are an AI assistant helping engineer the git repository at {state.repo_path}.\n"
        f"Current branches: {branches}\n"
        f"Branches with active workers: {', '.join(active) or 'none'}\n\n"
        "You can inspect code, propose changes, create branches, spawn workers, and more. "
        "Be concise and precise."
    )


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

app = FastAPI(title="claudulhu", lifespan=lifespan)


# ------------------------------------------------------------------
# WebSocket  /chat
# ------------------------------------------------------------------

@app.websocket("/chat")
async def chat_ws(websocket: WebSocket):
    session_id = str(uuid.uuid4())
    await handle_chat(
        websocket=websocket,
        session_id=session_id,
        repo_path=state.repo_path,
        model=state.model,
        system_prompt=_build_system_prompt(),
    )


@app.websocket("/chat/{session_id}")
async def chat_ws_with_id(websocket: WebSocket, session_id: str):
    await handle_chat(
        websocket=websocket,
        session_id=session_id,
        repo_path=state.repo_path,
        model=state.model,
        system_prompt=_build_system_prompt(),
    )


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

class SpawnRequest(BaseModel):
    task: str
    worktree_path: str | None = None


@app.get("/workers")
async def list_workers() -> JSONResponse:
    return JSONResponse([w.to_dict() for w in state.workers.all()])


@app.post("/workers/{branch}")
async def spawn_worker(branch: str, body: SpawnRequest) -> JSONResponse:
    snap = state.monitor.snapshot
    if branch not in snap.branches:
        raise HTTPException(404, f"Branch '{branch}' not found")

    wt_path = body.worktree_path or snap.branches[branch].worktree_path
    if not wt_path or not os.path.isdir(wt_path):
        raise HTTPException(400, f"No worktree directory for branch '{branch}'")

    info = state.workers.spawn(branch, wt_path, body.task)
    return JSONResponse(info.to_dict(), status_code=202)


@app.delete("/workers/{branch}")
async def stop_worker(branch: str) -> JSONResponse:
    w = state.workers.get(branch)
    if w is None:
        raise HTTPException(404, f"No worker for branch '{branch}'")
    await state.workers.stop(branch)
    return JSONResponse({"stopped": branch})


@app.get("/workers/{branch}")
async def get_worker(branch: str) -> JSONResponse:
    w = state.workers.get(branch)
    if w is None:
        raise HTTPException(404, f"No worker for branch '{branch}'")
    return JSONResponse(w.to_dict())


# ------------------------------------------------------------------
# Health
# ------------------------------------------------------------------

@app.get("/health")
async def health() -> dict[str, Any]:
    snap = state.monitor.snapshot
    return {
        "repo": state.repo_path,
        "branches": len(snap.branches),
        "workers": len(state.workers.all()),
        "model": state.model,
    }
