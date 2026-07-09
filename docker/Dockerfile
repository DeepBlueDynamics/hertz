# Multi-stage Dockerfile for Hertz
# Builder stage
FROM rust:1-bookworm AS builder

# Install build dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libusb-1.0-0-dev \
    libasound2-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/hertz

# Copy the workspace files
COPY Cargo.toml ./
COPY crates/ crates/
COPY bandplans/ bandplans/

# Build the daemon (required) and the TUI (best-effort while it's in flight)
RUN cargo build --release -p hertz-daemon && \
    (cargo build --release -p hertz-tui || echo "hertz-tui build skipped (in development)")

# Normalize binary names into /out regardless of crate [[bin]] renames
# (hertz-daemon builds as "hertzd" once its bin rename lands; "hertz-daemon" until then)
RUN mkdir -p /out && \
    (cp target/release/hertzd /out/hertzd 2>/dev/null || cp target/release/hertz-daemon /out/hertzd) && \
    (cp target/release/hertz /out/hertz 2>/dev/null || cp target/release/hertz-tui /out/hertz 2>/dev/null || \
     (printf '#!/bin/sh\necho "hertz tui: not included in this image build"\n' > /out/hertz && chmod +x /out/hertz))

# Runtime stage
FROM debian:bookworm-slim AS runtime

# Install runtime dependencies (ca-certificates for external APIs)
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libasound2 \
    && rm -rf /var/lib/apt/lists/*

# Create non-root user and directories
RUN groupadd -g 10001 hertz && \
    useradd -u 10001 -g hertz -m -s /bin/bash hertz && \
    mkdir -p /etc/hertz /data && \
    chown -R hertz:hertz /etc/hertz /data

# Copy release binaries from builder stage
COPY --from=builder /out/hertzd /usr/local/bin/hertzd
COPY --from=builder /out/hertz /usr/local/bin/hertz
COPY --from=builder /usr/src/hertz/bandplans /usr/share/hertz/bandplans

USER hertz
VOLUME ["/data"]

EXPOSE 9080

ENTRYPOINT ["hertzd"]
CMD ["--config", "/etc/hertz/hertz.toml"]
