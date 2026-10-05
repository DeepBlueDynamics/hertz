# Running ollaya for Hertz

ollaya (`ghcr.io/ollaya-dev/ollaya`) serves open *decision* models: you
give it a state (text or JSON) and multiple-choice questions, and it picks an option
with probabilities. Hertz uses it through `hertz-relay` (see `hertz-panes` in the
README) to reason about Hyperia panes, and next to route radio calls to them.
ollaya does not generate text; `/api/generate` and `/api/chat` are not served.

## 1. Start the server

ollaya ships as a container. With an NVIDIA GPU (needs the NVIDIA Container Toolkit,
or Docker Desktop with WSL2 GPU support on Windows):

```bash
docker run -d --name ollaya --gpus all \
  -p 127.0.0.1:11435:11435 \
  -v ollaya-models:/home/ollaya/.ollaya \
  --restart unless-stopped \
  ghcr.io/ollaya-dev/ollaya:cuda
```

- Port `11435` on localhost only. Hertz's default is `http://127.0.0.1:11435`
  (override with `OLLAYA_URL`).
- The volume keeps pulled models across container recreation (`OLLAYA_MODELS` in the
  image is `/home/ollaya/.ollaya/models`).

Check it:

```bash
curl http://127.0.0.1:11435/            # "Ollaya is running"
curl http://127.0.0.1:11435/api/version
```

## 2. Pull the models

```bash
docker exec ollaya ollaya pull laya:en             # 854 MB, English (ModernBERT-large)
docker exec ollaya ollaya pull laya:multilingual   # 684 MB, 100+ languages (mmBERT)
docker exec ollaya ollaya pull laya:latest         # 11 KB router: picks en or multilingual per request
docker exec ollaya ollaya list
```

Hertz defaults to `laya:en` (override with `OLLAYA_MODEL`).

## 3. Ask it something

`POST /api/decide` takes a `model`, a `state`, and named `questions`. A `choice`
question maps each option to a description of when it applies (at least two
options):

```bash
curl -s http://127.0.0.1:11435/api/decide -H 'content-type: application/json' -d '{
  "model": "laya:en",
  "state": "Radio call: Top Rabbit, come in, over.",
  "questions": { "pane": { "type": "choice", "criteria": {
    "Top Rabbit":        "The call addresses the pane named Top Rabbit",
    "Correct Alligator": "The call addresses the pane named Correct Alligator"
  }}}
}'
```

```json
{"answers":{"pane":{"type":"choice","choice":"Top Rabbit","confidence":0.9885,
  "probabilities":{"Top Rabbit":0.9923,"Correct Alligator":0.0036}}}, ...}
```

Other question types are `score` and `noul`. The first request after start loads
the model (a few seconds); after that a decision takes about 0.2 s on the GPU.

## 4. Use it from Hertz

From any Hyperia pane:

```bash
cargo run --release -p hertz-relay --bin hertz-panes
```

It pulls the window/tab/pane layout from Hyperia, sends it to ollaya, and prints
which tab and pane ollaya thinks are active next to what Hyperia reports.

## Other useful commands

```bash
docker exec ollaya ollaya ps         # loaded models
docker exec ollaya ollaya show laya:en
docker logs -f ollaya
docker exec ollaya ollaya mcp        # serve the models to agents over MCP
```
