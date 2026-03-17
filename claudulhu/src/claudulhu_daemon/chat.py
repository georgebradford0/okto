"""WebSocket chat handler — one ClaudeSDKClient session per connection."""

from __future__ import annotations

import asyncio
import json
from typing import Any

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
        frames.append({"type": "system", "text": msg.data})
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
) -> None:
    """Manages one user's WebSocket connection end-to-end."""
    await websocket.accept()

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
                for frame in _format_sdk_message(msg):
                    await websocket.send_text(_msg(**frame))
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            await websocket.send_text(_msg(type="error", message=str(exc)))

    try:
        async with sdk.ClaudeSDKClient(options=opts) as client:
            await websocket.send_text(_msg(type="ready", session_id=session_id))

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
                    # If a prior response is still streaming, interrupt it first
                    if stream_task and not stream_task.done():
                        stream_task.cancel()
                        try:
                            await stream_task
                        except asyncio.CancelledError:
                            pass
                        await client.interrupt()

                    await client.query(text)
                    stream_task = asyncio.create_task(stream_response(client))

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

                else:
                    await websocket.send_text(_msg(type="error", message=f"unknown type: {kind!r}"))

    except WebSocketDisconnect:
        if stream_task and not stream_task.done():
            stream_task.cancel()
    except Exception as exc:
        try:
            await websocket.send_text(_msg(type="error", message=str(exc)))
            await websocket.close()
        except Exception:
            pass
