# T5 — Container + runtime packaging (Cognitive Haddock 🌀)

Phase 9 basics pulled forward: the user wants the stack runnable in Docker with USB
passthrough ASAP. Do the T3 cleanup first, then containerize. PLAN §9 is the spec.

## Part 0 — T3 cleanup (before anything else)
1. **Pick ONE driver crate.** Your Cargo.toml still ships both `librtlsdr-rs` and
   `rtl-sdr-rs` (plus direct `rusb`). Decide (you've now used the APIs), remove the loser
   from hertz-sdr AND workspace deps, record the rationale in STATUS. Keep `rusb` only if
   your code calls it directly (enumeration).
2. Fix the 3 clippy warnings in `examples/dump_fft.rs` (needless range loops).
3. `cargo clippy --workspace --all-targets` must come back clean.

## Your paths
- `docker/**` (new), `.dockerignore`, `hertz.example.toml` (repo root)
- `docs/INSTALL.md`, `docs/OPERATIONS.md` (new)
- `crates/hertz-sdr` only for Part 0.

## Part 1 — Dockerfile (`docker/Dockerfile`)
- Multi-stage: `rust:1-bookworm` builder → `debian:bookworm-slim` runtime.
- Build workspace release binaries; runtime stage gets `hertzd` + `hertz` (hertz-tui may
  still be a stub binary — that's fine, wire it in so the image is future-proof; if the
  crate has no bin yet add a trivial `main.rs` printing "hertz tui: not yet implemented"
  — that is an allowed exception to path ownership, coordinate via STATUS).
- Runtime deps: `ca-certificates`, `libusb-1.0-0` (rusb links libusb dynamically on Linux
  unless the `vendored` feature is on — check hertz-sdr's resolved features and prefer
  vendored libusb so the runtime image needs nothing; document which way you went).
- Non-root user, `/data` volume, `ENTRYPOINT ["hertzd"]`, `CMD ["--config","/etc/hertz/hertz.toml"]`.
- `.dockerignore`: target/, data/, plan/reference, recordings.

## Part 2 — compose + config
- `docker/compose.yaml` per PLAN §9: `/dev/bus/usb` device mapping, cgroup rule c 189:*,
  port 9080, `./data:/data`, `./hertz.toml:/etc/hertz/hertz.toml:ro`, HERTZ_TOKEN env,
  restart unless-stopped, container_name hertzd.
- `hertz.example.toml`: two-dongle example straight from PLAN §3 (marine channelized +
  hopscan group commented out as "Phase 5"), transcription disabled, sensible data_dir.
- `docker/60-hertz-rtlsdr.rules`: udev rules for 0bda:2832 / 0bda:2838 (MODE 0666,
  TAG+="uaccess").

## Part 3 — docs
- `docs/INSTALL.md`: the driver matrix from PLAN §9 verbatim-expanded — Linux
  (blacklist dvb_usb_rtl28xxu + udev + powered hub), Windows (usbipd-win → WSL2 flow:
  winget install, usbipd list/bind/attach --wsl, re-attach on replug, then compose from
  WSL2; Zadig/WinUSB documented as the native-dev fallback), macOS (no container USB —
  native or Linux box). Include the `docker exec -it hertzd hertz tui` and remote
  `--connect` invocations.
- `docs/OPERATIONS.md`: start/stop/logs, where recordings land, how to add a bandplan,
  how to read `hertz doctor` output.

## Part 4 — verification (in your container, no USB needed)
- `docker build` succeeds (you have docker? if not: verify the Dockerfile stages by
  running the exact builder commands in your container — cargo build --release — and note
  that image build itself was not run; the supervisor builds it on the host).
- Compose file validates: `docker compose -f docker/compose.yaml config` (or note if
  docker absent).

## Done means
Part 0 clean workspace clippy; Dockerfile/compose/config/docs written; STATUS updated with
driver decision, libusb linking mode, and what you could/could not verify in-container.
