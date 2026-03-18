"""FastAPI application — wires together chat, workers, and git monitor."""

from __future__ import annotations

import asyncio
import json
import os
import re
from contextlib import asynccontextmanager
from typing import Any

import claude_agent_sdk as sdk
from fastapi import FastAPI, HTTPException, WebSocket, WebSocketDisconnect
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import JSONResponse
from git import Repo

from .chat import handle_chat
from .monitor import GitMonitor
from .sessions import SessionRecord, SessionStore
from .shared_session import SharedChatSession
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
    max_turns: int
    main_session: SharedChatSession


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

_MAIN_SESSION_KEY = "__main__"


@asynccontextmanager
async def lifespan(app: FastAPI):
    # Start git monitor
    state._stop = asyncio.Event()
    state._monitor_task = asyncio.create_task(
        state.monitor.run(state._stop), name="git-monitor"
    )

    # Start shared main chat session
    record = state.sessions.load(_MAIN_SESSION_KEY)
    resume_id = record.sdk_session_id if record else None
    opts = sdk.ClaudeAgentOptions(
        system_prompt=_main_system_prompt(),
        model=state.model,
        cwd=state.repo_path,
        permission_mode="bypassPermissions",
        resume=resume_id,
    )
    state.main_session = SharedChatSession(max_turns=state.max_turns)
    await state.main_session.start(opts, resumed=resume_id is not None)

    yield

    # Shutdown
    state._stop.set()
    state._monitor_task.cancel()
    try:
        await state._monitor_task
    except asyncio.CancelledError:
        pass
    await state.main_session.stop()
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
# WebSocket  /chat  (shared main session — all clients see same conversation)
# ------------------------------------------------------------------

@app.websocket("/chat")
async def chat_ws(websocket: WebSocket):
    await websocket.accept()

    async def _persist(sdk_session_id: str) -> None:
        import datetime
        state.sessions.save(SessionRecord(
            branch=_MAIN_SESSION_KEY,
            sdk_session_id=sdk_session_id,
            worktree_path=state.repo_path,
            last_seen=datetime.datetime.now(datetime.timezone.utc).isoformat(),
        ))

    # Greet the new subscriber
    await websocket.send_text(json.dumps({
        "type": "ready",
        "session_id": _MAIN_SESSION_KEY,
        "resumed": state.main_session.resumed,
    }))

    state.main_session.subscribe(websocket)
    print(f"[chat] subscriber joined (total={state.main_session.subscriber_count})")
    try:
        while True:
            raw = await websocket.receive_text()
            try:
                data = json.loads(raw)
            except json.JSONDecodeError:
                await state.main_session.send_to(
                    websocket, type="error", message="invalid JSON"
                )
                continue

            kind = data.get("type")

            if kind == "message":
                text = data.get("text", "").strip()
                if text:
                    await state.main_session.query(text, on_session_id=_persist)

            elif kind == "interrupt":
                await state.main_session.interrupt()

            elif kind == "spawn_worker":
                task = data.get("task", "").strip()
                if not task:
                    await state.main_session.send_to(
                        websocket, type="error", message="spawn_worker requires a task"
                    )
                    continue
                # spawning feedback is individual; worker_created is broadcast
                await state.main_session.send_to(websocket, type="spawning", task=task)
                try:
                    result = await spawn_worker_from_task(task)
                    await state.main_session.broadcast(type="worker_created", **result)
                except Exception as exc:
                    print(f"[chat] spawn_worker error: {exc}")
                    await state.main_session.send_to(
                        websocket, type="worker_error", message=str(exc)
                    )

            else:
                await state.main_session.send_to(
                    websocket, type="error", message=f"unknown type: {kind!r}"
                )

    except WebSocketDisconnect:
        pass
    finally:
        state.main_session.unsubscribe(websocket)
        print(f"[chat] subscriber left (total={state.main_session.subscriber_count})")


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
        "shared_session_subscribers": state.main_session.subscriber_count,
        "shared_session_turns": len(state.main_session._history),
        "max_turns": state.max_turns,
        "model": state.model,
    }
