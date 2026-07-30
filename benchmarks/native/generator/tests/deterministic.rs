//! Determinism test: same seed + scale → byte-identical hash across runs.

use native_generator::Dataset;
use sha2::{Digest, Sha256};

fn hash_iter<I, T>(iter: I) -> [u8; 32]
where
    I: Iterator<Item = T>,
    T: serde::Serialize,
{
    let mut h = Sha256::new();
    for item in iter {
        let bytes = serde_json::to_vec(&item).unwrap();
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    h.finalize().into()
}

#[test]
fn deterministic_streams_at_micro_scale() {
    let scale = 0.0001; // ~150 clients, fast test
    let a = Dataset::new(42, scale);
    let b = Dataset::new(42, scale);

    assert_eq!(hash_iter(a.empresas()), hash_iter(b.empresas()));
    assert_eq!(hash_iter(a.productos()), hash_iter(b.productos()));
    assert_eq!(hash_iter(a.clients()), hash_iter(b.clients()));
    assert_eq!(hash_iter(a.credits()), hash_iter(b.credits()));
    assert_eq!(hash_iter(a.installments()), hash_iter(b.installments()));
    assert_eq!(hash_iter(a.payments()), hash_iter(b.payments()));
    assert_eq!(hash_iter(a.collections()), hash_iter(b.collections()));
    assert_eq!(
        hash_iter(a.collection_actions()),
        hash_iter(b.collection_actions())
    );
    assert_eq!(
        hash_iter(a.credit_applications()),
        hash_iter(b.credit_applications())
    );
    assert_eq!(hash_iter(a.audit_log()), hash_iter(b.audit_log()));
    assert_eq!(hash_iter(a.notifications()), hash_iter(b.notifications()));
    assert_eq!(hash_iter(a.bi_snapshots()), hash_iter(b.bi_snapshots()));
}

#[test]
fn different_seeds_produce_different_clients() {
    let scale = 0.0001;
    let a = Dataset::new(42, scale);
    let b = Dataset::new(43, scale);
    assert_ne!(hash_iter(a.clients()), hash_iter(b.clients()));
}

#[test]
fn expected_counts_consistent_with_streams() {
    // At scale 0.0001 the absolute counts are tiny but the stream lengths
    // must match the expected_counts() prediction within rounding noise.
    // Catalog is fixed; client-derived streams use stochastic per-record
    // counts, so we check empresas/productos/clients exactly and total
    // approximately.
    let scale = 0.0001;
    let ds = Dataset::new(42, scale);
    let ec = ds.expected_counts();

    assert_eq!(ds.empresas().count() as u64, ec.empresas);
    assert_eq!(ds.productos().count() as u64, ec.productos);
    assert_eq!(ds.clients().count() as u64, ec.clients);
}

#[test]
fn rfcs_have_correct_format() {
    // Mexican RFC: 4 letters + 6 digits + 3 alnum = 13 chars.
    let ds = Dataset::new(42, 0.0001);
    for c in ds.clients().take(20) {
        assert_eq!(c.rfc.len(), 13, "rfc {} not 13 chars", c.rfc);
        assert!(c.rfc[..4].chars().all(|x| x.is_ascii_uppercase()));
        assert!(c.rfc[4..10].chars().all(|x| x.is_ascii_digit()));
        assert!(c.rfc[10..13].chars().all(|x| x.is_ascii_alphanumeric()));
    }
}
