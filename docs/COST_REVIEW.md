# Cost Efficiency Review — `core/src/lib.rs`

Review date: 2026-03-31
Reviewer: Claude
File reviewed: `core/src/lib.rs` (~1095 lines)

---

## Summary

The codebase already has solid cost-conscious foundations: ephemeral prompt caching on the system prompt and tool definitions, history compaction to prevent unbounded context growth, Haiku for lightweight branch-name generation, and tool-output truncation. The main remaining opportunities are in pricing accuracy, conversation-history caching, and a few structural patterns that leave tokens on the table.

---

## 1. Inaccurate Cost Calculation (HIGH)

**Location:** `cost_usd()` — line 250–252

```rust
pub fn cost_usd(model: &str, input_tokens: u64, output_tokens: u64) -> f64 {
    let (input_rate, output_rate) = if model.contains("opus") { (15.0, 75.0) } else { (3.0, 15.0) };
    ...
}
```

**Problem:** The fallback bucket (`$3.00/$15.00` per million) applies to both Sonnet *and* Haiku. Haiku 4.5 is priced at `$0.80/$4.00` per million — roughly 4× cheaper. Any session using Haiku (e.g., after a user selects it as their model) will show a cost estimate ~4× too high.

Additionally, the function does not account for the caching-specific token categories returned by the API:
- `cache_creation_input_tokens` — billed at **125%** of the normal input rate
- `cache_read_input_tokens` — billed at **10%** of the normal input rate

The `message_start` SSE event contains all three fields. Using only `input_tokens` (the uncached portion) means cache write surcharges are ignored and cache read discounts are not applied, making cost estimates inaccurate in both directions depending on caching activity.

**Recommendation:**
- Add a third pricing tier for models matching `"haiku"` (`$0.80/$4.00`).
- Extract `cache_creation_input_tokens` and `cache_read_input_tokens` from the `message_start` event alongside `input_tokens`, store them in `StreamUsage`, and incorporate them into `cost_usd`.

---

## 2. Conversation History Is Not Cached (HIGH)

**Location:** `stream_turn()` — lines 767–776

Each turn of the agentic loop sends the full compacted message history as plain JSON with no `cache_control` markers. Only two breakpoints exist per request:

1. The system prompt (correct)
2. The last tool definition (correct)

The entire message history — which grows with every tool call — is re-billed as fresh input tokens on every subsequent turn. In a long agentic run with 10+ tool invocations, this means turn N pays for approximately N × (average message size) tokens.

The Anthropic prompt-caching API supports `cache_control: {"type":"ephemeral"}` on individual message content blocks. Placing a breakpoint on the last "stable" message (e.g., the most recent fully-resolved tool-result group that will not change) would cache everything up to that point, with subsequent turns paying only ~10% of those tokens.

**Recommendation:** After compaction, identify the boundary between the stable portion of the history and the current live turn, and add a `cache_control` marker to the last stable message. This is the highest-value caching opportunity in the codebase and can cut input token costs by 50–80% in long agentic sessions.

---

## 3. `reqwest::Client` Recreated on Every Turn (MEDIUM)

**Location:** `stream_turn()` line 759, `generate_branch_name()` line 1031

```rust
let client = reqwest::Client::new();
```

A new HTTP client is built on every API call. `reqwest::Client` is designed to be shared across requests — it maintains a connection pool internally. Recreating it every turn discards the pool, forcing TCP and TLS re-negotiation with `api.anthropic.com` on each turn. This adds latency (especially visible in multi-turn agentic loops) and prevents HTTP/2 connection reuse which can reduce overhead.

**Recommendation:** Create the client once at session initialisation (or lazily via `once_cell`/`LazyLock`) and pass a reference into `stream_turn` and `generate_branch_name`. This doesn't reduce API cost directly but reduces per-turn latency, which matters when the agentic loop runs 5–20 iterations.

---

## 4. Tool Output Truncation Limit Is Generous (MEDIUM)

**Location:** `TOOL_OUTPUT_LIMIT` — line 236

```rust
pub const TOOL_OUTPUT_LIMIT: usize = 20_000;
```

