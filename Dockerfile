# Pin builder to bookworm so glibc matches the bookworm-slim runtime
# below. Upstream `rust:slim` rolled forward from bookworm to trixie
# in 2026-Q2 (glibc 2.36 → 2.38), which made the runtime fail to load
# the binary with "GLIBC_2.38 not found" — discovered during v0.4
# cp 6.2.3a soak smoke (finding H10 in cycle plan §8).
FROM rust:slim-bookworm AS builder
# Provided automatically by buildx (linux/amd64 → amd64, linux/arm64 → arm64).
ARG TARGETARCH
# Image variant recorded in the bench report. Defaults from the build arch when
# the caller passes nothing: amd64 → x86-v3 (AVX2), arm64 → arm.
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
# Confirm the AVX2 baseline flag is present in the build context for the x86
# image. It is target-scoped to x86_64-unknown-linux-gnu, so it applies to the
# amd64 build and is inert on arm64. Fail loudly if it is ever silently dropped
# so an x86-v3 image can never be published as a plain baseline by mistake.
RUN if [ "$TARGETARCH" = "amd64" ]; then \
      grep -q 'target-cpu=x86-64-v3' .cargo/config.toml \
        || { echo 'ERROR: target-cpu=x86-64-v3 missing from .cargo/config.toml for amd64 build'; exit 1; }; \
      echo "xyzdb image: amd64 / x86-v3 — target-cpu=x86-64-v3 (AVX2) applies"; \
    else \
      echo "xyzdb image: ${TARGETARCH} — x86-v3 flag inert (arm baseline)"; \
    fi
# v0.6.2 §8 — build with fat LTO + single codegen unit to match the
# production release artifact. The root [profile.release] already sets
# lto = "fat"; the env belt-and-suspenders keeps the image == the tagged
# artifact even if the profile is ever relaxed.
RUN CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
    cargo build --release -p xyzdb-server

# Runtime: distroless/cc-debian12 (glibc 2.36 + libgcc + libstdc++, ~34 MB)
# instead of debian:bookworm-slim (~97 MB). debian12 == bookworm, so the
# glibc-match rationale above (finding H10) still holds. The binary links
# only glibc/libgcc/libm (no openssl — TLS is rustls, optional), so cc has
# everything it needs. Net image 106 MB -> ~43 MB. Trade-off: no shell or
# package manager in the runtime (no `docker exec sh`); for debugging,
# attach a debian-slim sidecar sharing the namespace.
FROM gcr.io/distroless/cc-debian12
# Re-declare in this stage so the label captures the caller's value (or the
# default from the builder stage's auto-derivation when passed through).
ARG XYZ_IMAGE_VARIANT=""
LABEL org.xyzdb.image-variant="${XYZ_IMAGE_VARIANT}"
COPY --from=builder /build/target/release/xyzdb-server /usr/local/bin/
EXPOSE 2505
VOLUME /data
ENTRYPOINT ["xyzdb-server"]
# The image binds 0.0.0.0 so a running container is reachable. That is a
# non-loopback bind, so the server refuses to start without authentication:
# pass `--auth-token <file>` (the secure default). To run open on purpose,
# append `--insecure-allow-no-auth` at `docker run` time — never bake it in.
CMD ["--port", "2505", "--path", "/data/xyzdb", "--bind", "0.0.0.0"]
