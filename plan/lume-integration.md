# Hertz → Lume (v2)

Goal: hertzd finds a running Lume by itself and sends it the radio traffic, so
transcripts are searchable next to the boat's telemetry and documents. "What was
said on Ch 16 when the bilge alarm went off?"

Agreed with the Lume lead (Magnificent Vole) on 2026-10-15, checked against Lume
origin/main.

## What Lume has today

- **Port.** `lume serve` and the Signal K plugin's `lume ti ingest --serve` listen
  on `127.0.0.1:5863` by default. No mDNS, no discovery file, no "where is the
  server" command.
- **Health.** `GET /health` returns 200 with an empty body. It's public only
  under nuts-auth; otherwise it follows the normal auth rules.
- **Auth.** No auth configured: loopback clients need no token. Off loopback,
  Lume refuses to start without auth (`--http-token-file`, or `--nuts-auth` with
  `--nuts-allow`). OTLP also takes `--otlp-token-file`. `/v1/*` needs the write
  scope.
- **OTLP logs** (`POST /v1/logs`). Each record becomes a `logbook` document:
  - title: `event_name` (or the `event.name` attribute)
  - body: a JSON string of every attribute plus the OTLP body, so
    `match(body, 'channel 16')` and transcript words are searchable
  - attributes: not separate SQL columns
  - entity: `agent.urn:<service.instance.id | pane | service.name>`
  - deduplicated by a content hash; 8 MiB request limit
- **OTLP metrics** (`POST /v1/metrics`). The path is the metric name plus each
  attribute as `.key.value`. For example `radio.signal_db` with `channel=16`
  becomes `radio.signal_db.channel.16`. Attributes must be low-cardinality:
  channel id, not a raw frequency or a transcript id.

## Design

### Discovery

hertzd tries these in order at startup, then again every 60 s while it has no
Lume:

1. `[lume] url` in the config, if set.
2. Lume's discovery file (Lume-side item b, not built yet): `[lume]
   discovery_file`, default `~/.lume/serve.json`. On HaLOS the Signal K plugin
   writes it at `plugin-config-data/signalk-lume-ti/serve.json`, mounted read-only
   into hertz with the token file. It holds url, port, pid, version, otlp on/off,
   and the token file's path. hertz skips it when otlp is off. The pid check isn't
   built yet; the health probe catches a stale file.
3. A probe of `http://127.0.0.1:5863/health`. Until Lume item a ships, a bare 200
   counts as Lume. After it ships, the JSON must say `"service":"lume"` and
   `"otlp":true`.

No mDNS: a LAN needs a token anyway, so it would add moving parts for little.

```toml
[lume]
enabled = true                 # default true: discovery on, silent when no Lume
url = "http://127.0.0.1:5863"  # optional; skips discovery
token_file = "/data/lume-otlp-token"  # optional; bare token, one line, mode 600
discovery_file = "/lume/serve.json"   # optional; default ~/.lume/serve.json
instance = "viridis"           # service.instance.id; defaults to the host name
```

Token: read from `token_file`, or from the path in the discovery file. It goes
only in the `Authorization: Bearer` header, never on argv or in logs.

### What hertz sends

Resource attributes: `service.name=hertz`, `service.instance.id=<instance>`,
`service.version`.

**Logs** (`/v1/logs`, batched up to 1 s or 100 records):

| event_name | Body | Attributes |
|---|---|---|
| `radio.transcript` | transcript text | `channel`, `freq_mhz`, `dongle`, `recording`, `engine` (`whistle` or `server`) |
| `radio.squelch` | "open Ch 72" / "close Ch 72" | `channel`, `freq_mhz`, `signal_db`, `classification` |
| `radio.recording` | file name | `channel`, `duration_s` |
| `radio.relay` | routed pane or call sign, or "no match" | `channel`, `p`, `delivery` |

The server double-check transcript is sent as a second `radio.transcript` with
`engine=server` when it differs.

**Metrics** (`/v1/metrics`, every 10 s, Lume's bucket):

- `radio.signal_db`, `radio.noise_floor_db`: gauges per `channel`
- `radio.squelch_open_s`, `radio.transmissions`: sums per `channel`
- `radio.transcribe_ms`: gauge
- `radio.dongle_online`: 0/1 gauge per `dongle`

### Behaviour

- A bus subscriber in `crates/hertz-daemon/src/lume.rs`, beside `relay.rs`. It
  never blocks DSP or relay: it uses a bounded queue and drops the oldest record
  when full.
- Retry with backoff (1 s up to 60 s). When Lume goes away, drop the queue and go
  back to discovery.
- One log line when hertz finds Lume ("lume: sending to http://… (discovered via
  …)") and one when it loses Lume. No log line per record.

## HaLOS Pi

The Signal K plugin's Lume binds 127.0.0.1 inside the Signal K container. hertz
in its own container can't reach that loopback unless both use host networking.
Check that first. If they don't:

- The plugin runs OTLP bound to the docker gateway with an `otlpTokenFile`
  (Lume item c).
- hertz mounts that token file read-only from the Signal K app's data folder and
  sets `[lume] url` to the gateway.

That's how a second process gets a token with no person involved.

## Work

**hertz (start now).** Build and test against a hand-run
`lume serve --ti-store <root> --otlp` on loopback, no token:

1. `[lume]` config and discovery steps 1 and 3 (and step 2 once Lume writes the
   file).
2. `lume.rs`: log batching, the 10 s metrics, backoff.
3. Tests against a mock OTLP server on 127.0.0.1 port 0. Test that nothing is sent
   when no Lume is found.
4. Verify: key up on Ch 72, then `lume search "come in over"` finds the
   transcript (`SELECT ts_start, title, body FROM docs WHERE match(body, 'come in')`),
   and `SELECT * FROM telemetry_agents` shows `"radio.squelch_open_s.channel.72"`.
   OTLP metrics land in `telemetry_agents`, not `telemetry` (Lume decision D50).
   Delta sums need `aggregationTemporality` 1; hertz sets it.

   Done on Windows 2026-10-15 against `lume serve --otlp` 0.16.3: metrics arrive
   every 10 s under `agent.urn:<host>`.

**Lume (needs Kord's OK; Minor Canid builds it after 0.17.0):**

- **a.** `GET /health` returns JSON when asked:
  `{"service":"lume","version":"…","otlp":true,"auth":"none|token|nuts"}`. HEAD
  and old clients still get a bare 200.
- **b.** `lume serve` and `ti ingest --serve` write a discovery file
  (`~/.lume/serve.json`; the plugin's at its data folder root) with url, port, pid, version,
  otlp and the token file path (never the token). Remove it on exit.
- **c.** A Signal K plugin switch that turns on OTLP for hertz and creates the
  token file in its data folder.

No new Lume table or schema. Radio data lands in `telemetry` and `docs` under
`agent.urn:hertz…`.
