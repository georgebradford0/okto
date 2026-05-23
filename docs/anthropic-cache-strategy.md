# Prompt Caching Strategy

## Overview

Octo uses Anthropic's [prompt caching](https://docs.anthropic.com/en/docs/build-with-claude/prompt-caching) to reduce input token costs during agentic sessions. Cached tokens are billed at ~10% of the normal input rate (cache read) or ~125% (cache write, amortised on first use), so effective placement of cache breakpoints is the primary lever for controlling per-turn cost.

Anthropic allows a maximum of **4 cache breakpoints per request**. We consume all 4.

---

## Breakpoint placement

### 1. System prompt (`cache_control: ephemeral`)
The system prompt is static for the lifetime of a session. It is always marked with a cache breakpoint so it is written once and read on every subsequent turn.

### 2. Tool definitions (`cache_control: ephemeral`)
The tool list (last tool definition block) is also static. It receives a breakpoint immediately after the system prompt, keeping the entire preamble cached.

### 3 & 4. Message history (2 × `cache_control: ephemeral`)
The remaining 2 breakpoints are distributed across the compacted message history. The positions are calculated in `core/src/lib.rs` just before the API request is serialised:

```
breakpoint A  →  messages[n - 2]   ← caches entire history up to current turn
breakpoint B  →  messages[n / 2]   ← TTL fallback if A expires
```

For small histories (`n < 4`) only breakpoint A is placed.

#### Why two positions?
A single breakpoint at `n-2` caches the **entire** conversation history as one prefix — no earlier messages pay full price. The second breakpoint at `n/2` is a TTL resilience fallback: if the `n-2` cache expires after 5 minutes of inactivity, the API can still return a partial hit for the first half of history instead of billing the full history at input token rates.

---

## Cache TTL caveat

Anthropic's ephemeral cache entries expire after **5 minutes** of inactivity. In a session with slow turns (e.g. long tool calls or human think-time), the oldest breakpoint may expire and be re-written as a cache miss on the next turn. This is charged at the normal write rate — not a correctness problem, but it means the oldest breakpoint has diminishing returns in very slow sessions.

The current placement (`n-2`, `n/2`) already biases toward recent history: if the `n-2` entry expires, the `n/2` fallback covers the more recent half of the conversation — the portion most likely to still be in cache.

### Practical advice: don't leave conversations idle

If you step away for more than 5 minutes and then send another message, **all cache entries will have expired**. That turn pays full input token rates for the system prompt, tools, and the entire conversation history — identical in cost to starting a brand new conversation from scratch.

The TTL is per cache entry, not per session. Entries stay warm as long as *any* request hits them within the 5-minute window. On a busy server with many concurrent users sharing the same system prompt, that entry may effectively never expire. But a single idle conversation has no such benefit.

**Recommendation:** if you know you're stepping away for a while, start a fresh conversation when you return. You'll pay the same cost for the first turn either way, but a fresh context avoids sending a large stale history that will be billed at full rate.

---

## Message compaction interaction

Before breakpoints are applied, `compact_history` stubs out the bodies of tool-result messages older than the last 3 tool results. This reduces raw token volume independently of caching. The two mechanisms are complementary:

- Compaction reduces the number of tokens sent at all.
- Caching reduces the cost of the tokens that are sent repeatedly.

See [`history-compaction.md`](./history-compaction.md) for details on compaction.

---

## Relevant code

| File | What it does |
|---|---|
| `core/src/lib.rs` | Applies cache breakpoints to system prompt, tools, and message history before serialising the API request |
| `core/src/lib.rs` (`compact_history`) | Stubs old tool results to reduce token volume |
