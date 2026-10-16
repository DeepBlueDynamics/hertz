# Agent in the loop: ollaya listens to the radio and runs hertz tools

**Status: approved by Kord on 2026-10-16; being built.** The first draft of this
plan used an Ollama text model for arguments and replies. Kord's decisions
replaced that:

- **ollaya only.** hertz uses only ollaya (`/api/decide`) for decisions. No text
  generation, so the agent writes no free text.
- **Every decision is a choice.** ollaya picks the tool and then each of its
  arguments from a list of options. The code assembles the call from those
  picks. That's how "command writing" works: the code writes the command from the
  picks.
- **The tools are built to be decided by ollaya.** Each tool carries its own
  decision questions (below). The models get trained on what's recorded, by
  someone else, not by this work.
- **A rotating call sign.** The agent answers to a call sign that changes after
  each command it carries out, and the new one is posted to the chat.
- **Stock replies.** It answers through Hyperia `hyperia_spoken_summary` as
  `hertz-radio`, from fixed templates ("Confirmed, scanning channel 72.").
- **It speaks up on its own actions,** such as a channel it locked out.
- **Everything goes to local Lume:** each decision, its probabilities, the call
  made and its result. That's the training set.

## Flow of one call

1. **The call arrives.** A transcript comes in, and the relay asks ollaya who it
   addresses, as it does today. The agent's current call sign is one more option
   beside the Hyperia call signs and pane names. Calls to a pane go there as
   before.
2. **Pick the tool.** If the call names the agent, `/api/decide` asks one choice
   question, `tool`. Its options are the tools marked for the agent plus
   `nothing`, each with the tool's `when` text. Below `min_probability` (the
   relay's), the agent says "Say again" and does nothing.
3. **Pick the arguments.** For the chosen tool, one `/api/decide` call asks all
   of that tool's argument questions together, for example `channel` and
   `minutes`. Options come from live state: the channel ids on the scan, WX1–WX7,
   or fixed steps such as 5, 15, 30 or 60 minutes. An argument whose best option
   is below the threshold makes the agent say "Say again" and stop.
4. **Write and check the command.** The code builds the JSON arguments from the
   picks and calls the tool through the same registry `/mcp` uses. Tool
   validation runs as for any caller.
5. **Reply.** The tool's stock reply template is filled from the result and
   spoken: "Confirmed, weather on, every thirty minutes." Numbers are spelled out
   in code. A failure gets a stock reply too: "Unable, <reason>."
6. **Rotate.** After a command is carried out, the call sign changes and the new
   one goes to the chat, the log and Lume.
7. **Record.** One `radio.agent` record goes to Lume per call. It holds:
   - the transcript;
   - every question with its options, the answer and all probabilities;
   - the arguments built, the tool result and the reply spoken;
   - the call sign used.

## Decision spec on each tool

Every tool in the registry (`ops/`) may carry an agent spec. It's published in
`tools/list` as `_meta["hertz/decide"]`, so anyone training a model can read
exactly what ollaya is asked:

```json
{
  "when": "The caller wants NOAA weather sampling turned on or off",
  "questions": {
    "enabled": { "on": "The caller wants weather turned on", "off": "The caller wants weather turned off" },
    "every_minutes": { "15": "Every fifteen minutes, or no interval given", "30": "Every thirty minutes", "60": "Every hour" }
  },
  "reply": "Confirmed, weather {enabled}, every {every_minutes} minutes."
}
```

- **`when`:** one sentence, the criteria for this tool in the `tool` question.
- **`questions`:** argument name → option → when that option is right. The
  options are built at call time from live state (channels, WX ids), so new
  channels show up without code changes.
- **Conversion to the argument:** done in code: `"on"` → `true`, `"30"` → `30`,
  a channel option → its id.
- **`reply`:** the stock template, filled from the arguments and the result.
- **Only tools with a spec can be picked by the agent.** Plain MCP clients see
  every tool.

Many channels (around 40) make a long option list. If ollaya struggles, the
channel question splits in two: band group first, then the channel in that
group. That's two levels of choice, never one long flat list.

## Call sign

- **Words:** the agent makes its own call sign from two NATO words ("Quebec
  Lima"), in the style of Hyperia's, so the transcriber's keyword list and the
  routing question treat it like the others.
- **Rotation:** a new call sign after each command carried out and at start. A
  call sign heard on the air works once.
- **Where it's posted:** it's spoken once at start and logged with every change
  in hertzd's console (`AGENT | call sign now Quebec Lima`). It goes to Kord's
  Hyperia chat as a `msg_send` to the configured `agent_chat` pane, sent as
  hertz-radio. It also goes to Lume.
- **Transcription keywords:** it's added to the on-device recognizer's keywords,
  as pane names are.

## Guards

- **Pacing:** one command per call, and at most 10 per 10 minutes.
- **No loops:** the agent ignores calls that arrive while it's speaking, and its
  own spoken summaries never come back in as calls.
- **Dedupe:** each transmission is handled once, keyed by recording file. The
  server double-check doesn't fire the command a second time.
- **Kill switch:** `agent_set {enabled: false}` over MCP or REST; and a
  `stand_down` choice in the `tool` question.
- **Confirmation:** read-only tools answer right away. Radio changes are made and
  then confirmed aloud; there's no read-back step.

## Where it lives

In hertzd, beside the relay: `crates/hertz-daemon/src/agent.rs`. It only makes
ollaya and Hyperia HTTP calls (about 0.2 s each), so there's no model to stall
the radio. It calls tools through the in-process registry, not over HTTP.

## Not in scope

- An Ollama text model, free-form replies, a ReAct loop.
- `relay_send` and messages to other panes.
- Training the decision models.

## Phases

| Phase | Work |
|---|---|
| **A** | Registry `_meta` decide specs; `agent.rs` with the call sign, tool and argument choices, stock replies, `radio.agent` to Lume; agent specs on the phase-1 tools (weather, status, recent calls) |
| **B** | Specs on the phase-2 tools (scan, lockout, hold, quick scan); pacing guards and the kill switch |
| **C** | Speaking up on its own actions (stuck channel locked out, a weather alert) |

## Tests

- **Specs:** every tool with a spec has a `when`, at least two options per
  question, a reply template whose fields exist, and an option conversion that
  produces arguments the tool accepts.
- **Agent:** a mock ollaya (127.0.0.1, port 0) answers fixed choices. The test
  checks that the call is built correctly, the reply text, the rotation, below-threshold "Say
  again", dedupe and the `radio.agent` record.
- **Accuracy set:** recorded and typed calls with the expected tool and
  arguments, including Whisper-garbled text. It scores ollaya models; it's
  meant for whoever trains them.
