# Transcription service (Whisper)

hertz's second transcriber. Whistle transcribes every call on the device in
about 0.2 s. After the relay delivers a call, it sends the recording here and
gets back Whisper's version, which is slower and better on names:
- if that version differs, a follow-up goes to the same pane;
- if the on-device transcript named no one, routing is tried again on this one.

Without the service, hertz logs `transcription service down` and skips both.

This is a copy of `nemesis8/services/transcription`, kept here so hertz can
deploy its own. It's changed to hertz's port 8765 and published on loopback only.

## Run it

GPU (NVIDIA, model `medium`, what hertz has used):

```sh
cd deploy/transcription
docker compose up -d --build
curl http://127.0.0.1:8765/health
```

CPU only (model `base`):

```sh
docker compose -f docker-compose.cpu.yml up -d --build
```

Another model: `WHISPER_MODEL=small docker compose up -d --build`. The model is
downloaded into the image at build time, so the container starts ready.

The first GPU build downloads CUDA PyTorch, several GB.

## How hertz finds it

`[relay] verify_url` (default `http://localhost:8765`) and `verify_model`
(default `medium`). Set `verify_url = ""` to turn the double-check off.

## API

| Method and path | Does |
|---|---|
| `POST /transcribe` | Multipart `file` (WAV), optional `job_id`, `model`, `callback_url` → `{job_id, status: "queued"}` |
| `GET /status/{job_id}` | `queued`, `processing`, `completed` or `failed` |
| `GET /download/{job_id}` | The transcript report (text) |
| `POST /classify` | Coarse ambient-sound labels for a WAV |
| `GET /health` | Model loaded, GPU, queue counts |

There's no auth. Inside the container it binds 0.0.0.0, but the compose files
publish it only on 127.0.0.1. Don't publish it to the LAN.
