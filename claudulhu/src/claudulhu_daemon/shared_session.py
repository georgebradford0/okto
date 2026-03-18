"""Shared main chat session — one Claude SDK client, many WebSocket subscribers."""

from __future__ import annotations

import asyncio
import json
from typing import Awaitable, Callable

import claude_agent_sdk as sdk
from fastapi import WebSocket


class SharedChatSession:
    """A single Claude session shared across all connected WebSocket clients."""

    def __init__(self) -> None:
        self._client: sdk.ClaudeSDKClient | None = None
        self._subscribers: set[WebSocket] = set()
        self._stream_task: asyncio.Task | None = None
        self._is_streaming = False
        self._resumed = False

    # ------------------------------------------------------------------
    # Lifecycle
    # ------------------------------------------------------------------

    async def start(
        self,
        options: sdk.ClaudeAgentOptions,
        resumed: bool = False,
    ) -> None:
        """Start (or resume) the Claude session. Called once at app startup."""
        self._resumed = resumed
        self._client = sdk.ClaudeSDKClient(options=options)
        await self._client.__aenter__()
        print(f"[shared_session] started (resumed={resumed})")

    async def stop(self) -> None:
        """Tear down gracefully. Called at app shutdown."""
        if self._stream_task and not self._stream_task.done():
            self._stream_task.cancel()
            try:
                await self._stream_task
            except asyncio.CancelledError:
                pass
        if self._client:
            try:
                await self._client.__aexit__(None, None, None)
            except Exception as exc:
                print(f"[shared_session] stop error: {exc}")
            self._client = None

    # ------------------------------------------------------------------
    # Subscriber management
    # ------------------------------------------------------------------

    def subscribe(self, ws: WebSocket) -> None:
        self._subscribers.add(ws)

    def unsubscribe(self, ws: WebSocket) -> None:
        self._subscribers.discard(ws)

    @property
    def subscriber_count(self) -> int:
        return len(self._subscribers)

    @property
    def is_streaming(self) -> bool:
        return self._is_streaming

    @property
    def resumed(self) -> bool:
        return self._resumed

    # ------------------------------------------------------------------
    # Messaging
    # ------------------------------------------------------------------

    async def broadcast(self, **kwargs) -> None:
        """Send a JSON frame to all subscribers, pruning dead connections."""
        text = json.dumps(kwargs)
        dead: set[WebSocket] = set()
        for ws in list(self._subscribers):
            try:
                await ws.send_text(text)
            except Exception:
                dead.add(ws)
        self._subscribers -= dead

    async def send_to(self, ws: WebSocket, **kwargs) -> None:
        """Send a JSON frame to one subscriber."""
        try:
            await ws.send_text(json.dumps(kwargs))
        except Exception:
            pass

    # ------------------------------------------------------------------
    # Claude interaction
    # ------------------------------------------------------------------

    async def query(
        self,
        text: str,
        on_session_id: Callable[[str], Awaitable[None]] | None = None,
    ) -> None:
        if self._client is None:
            await self.broadcast(type="error", message="Session not initialized")
            return

        # Cancel any in-flight stream
        if self._stream_task and not self._stream_task.done():
            self._stream_task.cancel()
            try:
                await self._stream_task
            except asyncio.CancelledError:
                pass
            await self._client.interrupt()

        try:
            await self._client.query(text)
            self._stream_task = asyncio.create_task(
                self._stream_response(on_session_id)
            )
        except Exception as exc:
            print(f"[shared_session] query error: {exc}")
            await self.broadcast(type="error", message=str(exc))

    async def interrupt(self) -> None:
        if self._stream_task and not self._stream_task.done():
            self._stream_task.cancel()
            try:
                await self._stream_task
            except asyncio.CancelledError:
                pass
        if self._client:
            try:
                await self._client.interrupt()
            except Exception:
                pass
        self._is_streaming = False
        await self.broadcast(type="interrupted")

    async def _stream_response(
        self,
        on_session_id: Callable[[str], Awaitable[None]] | None,
    ) -> None:
        self._is_streaming = True
        try:
            async for msg in self._client.receive_response():  # type: ignore[union-attr]
                print(f"[shared_session] msg type={type(msg).__name__}")
                if isinstance(msg, sdk.AssistantMessage):
                    for block in msg.content:
                        if isinstance(block, sdk.TextBlock):
                            await self.broadcast(type="text", text=block.text)
                        elif isinstance(block, sdk.ToolUseBlock):
                            await self.broadcast(
                                type="tool_use",
                                tool=block.name,
                                input=block.input,
                            )
                        elif isinstance(block, sdk.ToolResultBlock):
                            await self.broadcast(
                                type="tool_result",
                                tool_use_id=block.tool_use_id,
                                content=block.content,
                            )
                elif isinstance(msg, sdk.ResultMessage):
                    if msg.session_id and on_session_id:
                        await on_session_id(msg.session_id)
                    await self.broadcast(
                        type="result",
                        cost_usd=msg.total_cost_usd,
                        turns=msg.num_turns,
                        session_id=msg.session_id,
                        result=msg.result,
                    )
                elif isinstance(msg, sdk.SystemMessage):
                    text = msg.data.get("message") or json.dumps(msg.data)
                    await self.broadcast(type="system", text=text)
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            print(f"[shared_session] stream error: {type(exc).__name__}: {exc}")
            await self.broadcast(type="error", message=str(exc))
        finally:
            self._is_streaming = False
