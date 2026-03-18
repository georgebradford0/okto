"""Shared main chat session — one Claude SDK client, many WebSocket subscribers."""

from __future__ import annotations

import asyncio
import json
from typing import Awaitable, Callable

import claude_agent_sdk as sdk
from fastapi import WebSocket

# Fraction of max_turns to retain after a prune (keeps the newest turns)
_KEEP_RATIO = 0.75


def _is_context_overflow(exc: Exception) -> bool:
    msg = str(exc).lower()
    return any(k in msg for k in (
        "context", "too long", "prompt is too long",
        "maximum context", "token", "length exceeded",
    ))


class SharedChatSession:
    """A single Claude session shared across all connected WebSocket clients.

    Conversation history is tracked locally. When the number of completed
    turns reaches *max_turns*, the oldest turns are pruned and a fresh
    Claude session is started with the retained history baked into the
    system prompt. The same rotation is triggered reactively if a context-
    overflow error is received from the API.
    """

    def __init__(self, max_turns: int = 100) -> None:
        self._max_turns = max_turns
        self._client: sdk.ClaudeSDKClient | None = None
        self._base_system_prompt: str = ""
        self._base_options: sdk.ClaudeAgentOptions | None = None

        # Completed turns: (user_message, assistant_text)
        self._history: list[tuple[str, str]] = []
        # Accumulate the in-flight turn
        self._pending_user_msg: str | None = None
        self._pending_assistant_text: str = ""

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
        self._base_options = options
        self._base_system_prompt = options.system_prompt or ""
        self._client = sdk.ClaudeSDKClient(options=options)
        await self._client.__aenter__()
        print(f"[shared_session] started (resumed={resumed}, max_turns={self._max_turns})")

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

        # Proactive prune: rotate before we hit the limit
        if len(self._history) >= self._max_turns:
            await self._rotate(reason="context limit reached")

        # Cancel any in-flight stream
        if self._stream_task and not self._stream_task.done():
            self._stream_task.cancel()
            try:
                await self._stream_task
            except asyncio.CancelledError:
                pass
            await self._client.interrupt()

        self._pending_user_msg = text
        self._pending_assistant_text = ""

        try:
            await self._client.query(text)
            self._stream_task = asyncio.create_task(
                self._stream_response(on_session_id)
            )
        except Exception as exc:
            print(f"[shared_session] query error: {exc}")
            self._pending_user_msg = None
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
        self._pending_user_msg = None
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
                            self._pending_assistant_text += block.text
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
                    # Commit the completed turn to history
                    if self._pending_user_msg is not None:
                        self._history.append((
                            self._pending_user_msg,
                            self._pending_assistant_text,
                        ))
                        self._pending_user_msg = None
                        self._pending_assistant_text = ""

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
            if _is_context_overflow(exc):
                # Reactive prune: drop more aggressively (keep only half)
                await self._rotate(reason="context overflow", keep=int(self._max_turns * 0.5))
            else:
                await self.broadcast(type="error", message=str(exc))
        finally:
            self._is_streaming = False

    # ------------------------------------------------------------------
    # Context rotation
    # ------------------------------------------------------------------

    async def _rotate(
        self,
        reason: str = "context limit reached",
        keep: int | None = None,
    ) -> None:
        """Prune oldest turns and restart with a fresh Claude session.

        The retained turns are baked into the system prompt of the new
        session so the model has the recent conversation as background.
        """
        target = keep if keep is not None else int(self._max_turns * _KEEP_RATIO)
        pruned = max(0, len(self._history) - target)

        if pruned:
            self._history = self._history[-target:]

        print(f"[shared_session] rotating ({reason}): pruned {pruned} turns, kept {len(self._history)}")

        # Build augmented system prompt with retained history
        if self._history:
            history_block = "\n\n".join(
                f"User: {u}\nAssistant: {a}"
                for u, a in self._history
            )
            system_prompt = (
                self._base_system_prompt
                + "\n\n---\nRecent conversation history (oldest messages have been pruned):\n\n"
                + history_block
            )
        else:
            system_prompt = self._base_system_prompt

        # Tear down old client
        if self._client:
            try:
                await self._client.__aexit__(None, None, None)
            except Exception:
                pass
            self._client = None

        # Start fresh (no resume — the history is now in the system prompt)
        assert self._base_options is not None
        opts = sdk.ClaudeAgentOptions(
            system_prompt=system_prompt,
            model=self._base_options.model,
            cwd=self._base_options.cwd,
            permission_mode=self._base_options.permission_mode,
        )
        self._client = sdk.ClaudeSDKClient(options=opts)
        await self._client.__aenter__()
        self._resumed = False

        notice = f"context pruned — {pruned} oldest turn{'s' if pruned != 1 else ''} removed, {len(self._history)} retained"
        await self.broadcast(type="system", text=notice)
        print(f"[shared_session] fresh session started after rotation")
