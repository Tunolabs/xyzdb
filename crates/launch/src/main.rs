//! Container entrypoint: pick the engine build this CPU can actually run.
//!
//! # Why this exists
//!
//! `.cargo/config.toml` compiles every `x86_64-unknown-linux-gnu` build with
//! `target-cpu=x86-64-v3`, so the published amd64 engine binary **requires** AVX2,
//! FMA, BMI1/2 and the rest of that feature level. On an x86-64 without them the
//! process dies with SIGILL at the first vector instruction — before it can print
//! anything useful. The image was therefore fast on modern hardware and unusable
//! on older hardware, with no way for the operator to tell in advance.
//!
//! The image now carries both builds and this launcher chooses between them, so a
//! user needs to know nothing and pick nothing.
//!
//! # Why that is safe here specifically
//!
//! Because the two builds are not two answers. `target-cpu=x86-64` (SSE2) and
//! `target-cpu=x86-64-v3` (AVX2) produce **byte-identical** similarity scores —
//! the vector kernel has no FMA, so widening 2×SSE2 to one 256-bit multiply is the
//! same math in the same accumulation order. That equality is a CI-checked gate
//! (`crates/core/tests/score_bit_identity.rs` runs under both RUSTFLAGS and fails
//! on divergence), not an assumption. Without that gate, shipping two binaries
//! would mean shipping two possible results, and this design would be wrong.
//!
//! # Why a binary rather than a shell script
//!
//! The runtime image is `distroless/cc`, which has no shell. This is ~40 lines
//! with no dependencies, compiled at the baseline ISA so it is guaranteed to
//! start wherever the container starts.

// SPDX-License-Identifier: BUSL-1.1
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

/// Base path of the binary to launch; `.v3` / `.v2` are appended. Overridable
/// because the same launcher fronts two images: the engine (`xyzdb-server`) and
/// the MCP server (`xyzdb-mcp`, which links the engine in `--embed` mode and so
/// carries the same ISA baseline). One launcher, two entrypoints, no second copy
/// of this logic to drift.
const DEFAULT_BASE: &str = "/usr/local/bin/xyzdb-server";

fn base() -> String {
    std::env::var("XYZDB_LAUNCH_TARGET").unwrap_or_else(|_| DEFAULT_BASE.to_string())
}

/// Does this CPU implement the whole `x86-64-v3` level?
///
/// Checking `avx2` alone would be wrong: `target-cpu=x86-64-v3` also emits FMA,
/// BMI1/BMI2, F16C, LZCNT and MOVBE, and a CPU with AVX2 but without BMI2 would
/// still SIGILL. The list is the level's definition, not a guess at which
/// instruction the compiler happened to pick.
#[cfg(target_arch = "x86_64")]
fn cpu_has_v3() -> bool {
    is_x86_feature_detected!("avx")
        && is_x86_feature_detected!("avx2")
        && is_x86_feature_detected!("bmi1")
        && is_x86_feature_detected!("bmi2")
        && is_x86_feature_detected!("f16c")
        && is_x86_feature_detected!("fma")
        && is_x86_feature_detected!("lzcnt")
        && is_x86_feature_detected!("movbe")
        && is_x86_feature_detected!("popcnt")
        && is_x86_feature_detected!("sse4.2")
        && is_x86_feature_detected!("xsave")
}

#[cfg(not(target_arch = "x86_64"))]
fn cpu_has_v3() -> bool {
    // On aarch64 the image ships one build and the flag never applied.
    false
}

/// Present AND executable. Checking only `exists()` was not enough: with the v3
/// binary present but unusable, the launcher tried it, `exec` failed with EACCES
/// and it exited instead of falling back — verified by mounting /dev/null over
/// the v3 path, which is exactly the shape of a half-built image.
fn usable(p: &str) -> bool {
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn main() -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let base = base();
    let v3 = format!("{base}.v3");
    let v2 = format!("{base}.v2");

    // Preference order, best first. Each candidate carries why it would be
    // chosen, so the message explains the decision rather than announcing it.
    let mut candidates: Vec<(&str, &str)> = Vec::new();
    if cpu_has_v3() {
        candidates.push((v3.as_str(), "CPU implements x86-64-v3 (AVX2/FMA/BMI2)"));
        candidates.push((
            v2.as_str(),
            "fell back after the v3 build could not be executed",
        ));
    } else if cfg!(target_arch = "x86_64") {
        candidates.push((
            v2.as_str(),
            "CPU does not implement x86-64-v3; using the portable baseline build",
        ));
    } else {
        candidates.push((v2.as_str(), "single build in this image"));
    }

    let mut tried = 0;
    for (path, why) in &candidates {
        if !usable(path) {
            continue;
        }
        tried += 1;
        // Say which one, on stderr, before handing over. The selection being
        // automatic is no reason for it to be invisible: an operator comparing
        // two hosts needs to see that one took the baseline path.
        eprintln!(
            "xyzdb-launch: exec {} ({why})",
            Path::new(path)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        );
        // exec, not spawn: the engine must be PID 1's process image so signals
        // and exit codes reach it unchanged. `graceful_shutdown` is wired to
        // SIGTERM (OPERATIONS.md §9), and a wrapper that forwarded signals
        // imperfectly would silently turn a clean drain into a WAL replay.
        //
        // Past this call the process IS the engine, so anything below only runs
        // when exec itself failed.
        let err = Command::new(path).args(&args).exec();
        eprintln!("xyzdb-launch: exec {path} failed: {err}");
    }

    if tried == 0 {
        eprintln!(
            "xyzdb-launch: no usable binary in this image (looked for {v3} then {v2}) \
             — it was built wrong; refusing to guess"
        );
        std::process::exit(78); // EX_CONFIG
    }
    // Every candidate was present and every exec failed. Not a configuration
    // problem any more: something is wrong with the binaries themselves.
    eprintln!("xyzdb-launch: every engine build in this image failed to exec");
    std::process::exit(126);
}

// What this CANNOT do, stated so nobody relies on it: if the feature detection
// above ever says yes on a CPU that then faults, the fault arrives as SIGILL
// inside the engine, after a successful exec. There is no fallback from that —
// the detection IS the protection. The fallback here covers a bad image, not a
// bad prediction.
