# Agent in the loop: a local model that listens to the radio and acts

**Status: draft for Kord.** It includes advice from the Lume agent (Magnificent
Vole) and the Hyperia agent (Ordinary Penguin); see "What Lume and Hyperia
learned" below. Kord asked for this on 2026-10-16. Assume a dumb model:
qwen2.5:7b class, on local Ollama.

## What it's for

Kord keys up and talks to the radio, not to a specific pane. Today hertz only
routes a call to the pane it names. With the agent in the loop, a call that
names the agent ("Hertz, …") or names no one gets handled. Examples:

| Kord says | Agent does |
|---|---|
| "Hertz, what's the weather?" | `weather_latest` (or Lume), then speaks a short summary |
| "Hertz, turn weather on, every thirty" | `weather_set {enabled: true, every_minutes: 30}`, then confirms aloud |
| "Hertz, lock out seventy-one" | `lockout {channel: "71", minutes: 30}`, then confirms |
| "Hertz, what did I miss?" | `transcripts {since: 1h}`, then summarizes |
| "Hertz, tell Magnificent Vole the Lume build is good" | Hyperia `msg_send` to the pane (confirms first) |
| "Hertz, scan FRS only" | `scan_set {entries: ["frs-gmrs"]}`, then confirms |
| *(Ch 71 stuck open for 30 s)* | Reports it, or locks it out if allowed |

## Design for a dumb model

A 7B model can't plan many steps, juggle 35 tools or produce free-form JSON
reliably. So the code does the structure, and the model makes small,
constrained choices.

### 1. One call, one turn; no open-ended loop

Each radio call is one task with a hard step budget (3 tool calls at most) and a
wall-clock limit (20 s). There's no "keep going until done" and no agent
talking to itself.

### 2. Route first with constrained choice (ollaya `/api/decide`)

1. **Is it for the agent?** hertz already routes by call sign, and the agent
   gets its own call sign ("Hertz" or a Hyperia call sign). Calls that name
   another pane go there as today.
2. **Which intent?** `/api/decide` choice over a small set of intents, each with
   a one-line description. Start with 6; Hyperia saw qwen2.5:7b get unreliable
   past about 5 tools, and Lume past about 8. The 6 are:
   - **do nothing**, which Hyperia says is usually right
   - **question** (weather, what did I miss, status)
   - **weather on/off**
   - **scan change**
   - **lockout**
   - **message a pane**

   That's a probability over a list, not generation, which is what small models
   do well; the relay already gets 0.9+ on clear calls. More intents come later
   as a second `/api/decide` level under a group ("scan change" →
   add / remove / band only), never as one long flat list.
