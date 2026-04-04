# History compaction

## Why it exists

Every turn of the agentic loop appends two messages to the session history:

1. An **assistant message** — reasoning text plus one or more `tool_use` blocks.
2. A **user message** — one `tool_result` block per tool that was called.

Without any pruning, turn N sends the full history of all previous turns as input
tokens. Because the history grows with every turn, the input token cost of a session
scales quadratically with the number of turns. A 20-turn session ends up sending
roughly 20× the average turn size in input tokens on its final call.

`compact_history` in `core/src/lib.rs` reduces this by stubbing out old turns before
each API request.

---

## What gets compacted and what does not

### The `keep_full` window

`compact_history` is called with `keep_full = 20`. This means the **20 most recent
tool-result user messages** and their paired assistant turns are kept at full fidelity.
Everything older is stubbed.

The window exists because the model frequently needs to refer back to recent tool
results (e.g. a file it just read, the output of the last bash command). Stubbing
those would hurt task quality.

`keep_full = 20` is a deliberately generous setting chosen to prioritise task quality
over token cost while the impact of the new stub preview format (see below) is being
evaluated. The tradeoff is straightforward: a larger window means more input tokens
per turn but fewer re-exploration loops. For most tasks (< 20 tool calls) this means
the full history is never compacted at all.

### What counts as a "tool-result message"

Only user messages whose **entire content** is `ToolResult` blocks qualify. The
initial user message (the human's prompt) is never touched, regardless of age,
because it contains the task description the model is working towards.

---

## What stubs look like

### Tool-result user messages (old)

The raw content is replaced with a compact outcome + size summary:

| Outcome | Stub |
|---|---|
| Success | `[ok — 3 412 chars, truncated]` |
| Error (starts with `error:` or `HTTP `) | `[error — 87 chars, truncated]` |
| Empty content | `[empty]` |

This tells the model what happened and roughly how much output the tool produced,
without including any of the content. The model can infer from the outcome tag
whether the step succeeded, and from the size whether the result was trivial or
substantial.

**Before this change** the stub was the first 400 raw characters of the content
followed by `…[truncated]`. This gave the model an incomplete fragment with no
signal about success/failure or how much was omitted.

### Paired assistant messages (old)

Each old tool-result message has an assistant turn immediately before it. That turn
is also stubbed:

- **`Text` blocks** — replaced with `[truncated]`.
- **`ToolUse` blocks** — `id` and `name` are preserved; `input` is replaced with `{}`.

The `id` must be kept intact because the API validates that every `tool_use` block
in an assistant message has a matching `tool_use_id` in the following user message.
The `name` is kept so the model can see which tool was called at each step.
The `input` detail is dropped because it is redundant once the result is known.

**Before this change** old assistant messages were passed through untouched at full
size. In a long session this was the dominant source of history bloat — a 20-turn
run with 200–500 token assistant reasoning blocks would send the full text of all 14
old assistant turns on every call, regardless of how many tool-result stubs were in
place.

---

## Max turns

`run_agentic_loop` enforces a hard limit of **100 turns**. If the model reaches this
limit the loop emits a `ChatEvent::Error` and exits. This prevents runaway sessions
from accumulating unbounded cost when the model stalls.

## Token impact

For a session with T total turns and `keep_full = 20`:

- **Tool-result messages:** turns 1 through T−20 go from their full output size (up
  to 10 000 chars / ~2 500 tokens each) down to a stub with a 300-char preview.
- **Paired assistant messages:** turns 1 through T−20 go from full reasoning text
  (typically 100–500 tokens each) down to a 200-char preview plus minimal `ToolUse` stubs.

For sessions under 20 tool-result turns, nothing is compacted at all. For longer
sessions, compaction still reduces old-turn token cost substantially while the
preview format retains the key finding from each step.

---

## Call site

```
stream_turn()  →  compact_history(messages, 20)  →  messages_json (sent to API)
```

Compaction runs on the in-memory session snapshot before each API call and does not
mutate the stored session. The full history is preserved in `Session::messages` so
that future turns are compacted from the authoritative source, not from a
previously-compacted view.

---

## Known issues and recommendations

### Problem: sessions loop without making progress

In practice, long agentic sessions frequently stall: the model re-runs the same
discovery tools (grep, read_file, bash) in a cycle without advancing toward the goal.
Four causes have been identified:

#### 1. Content-free stubs erase working memory (primary cause)

`[ok — 123 chars, truncated]` tells the model nothing about what was found. If the
truncated content was the output of a `grep` that located the relevant function, or a
`read_file` that showed the logic to change, that information is gone. On the next turn
the model has no record of having found it and re-runs the same command.

**Recommendation:** replace the content-free stub with a head preview — keep the first
~300 chars of the actual tool output followed by `…[truncated]`. This preserves the
key finding (the matched line, the error message, the function signature) while still
dramatically reducing token count on old turns.

```
[ok — 1 234 chars total]
src/lib.rs:42: pub fn compact_history(messages: &[ApiMessage], keep_full: usize) …
…[truncated]
```

Note: the *previous* approach (before the current stub format was introduced) did keep
a 400-char prefix. It was replaced because it gave "an incomplete fragment with no
signal about success/failure." The right fix is to keep both: outcome tag **and** a
content preview.

#### 2. Assistant text stubs wipe the model's own conclusions

`[truncated]` replaces the model's reasoning, plan, and any intermediate conclusions
it wrote down. When these are gone the model re-derives the same things from scratch.

**Recommendation:** keep the first ~200 chars of each assistant `Text` block (the
opening sentence usually captures the conclusion) followed by `…[truncated]`.

#### 3. `keep_full = 3` is too small for complex tasks

A non-trivial task (explore → locate → read → edit → verify → commit) spans 8–15 tool
calls. With `keep_full = 3`, turns 1–10 of a 13-turn session are fully erased by step 13.

**Recommendation:** raise to 6. The token increase is modest (3 extra full turns) and
significantly extends the model's effective working memory.

#### 4. No stagnation detection or max-turns guard

The agentic loop has no upper bound on turns and no detection of repeated identical
tool calls. A stuck model runs indefinitely at cost.

**Recommendations:**
- Add `MAX_TURNS = 50`; emit `ChatEvent::Error` and return when exceeded.
- Track recent `(tool_name, input_hash)` pairs; if the same call appears twice in the
  last 6 turns, prepend a warning to the tool result instructing the model not to
  repeat it and to either proceed or use `ask_user`.

### Priority order

1. **Head preview in tool-result stubs** — directly fixes the re-exploration loop.
2. **Head preview in assistant text stubs** — preserves plan/conclusion context.
3. **Raise `keep_full` to 6** — widens the full-fidelity window cheaply.
4. **Max-turns + stagnation detection** — safety net; prevents runaway cost.