20,000 characters of tool output per tool call feeds back into the next turn as input tokens. In the worst case — a `bash` command that dumps a large file, or a `web_fetch` returning a long HTML page — this is ~5,000–7,000 tokens per tool result being re-sent to the model.

For many tool types the full 20k is not needed. A `glob` result, a `grep` match list, or a `task_list` response rarely needs more than 2,000–4,000 chars to convey all actionable information.

**Recommendation:** Consider per-tool truncation limits rather than a single constant. `bash`, `web_fetch`, and `read_file` (without offset/limit) are the main sources of large outputs; tightening those specifically (e.g., 8,000 chars) while keeping `grep`/`glob`/task tools at a lower limit (e.g., 4,000 chars) would reduce the compounded token cost of multi-turn tool loops without significantly hurting model quality.

---

## 5. History Compaction Stub Is Very Short (LOW–MEDIUM)

**Location:** `compact_history()` — line 716

```rust
const STUB_LIMIT: usize = 400;
```

Older tool-result messages are truncated to 400 characters. While this keeps history small, 400 chars frequently cuts off mid-result in ways that leave the model with no useful signal from those turns. An entirely empty stub (or a stub that communicates the tool name + outcome category, not just the first 400 chars of output) would save the same tokens while being less confusing.

For tool results where the outcome matters structurally (e.g., "file edited successfully", "error: permission denied"), 400 chars may capture the key information. For large read/bash outputs it does not. The current strategy also compacts only `tool_result` messages from the `user` role, leaving verbose `assistant` text blocks (e.g., long reasoning paragraphs before a tool call) at full size even when they are many turns old.

**Recommendation:** Also compact old `assistant` text blocks beyond the keep_full window, and consider stub format that conveys outcome type rather than raw prefix (e.g., `"[bash: exit 0, 1432 chars output — truncated]"`).

---

## 6. `max_tokens` Is Hard-Coded at 16,000 (LOW)

**Location:** `stream_turn()` — line 771

```rust
"max_tokens": 16000,
```

This ceiling is applied uniformly to every request: simple questions, agentic tool loops, and branch-name-adjacent calls alike. 16k is reasonable as a safety ceiling for long coding sessions but is not adaptive.

Note: `max_tokens` caps output — it does not directly inflate cost unless the model fills the full budget, which is uncommon in practice. The risk here is subtle: a very high ceiling gives the model implicit permission to be verbose, and verbose assistant responses in mid-loop turns become part of the message history re-sent as input tokens on all subsequent turns.

**Recommendation:** Consider making this configurable per task type and document that lower ceilings for simple Q&A sessions (e.g., 4,096) can indirectly reduce costs via shorter assistant turns that contribute less to history.

---

## 7. No Session Cost Budget or Guardrails (LOW)

There is currently no mechanism to warn users or halt a session when the accumulated cost exceeds a configurable threshold. In a runaway agentic loop (e.g., model repeatedly calls `bash` with incorrect commands), costs can compound quickly.

**Recommendation:** Add an optional `max_cost_usd` field to `Config`. After each turn, compare `cost_usd(total_input, total_output)` against the budget and emit a `ChatEvent::Error` if exceeded. Even a soft warning at 50% of budget would help users avoid surprises.

---

## 8. `generate_branch_name` Uses No Caching (VERY LOW)

**Location:** lines 1030–1054

The Haiku call for branch-name generation sends no `cache_control` headers and doesn't enable the beta caching feature. Since this prompt is trivially short (one user message + a small system-level instruction embedded in the user turn), there is no real token cost to cache here. The prompt itself is different every call. This is fine as-is and is already well-optimised (Haiku + 32 max_tokens).

---

## Prioritised Action List

| # | Recommendation | Impact | Effort |
|---|---|---|---|
| 1 | Fix `cost_usd` pricing tiers (add Haiku; track cache token categories) | High | Low |
| 2 | Add `cache_control` breakpoints to stable message history | High | Medium |
| 3 | Share `reqwest::Client` across calls | Medium (latency) | Low |
| 4 | Per-tool output truncation limits | Medium | Low |
| 5 | Compact old assistant text blocks + improve stub format | Low–Medium | Low |
| 6 | Make `max_tokens` configurable | Low | Low |
| 7 | Session cost budget / guardrail | Low | Medium |