3. **Below the confidence threshold**, the agent asks back on the air ("Say
   again?") and does nothing.

### 3. Fill arguments with a schema-constrained call

For the chosen intent only, call Ollama `/api/chat` with `format` set to that
one tool's JSON Schema (structured outputs), so the model can't emit invalid
JSON or the wrong tool. One small schema (2–4 fields, enums wherever possible:
channel ids, WX1–WX7) instead of 35 tools.

Then validate in code (known channel, ranges). On failure, retry once with an
error message that **contains the fix** ("71x is not a channel; channels on
this scan are 68, 69, 71, 72, gmrs-1 … gmrs-22"), then give up and say so. Lume
found error messages that hand back the right value help small models more than
anything else.

**Questions use a small read-only loop instead**, Lume-style ReAct (the shape of
`run_chat_loop` in lume `src/chat_sql.rs`):
- Ollama native tool calling with only 3 read-only tools: `transcripts`,
  `weather_latest`, and Lume's `ti_query` through Lume's own MCP;
- temperature 0 and at most 4 turns;
- a tool error goes back as tool content with the fix, and after 3 errors the
  model must answer.

No `format` here: Lume found it fights tool calling on small models. Actions
never go through this loop, so a question can't change the radio.

### 4. Act through hertzd's MCP and Hyperia

The loop is a plain MCP client to `127.0.0.1:9080/mcp` and Hyperia's MCP. The
model never sees the full tool list, only the one schema for its intent.

### 5. Confirm before risky actions

- **Read-only** (weather, status, recent): act right away.
- **Radio changes** (lockout, scan, squelch, weather on/off): act, then
  confirm aloud; every change is logged as `Event::Tuning` (actor `agent`) and
  can be undone with "Hertz, undo".
- **Messages to other panes:** read the message back and act only on
  "affirmative" on the next call. Never free-form posting as `hertz-radio`.

### 6. Answer briefly, in radio style

The model writes the reply (1–2 sentences, under 200 characters) from the tool
result. Hyperia `hyperia_spoken_summary` speaks it, and it's also mailed to Kord's
pane. Numbers are formatted in code ("eighty-eight degrees") so the model doesn't
garble them.

### 7. Guards

- **Rate:** at most one action per call, and 10 per 10 minutes.
- **No loops:** the agent never reacts to its own transmissions or to
  hertz-radio follow-ups (the relay already skips its own pane).
- **Kill switch:** an `agent_set {enabled: false}` MCP tool, and "Hertz, stand
  down" on the air.
- **Audit:** every step is logged (heard, intent and p, arguments, tool
  result, reply) and sent to Lume as `radio.agent`.
- **Dedupe:** each transmission is handled once, keyed by recording file, and
  the server follow-up for the same call doesn't trigger a second action.
- **Idempotency:** every Hyperia `msg_send` gets an idempotency key.
- **Feedback:** never act on its own spoken summaries if the speaker can be
  heard by a radio. Calls that arrive while it speaks are ignored.
- **Consent:** Hyperia asks Kord the first time the agent writes to each pane
  and remembers the answer. After a deny it refuses repeats for a while
  (`recently_denied`), so the agent can't spam past a "no".

## Where it lives

The plan is a new `hertz-agent` crate, a small binary that runs beside hertzd.
It's an MCP client for hertzd and Hyperia, and talks to ollaya and Ollama
directly. It starts with `hertzd --agent` or on its own. Running it outside
hertzd means a model hang can't stall the radio, and it can run on another
machine against the Pi.

It uses `wait_for_call` (phase 1 of [mcp-tools.md](mcp-tools.md)) to listen
without polling.

**Identity:** its own Hyperia agent identity, `hertz-agent`, like `hertz-radio`.
It isn't a pane, because pane credentials die when the pane closes. It may use
only:
- `msg_send`
- `hyperia_spoken_summary`
- `terminal_status`, read-only

It gets no `terminal_run`, `terminal_keys` or `pane_send`, and no
`radio_deliver`, which belongs to hertz-radio.

**Reuse:**
- **hertz-relay:** the Hyperia and ollaya clients.
- **Lume `src/chat_sql.rs` (about 300 lines to copy):** it isn't a library API,
  but it has
  - Ollama endpoint probing (`select_ollama_endpoint`);
  - cloud auth that sends `OLLAMA_API_KEY` only to ollama.com
    (`with_ollama_auth`);
  - the message and tool serde structs;
  - the loop shape.
- **MCP client:** neither Lume nor Hyperia has one to reuse. Plain JSON-RPC
  `tools/list` and `tools/call` over HTTP is enough, with `inputSchema` mapped
  to Ollama's `parameters`, as Lume does at `chat_sql.rs:193`.
- **Hyperia's ghost agent** (sidecar `ghost/`, `doors.rs`) is a human-facing
  chat window, not an event loop, so it isn't the base for this.
- `pane_on_idle` exists if an action should wait for a pane to go idle.

## What Lume and Hyperia learned with small models

**From Lume** (qwen2.5:7b passed the New Braunfels question after PR #87):
- **Show the real values.** A schema-first tool that lists what exists (names,
  counts, ranges) lets the model copy instead of guess. For hertz:
  `status` lists the actual channel ids and the WX channels.
- **Use placeholders, never literal examples.** Small models copy examples
  verbatim (`'generator maintenance'` got run as a query). Write
  `<channel id>`, not `"72"`.
- **Errors carry the fix.** This helped most.
- **Keep tools small.** One or two sentences each; few, flat, mostly optional
  parameters; enums.
- **Inject context in code.** The model never fills ids it can't know, such as
  the dongle or the store.
- **Settings and fallbacks.** Temperature 0. Expect the model to write the call
  as text instead of a tool call, and parse that once as a fallback.
- **Size.** 3–5 tools are fine; past about 8 it picks wrong or invents
  arguments.

**From Hyperia:**
- **Constrained choice first,** then fill only that action's arguments with
  `format` set to a JSON schema. Validate in code, and no free tool calling for
  actions.
- **Always offer a "do nothing" action.** Native tool calling on qwen2.5:7b gets
  unreliable past about 5 tools.
- **Hyperia doesn't rate-limit agents,** so the guards live in the loop.

**Still open:** how stable ollaya's `/api/decide` is as the main decision step.
Hyperia doesn't own ollaya, and the owner hasn't been asked yet. hertz has used
it for routing since v0.2 without trouble.

## Depends on

- `/mcp` phase 1: `wait_for_call`, `transcripts`, `status`, `weather_latest`.
- Phase 2 for the scan, lockout and squelch intents. Until then those intents
  answer "not yet".
- An agent call sign. The relay must route "Hertz …" calls to the agent rather
  than to a pane.

## Phases

| Phase | Work | Gets Kord |
|---|---|---|
| **A** | The `hertz-agent` skeleton; intent routing with `/api/decide`; read-only intents (weather, status, recent) answered aloud; audit log | Ask the radio questions and get spoken answers |
| **B** | Argument filling with structured outputs; radio-change intents (weather on/off, lockout, scan, squelch) with confirm and undo; guards | Control the radio by voice |
| **C** | Messages to panes with read-back; proactive reports (stuck channel, weather alerts); tuning on a test set of recorded calls | The agent does useful things unprompted |

## Tests

- **Intent set:** about 50 recorded or typed calls with the expected intent and
  arguments, including Whistle-garbled text ("Stables arena", "Newbrand Fulls").
  Measures `/api/decide` accuracy and argument validity per model. Run it before
  switching models.
- **Loop:** mock hertzd MCP and mock Hyperia (127.0.0.1, port 0). Checks the
  step budget, that it ignores its own transmissions, confirm before messaging,
  rate limits, and undo.

## Questions (also on Kord's sticky)

1. **Call sign:** what does the agent answer to? "Hertz", or a Hyperia-style
   call sign?
2. **Model:** qwen2.5:7b, gemma4:26b (local, slower), or ollaya's laya:en for
   routing plus qwen for arguments?
3. **Changes:** may the agent change the radio (lockout, scan) without asking,
   or should it always confirm first?
4. **Proactive:** should it speak up on its own (stuck channel, severe weather),
   or only when called?
