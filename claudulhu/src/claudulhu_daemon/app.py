"""FastAPI application — wires together chat, workers, and git monitor."""

from __future__ import annotations

import asyncio
import os
from contextlib import asynccontextmanager
from typing import Any

from fastapi import FastAPI, HTTPException, WebSocket
from fastapi.responses import JSONResponse
from git import Repo

from .chat import handle_chat
from .monitor import GitMonitor
from .sessions import SessionRecord, SessionStore
from .workers import WorkerPool


# ---------------------------------------------------------------------------
# App state
# ---------------------------------------------------------------------------

class _State:
    repo: Repo
    repo_path: str
    monitor: GitMonitor
    workers: WorkerPool
    sessions: SessionStore
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
        "You can inspect code, answer questions, create branches, and coordinate work. "
        "Be concise and precise."
    )


def _worktree_system_prompt(worktree_path: str) -> str:
    snap = state.monitor.snapshot
    branches = ", ".join(snap.branches) or "none"
    active = [w.branch for w in state.workers.all()]
    return (
        f"You are an AI assistant helping engineer the git repository at {state.repo_path}.\n"
        f"You are working in the worktree at {worktree_path}.\n"
        f"Current branches: {branches}\n"
        f"Branches with active sessions: {', '.join(active) or 'none'}\n\n"
        "You can inspect code, propose changes, make commits, and more. "
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

app = FastAPI(title="claudulhud", lifespan=lifespan)

_MAIN_SESSION_KEY = "__main__"


# ------------------------------------------------------------------
# WebSocket  /chat  (main repo-level session)
# ------------------------------------------------------------------

@app.websocket("/chat")
async def chat_ws(websocket: WebSocket):
    if state._main_chat_active:
        await websocket.close(code=4009, reason="A main chat session is already active")
        return

    record = state.sessions.load(_MAIN_SESSION_KEY)
    resume_id = record.sdk_session_id if record else None

    async def _persist(sdk_session_id: str) -> None:
        import datetime
        state.sessions.save(SessionRecord(
            branch=_MAIN_SESSION_KEY,
            sdk_session_id=sdk_session_id,
            worktree_path=state.repo_path,
            last_seen=datetime.datetime.now(datetime.timezone.utc).isoformat(),
        ))

    state._main_chat_active = True
    try:
        await handle_chat(
            websocket=websocket,
            session_id=_MAIN_SESSION_KEY,
            repo_path=state.repo_path,
            model=state.model,
            system_prompt=_main_system_prompt(),
            resume_sdk_session_id=resume_id,
            on_session_id=_persist,
        )
    finally:
        state._main_chat_active = False


# ------------------------------------------------------------------
# WebSocket  /workers/{branch}
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

    # Resume prior session if one exists on disk
    record = state.sessions.load(branch)
    resume_id = record.sdk_session_id if record else None

    async def _persist_session_id(sdk_session_id: str) -> None:
        import datetime
        state.sessions.save(SessionRecord(
            branch=branch,
            sdk_session_id=sdk_session_id,
            worktree_path=wt_path,
            last_seen=datetime.datetime.now(datetime.timezone.utc).isoformat(),
        ))

    state.workers.register(branch, wt_path, websocket)
    try:
        await handle_chat(
            websocket=websocket,
            session_id=branch,
            repo_path=wt_path,
            model=state.model,
            system_prompt=_worktree_system_prompt(wt_path),
            resume_sdk_session_id=resume_id,
            on_session_id=_persist_session_id,
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
