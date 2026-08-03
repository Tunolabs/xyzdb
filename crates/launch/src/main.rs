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

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

const V3: &str = "/usr/local/bin/xyzdb-server.v3";
const V2: &str = "/usr/local/bin/xyzdb-server.v2";

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

fn main() -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Order matters: prefer v3 when the CPU allows it AND that build is in the
    // image, and fall through to the baseline otherwise. An image that ships only
    // one of the two (the arm64 case) still works without a special case.
    let (chosen, why) = if cpu_has_v3() && Path::new(V3).exists() {
        (V3, "CPU implements x86-64-v3 (AVX2/FMA/BMI2)")
    } else if Path::new(V2).exists() {
        let why = if cfg!(target_arch = "x86_64") && !cpu_has_v3() {
            "CPU does not implement x86-64-v3; using the portable baseline build"
        } else {
            "single build in this image"
        };
        (V2, why)
    } else {
        eprintln!(
            "xyzdb-launch: neither {V3} nor {V2} is present in this image — it was \
             built wrong; refusing to guess"
        );
        std::process::exit(78); // EX_CONFIG
    };

    // Say which one, on stderr, before handing over. The selection being
    // automatic is no reason for it to be invisible: an operator comparing two
    // hosts needs to see that one took the baseline path.
    eprintln!(
        "xyzdb-launch: exec {} ({})",
        Path::new(chosen)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy(),
        why
    );

    // exec, not spawn: the engine must be PID 1's process image so signals and
    // exit codes reach it unchanged. `graceful_shutdown` is wired to SIGTERM
    // (OPERATIONS.md §9), and a wrapper that forwarded signals imperfectly would
    // silently turn a clean drain into a WAL replay on the next start.
    let err = Command::new(chosen).args(&args).exec();
    eprintln!("xyzdb-launch: exec {chosen} failed: {err}");
    std::process::exit(126);
}
