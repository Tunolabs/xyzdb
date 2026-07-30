//! Dimension validation on PUT.
//!
//! A lobe's searchable vector field learns its dimension from the first embedding
//! written and enforces it thereafter: a later vector of a different dimension is
//! REJECTED at ingest instead of being silently dropped from every NEAREST top-k
//! at query time (`as_vector` skips a mismatched-dimension candidate with no
//! signal). Lossless — only malformed / wrong-model data is refused — and
//! flexible: each field may be any dimension; only mixing dimensions *within one
//! field* is closed. The learned dimension is durable across restart.

use xyzdb_engine::engine::Engine;

/// A `dim`-length all-float list literal → packs as a `Value::Vector` (the
/// packing threshold is 64 dims). `{:?}` on an `f32` renders the float form
/// (`0.5`), so the whole list is all-float and never a plain integer list.
fn vec_lit(dim: usize) -> String {
    let parts: Vec<String> = (0..dim)
        .map(|i| format!("{:?}", (i % 10) as f32 + 0.5))
        .collect();
    format!("[{}]", parts.join(","))
}

#[test]
fn learns_dim_on_first_put_then_rejects_a_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path()).unwrap();
    engine.run(r#"LOBE "mem""#).unwrap();
    engine.run(r#"VECTOR emb IN "mem""#).unwrap();

    // The first embedding fixes the field at 64 dimensions.
    engine
        .run(&format!(r#"PUT {{id:"a", emb:{}}} IN "mem""#, vec_lit(64)))
        .expect("first 64-dim PUT learns the dim");
    // A matching vector is accepted.
    engine
        .run(&format!(r#"PUT {{id:"b", emb:{}}} IN "mem""#, vec_lit(64)))
        .expect("matching 64-dim PUT");

    // A different dimension is a hard error — not a silent skip.
    let err = engine
        .run(&format!(r#"PUT {{id:"c", emb:{}}} IN "mem""#, vec_lit(128)))
        .expect_err("128-dim into a 64-dim field must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("dimension") && msg.contains("128") && msg.contains("64"),
        "expected a clear dim-mismatch error naming both dims, got: {msg}"
    );
}

#[test]
fn learned_dim_persists_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let engine = Engine::open(dir.path()).unwrap();
        engine.run(r#"LOBE "mem""#).unwrap();
        engine.run(r#"VECTOR emb IN "mem""#).unwrap();
        engine
            .run(&format!(r#"PUT {{id:"a", emb:{}}} IN "mem""#, vec_lit(64)))
            .unwrap();
        // Engine::drop → shutdown persists the learned spec (dim = 64) durably.
    }
    // Re-open: the dimension must have survived to disk (0x02 slot).
    let engine = Engine::open(dir.path()).unwrap();
    let err = engine
        .run(&format!(r#"PUT {{id:"c", emb:{}}} IN "mem""#, vec_lit(128)))
        .expect_err("dim not enforced after restart → not persisted");
    assert!(
        err.to_string().contains("dimension"),
        "post-restart error: {err}"
    );
    // A matching vector still writes.
    engine
        .run(&format!(r#"PUT {{id:"d", emb:{}}} IN "mem""#, vec_lit(64)))
        .expect("64-dim still accepted after restart");
}
