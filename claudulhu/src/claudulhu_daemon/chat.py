"""WebSocket chat handler — ephemeral Claude session per connection."""

from __future__ import annotations

import asyncio
import json
from typing import Any, Awaitable, Callable

import claude_agent_sdk as sdk
from fastapi import WebSocket, WebSocketDisconnect


# ---------------------------------------------------------------------------
# Wire protocol helpers
# ---------------------------------------------------------------------------

def _msg(**kwargs: Any) -> str:
    return json.dumps(kwargs)


def _format_sdk_message(msg: sdk.Message) -> list[dict]:
    """Translate one SDK Message into ≥0 wire frames."""
    frames: list[dict] = []
    if isinstance(msg, sdk.AssistantMessage):
        for block in msg.content:
            if isinstance(block, sdk.TextBlock):
                frames.append({"type": "text", "text": block.text})
            elif isinstance(block, sdk.ToolUseBlock):
                frames.append({"type": "tool_use", "tool": block.name, "input": block.input})
            elif isinstance(block, sdk.ToolResultBlock):
                frames.append({"type": "tool_result", "tool_use_id": block.tool_use_id, "content": block.content})
    elif isinstance(msg, sdk.ResultMessage):
        frames.append({
            "type": "result",
            "cost_usd": msg.total_cost_usd,
            "turns": msg.num_turns,
            "session_id": msg.session_id,
            "result": msg.result,
        })
    elif isinstance(msg, sdk.SystemMessage):
        # Skip init frames — they contain internal Claude Code session metadata
        if msg.data.get("subtype") == "init":
            return frames
        import json as _json
        text = msg.data.get("message") or _json.dumps(msg.data)
        frames.append({"type": "system", "text": text})
    return frames


# ---------------------------------------------------------------------------
# Single-session handler
# ---------------------------------------------------------------------------

async def handle_chat(
    websocket: WebSocket,
    session_id: str,
    repo_path: str,
    model: str,
    system_prompt: str,
    on_spawn_worker: Callable[[str], Awaitable[dict]] | None = None,
) -> None:
    """Manages one ephemeral WebSocket chat session end-to-end.

    The Claude session is created fresh on connect and discarded on disconnect.
    Conversation history does not persist between connections.
    """
    await websocket.accept()
    await websocket.send_text(_msg(type="ready", session_id=session_id, resumed=False))

    opts = sdk.ClaudeAgentOptions(
        system_prompt=system_prompt,
        model=model,
        cwd=repo_path,
        permission_mode="bypassPermissions",
    )

    stream_task: asyncio.Task | None = None

    async def stream_response(client: sdk.ClaudeSDKClient) -> None:
        try:
            async for msg in client.receive_response():
                print(f"[chat:{session_id}] msg type={type(msg).__name__}")
                for frame in _format_sdk_message(msg):
                    await websocket.send_text(_msg(**frame))
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            print(f"[chat:{session_id}] stream error: {type(exc).__name__}: {exc}")
            try:
                await websocket.send_text(_msg(type="error", message=str(exc)))
            except Exception:
                pass

    try:
        async with sdk.ClaudeSDKClient(options=opts) as client:
            print(f"[chat:{session_id}] connected")

            while True:
                raw = await websocket.receive_text()
                try:
                    data = json.loads(raw)
                except json.JSONDecodeError:
                    await websocket.send_text(_msg(type="error", message="invalid JSON"))
                    continue

                kind = data.get("type")

                if kind == "message":
                    text = data.get("text", "").strip()
                    if not text:
                        continue
                    if stream_task and not stream_task.done():
                        stream_task.cancel()
                        try:
                            await stream_task
                        except asyncio.CancelledError:
                            pass
                        await client.interrupt()

                    try:
                        await client.query(text)
                        stream_task = asyncio.create_task(stream_response(client))
                    except Exception as exc:
                        print(f"[chat:{session_id}] query error: {exc}")
                        await websocket.send_text(_msg(type="error", message=str(exc)))

                elif kind == "interrupt":
                    if stream_task and not stream_task.done():
                        stream_task.cancel()
                        try:
                            await stream_task
                        except asyncio.CancelledError:
                            pass
                    await client.interrupt()
                    await websocket.send_text(_msg(type="interrupted"))

                elif kind == "set_model":
                    m = data.get("model")
                    if m:
                        await client.set_model(m)
                        await websocket.send_text(_msg(type="model_set", model=m))

                elif kind == "spawn_worker":
                    task = data.get("task", "").strip()
                    if not task:
                        await websocket.send_text(_msg(type="error", message="spawn_worker requires a task"))
                        continue
                    if on_spawn_worker is None:
                        await websocket.send_text(_msg(type="error", message="worker spawning not available"))
                        continue
                    await websocket.send_text(_msg(type="spawning", task=task))
                    try:
                        result = await on_spawn_worker(task)
                        await websocket.send_text(_msg(type="worker_created", task=task, **result))
                    except Exception as exc:
                        print(f"[chat:{session_id}] spawn_worker error: {exc}")
                        await websocket.send_text(_msg(type="worker_error", message=str(exc)))

                else:
                    await websocket.send_text(_msg(type="error", message=f"unknown type: {kind!r}"))

    except WebSocketDisconnect:
        if stream_task and not stream_task.done():
            stream_task.cancel()
        print(f"[chat:{session_id}] disconnected")
    except Exception as exc:
        print(f"[chat:{session_id}] unhandled exception: {type(exc).__name__}: {exc}")
        try:
            await websocket.send_text(_msg(type="error", message=str(exc)))
            await websocket.close()
        except Exception:
            pass
