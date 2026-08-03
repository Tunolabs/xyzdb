# Pin builder to bookworm so glibc matches the bookworm-slim runtime
# below. Upstream `rust:slim` rolled forward from bookworm to trixie
# in 2026-Q2 (glibc 2.36 → 2.38), which made the runtime fail to load
# the binary with "GLIBC_2.38 not found" — discovered during v0.4
# cp 6.2.3a soak smoke (finding H10 in cycle plan §8).
FROM rust:slim-bookworm AS builder
# Provided automatically by buildx (linux/amd64 → amd64, linux/arm64 → arm64).
ARG TARGETARCH
# Image variant recorded as the org.xyzdb.image-variant label (set in the final
# stage). The bench runner and the publish flow pass the explicit AVX2-baseline
# name (x86-v3 on amd64, arm on arm64); with no --build-arg the label falls back
# to the build arch. Declared here too so a --build-arg is accepted either stage.
ARG XYZ_IMAGE_VARIANT=""
# build-essential + pkg-config cover the C-compiling -sys crates (jemalloc,
# zstd, aws-lc, ring). libssl-dev was removed: openssl-sys is absent from
# Cargo.lock (TLS is rustls/ring, self-contained), so nothing links or
# build-probes libssl. ca-certificates kept for the build environment.
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential pkg-config ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
# Single-workspace layout (0.9): the whole workspace builds from the root.
# .cargo/config.toml carries the target-cpu=x86-64-v3 flag for the
# x86_64-linux publish build (this image); other triples inherit nothing.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY .cargo/ .cargo/
COPY crates/ crates/
COPY tools/ tools/
# Arch is detected with `uname -m`, NOT with $TARGETARCH. TARGETARCH is populated
# only by buildx; with the classic builder — which is what you get from
# `apt install docker.io` on a current Ubuntu, where buildx is absent — it is
# EMPTY. The previous version of this guard tested it and therefore took the
# "arm baseline" branch on an AVX2 x86 host, printing the opposite of the truth
# and skipping the assertion it exists to make. `uname -m` is always there.
#
# v0.6.2 §8 — fat LTO + one codegen unit so the image binary == the tagged release
# artifact even if [profile.release] is ever relaxed.
#
# On x86-64 the engine is built TWICE and both go in the image:
#   .v3  target-cpu=x86-64-v3 (inherited from .cargo/config.toml) — AVX2/FMA/BMI2
#   .v2  target-cpu=x86-64    (explicit RUSTFLAGS, which overrides the config)
# `xyzdb-launch` picks one at startup from the CPU's actual feature set. Shipping
# only .v3 is what made the amd64 image SIGILL on a pre-AVX2 host; shipping only
# .v2 would give up ~the whole point of the v3 flip. Both is the only option that
# needs no decision from whoever runs it.
#
# This is safe ONLY because the two builds are not two answers: v2 and v3 produce
# byte-identical scores, gated in CI by crates/core/tests/score_bit_identity.rs.
# If that gate ever goes, this dual-build has to go with it.
RUN set -e; \
    ARCH="$(uname -m)"; \
    mkdir -p /build/out; \
    export CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1; \
    if [ "$ARCH" = "x86_64" ]; then \
      grep -q 'target-cpu=x86-64-v3' .cargo/config.toml \
        || { echo 'ERROR: target-cpu=x86-64-v3 missing from .cargo/config.toml for an x86_64 build'; exit 1; }; \
      echo "xyzdb image: x86_64 — building BOTH v3 (AVX2) and v2 (baseline); the launcher selects at startup"; \
      cargo build --release -p xyzdb-server; \
      cp target/release/xyzdb-server /build/out/xyzdb-server.v3; \
      RUSTFLAGS="-C target-cpu=x86-64" cargo build --release -p xyzdb-server; \
      cp target/release/xyzdb-server /build/out/xyzdb-server.v2; \
    else \
      echo "xyzdb image: $ARCH — one build; the x86 flag is target-scoped and inert here"; \
      cargo build --release -p xyzdb-server; \
      cp target/release/xyzdb-server /build/out/xyzdb-server.v2; \
    fi; \
    RUSTFLAGS="$([ "$ARCH" = x86_64 ] && echo '-C target-cpu=x86-64')" \
      cargo build --release -p xyzdb-launch; \
    cp target/release/xyzdb-launch /build/out/xyzdb-launch; \
    echo "--- image payload:"; ls -la /build/out/; \
    test -x /build/out/xyzdb-launch || { echo 'ERROR: launcher missing'; exit 1; }; \
    test -x /build/out/xyzdb-server.v2 || { echo 'ERROR: baseline engine missing'; exit 1; }; \
    if [ "$ARCH" = "x86_64" ]; then \
      test -x /build/out/xyzdb-server.v3 \
        || { echo 'ERROR: x86_64 image without the v3 build — the launcher would always take the slow path'; exit 1; }; \
    fi

# Runtime: distroless/cc-debian12 (glibc 2.36 + libgcc + libstdc++, ~34 MB)
# instead of debian:bookworm-slim (~97 MB). debian12 == bookworm, so the
# glibc-match rationale above (finding H10) still holds. The binary links
# only glibc/libgcc/libm (no openssl — TLS is rustls, optional), so cc has
# everything it needs. Net image 106 MB -> ~43 MB. Trade-off: no shell or
# package manager in the runtime (no `docker exec sh`); for debugging,
# attach a debian-slim sidecar sharing the namespace.
FROM gcr.io/distroless/cc-debian12
# Re-declared so the label captures the caller's value. With no --build-arg it
# falls back to the build arch ($TARGETARCH -> amd64/arm64), never empty; an
# explicit XYZ_IMAGE_VARIANT (x86-v3 / arm, as the bench and publish flow pass)
# wins. A LABEL cannot map amd64 -> x86-v3 on its own, so the explicit AVX2 name
# is set by whoever builds for publication.
ARG TARGETARCH
ARG XYZ_IMAGE_VARIANT=""
# `$TARGETARCH` is empty under the classic builder (see the arch note above), so
# this label used to come out as an empty string — a stamp that says nothing while
# looking like it says something. The fallback is now explicit, and the
# authoritative fact lives in the label below.
LABEL org.xyzdb.image-variant="${XYZ_IMAGE_VARIANT:-runtime-selected}"
# On x86_64 this is a DUAL-ISA image: it carries the baseline and the AVX2 engine
# and selects per host. Do not tag it `x86-v3` — that name says the image only
# runs where v3 does, which is what this stopped being.
LABEL org.xyzdb.isa-selection="runtime (xyzdb-launch); x86_64 carries v2+v3, other arches one build"
COPY --from=builder /build/out/ /usr/local/bin/
EXPOSE 2505
VOLUME /data
# The launcher execs the engine, so the engine still ends up as PID 1's process
# image and receives SIGTERM directly (graceful drain, OPERATIONS.md §9).
ENTRYPOINT ["xyzdb-launch"]
# The image binds 0.0.0.0 so a running container is reachable. That is a
# non-loopback bind, so the server refuses to start without authentication:
# pass `--auth-token <file>` (the secure default). To run open on purpose,
# append `--insecure-allow-no-auth` at `docker run` time — never bake it in.
CMD ["--port", "2505", "--path", "/data/xyzdb", "--bind", "0.0.0.0"]
