//! Deterministic RFC + CURP synthesis. RFC follows the Mexican format
//! `[A-Z]{4}[0-9]{6}[A-Z0-9]{3}` (4 letters + birth date YYMMDD + 3 alnum
//! homoclave). CURP is 18 chars.
//!
//! These are **synthetic** — the validation digits and homoclave are not
//! computed against the real SAT algorithm, but the strings are
//! syntactically valid format-wise and globally unique within the
//! generator's seed.

use rand::RngExt;

const LETTERS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

/// Build an RFC string from a deterministic ordinal. Same ordinal → same RFC.
///
/// **Uniqueness guarantee**: the (date, homoclave) tuple is a strict
/// bijection with `ordinal` in the range `0..(80*12*28*36*36*36) =
/// 1 254 113 280`, so different ordinals always produce different
/// RFCs. The 4 leading letters are decorative (derived from the
/// ordinal but not required for uniqueness).
///
/// Earlier versions of this function used a splitmix-style homoclave
/// without ordinal stamping; the date cycle (26 880) plus a
/// non-injective homoclave caused collisions at ≥~30 K ordinals. At
/// Scale 0.1 (150 K clients) the bench escaped via PRNG-rare
/// collisions; at Scale 1.0 (1.5 M clients) the duplicate-key error
/// was deterministic on the 218th client. The fix base-36 encodes
/// `(ordinal / 26 880)` into the homoclave so the date+homoclave
/// product space exceeds 1.25 G unique ordinals. (Finding 15.)
///
/// The `_rng` parameter is retained for ABI continuity with call
/// sites that already thread an RNG through; it is unused here.
pub fn rfc_for_ordinal(_rng: &mut impl RngExt, ordinal: u64) -> String {
    let mut s = String::with_capacity(13);
    // 4 letters: decorative permutation from the ordinal (not required
    // for uniqueness; the date+homoclave bijection below carries it).
    for i in 0..4 {
        let pick = (ordinal
            .wrapping_add(i as u64)
            .wrapping_mul(0x9E3779B97F4A7C15)) as usize
            % LETTERS.len();
        s.push(LETTERS[pick] as char);
    }
    // 6 digits (birth date YYMMDD): bijective decomposition of the
    // ordinal into yy ∈ 0..80, mm ∈ 1..12, dd ∈ 1..28 → 26 880 distinct
    // dates per cycle.
    let yy = (ordinal % 80) as u8 + 50; // 50-79 → 1950-1979
    let mm = ((ordinal / 80) % 12) as u8 + 1; // 1-12
    let dd = ((ordinal / (80 * 12)) % 28) as u8 + 1; // 1-28
    use std::fmt::Write;
    write!(s, "{:02}{:02}{:02}", yy % 100, mm, dd).unwrap();
    // 3-char homoclave: base-36 encoding of `ordinal / 26 880`. With
    // 36³ = 46 656 distinct homoclaves per date, the total RFC space
    // covered is 26 880 × 46 656 ≈ 1.25 G — sufficient for any
    // practical bench scale.
    const DATE_PERIOD: u64 = 80 * 12 * 28;
    let mut h = ordinal / DATE_PERIOD;
    for _ in 0..3 {
        s.push(ALNUM[(h as usize) % ALNUM.len()] as char);
        h /= ALNUM.len() as u64;
    }
    s
}

pub fn curp_for_rfc(rfc: &str, ordinal: u64) -> String {
    // CURP is 18 chars; first 4 letters typically share with RFC, then
    // birth date, then sex + estado + 3-letter consonants + homoclave.
    let mut s = String::with_capacity(18);
    s.push_str(&rfc[..4]);
    s.push_str(&rfc[4..10]); // YYMMDD
    s.push(if ordinal % 2 == 0 { 'H' } else { 'M' }); // sexo
    // 2 letters estado
    let est = ordinal as usize % LETTERS.len();
    s.push(LETTERS[est] as char);
    s.push(LETTERS[(est + 7) % LETTERS.len()] as char);
    // 3 internal consonants (deterministic)
    for i in 0..3 {
        let p = (ordinal.wrapping_add(0x100 * i as u64)) as usize % LETTERS.len();
        s.push(LETTERS[p] as char);
    }
    // 2 homoclave alnum
    for i in 0..2 {
        let p = (ordinal.wrapping_add(0x200 * i as u64)) as usize % ALNUM.len();
        s.push(ALNUM[p] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand_chacha::ChaCha20Rng;
    use std::collections::HashSet;

    fn rng() -> ChaCha20Rng {
        ChaCha20Rng::from_seed([0u8; 32])
    }

    #[test]
    fn rfc_unique_at_scale_1() {
        // Bench Scale 1.0 = 1 500 000 clients. Verify the bijection.
        let mut rng = rng();
        let n: u64 = 1_500_000;
        let mut seen: HashSet<String> = HashSet::with_capacity(n as usize);
        for ord in 0..n {
            let r = rfc_for_ordinal(&mut rng, ord);
            assert_eq!(r.len(), 13, "rfc {} not 13 chars: {}", ord, r);
            assert!(
                seen.insert(r.clone()),
                "duplicate RFC {} at ordinal {}",
                r,
                ord
            );
        }
        assert_eq!(seen.len() as u64, n);
    }

    #[test]
    fn rfc_deterministic_per_ordinal() {
        // Same ordinal → same RFC across calls / RNG states.
        let mut a = rng();
        let mut b = rng();
        // Advance b's RNG so its state diverges; output must still match.
        for _ in 0..1000 {
            let _: u32 = b.random();
        }
        for ord in [0u64, 1, 42, 26_879, 26_880, 100_000, 999_999] {
            assert_eq!(rfc_for_ordinal(&mut a, ord), rfc_for_ordinal(&mut b, ord));
        }
    }
}
