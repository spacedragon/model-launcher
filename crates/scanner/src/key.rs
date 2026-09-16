//! Deterministic key and identity derivation, and collision resolution.
//!
//! # Why this is not "derive a slug and hope"
//!
//! `models.key` is `UNIQUE` and is the readable identifier every API path uses
//! (`docs/api.md` §5), so two artifacts can never share one. Three properties
//! matter and are all delivered here:
//!
//! 1. **Stability** — [`model_id`] and the key of a file must not change across
//!    a daemon restart, an index rebuild, or a delete/re-appear cycle;
//! 2. **Order independence** — resolving a collision by "the first one wins"
//!    would make the index depend on filesystem enumeration order, so the same
//!    tree could yield different keys on two machines (or two scans). Here the
//!    suffix is derived from the *sorted set of absolute paths*;
//! 3. **No new dependency** — the workspace carries no hash/`uuid` crate (see
//!    `docs/architecture.md` §2), so the stable id is an RFC 4122 version-5
//!    UUID seeded by a local SHA-256 implementation.
//!
//! # Key algorithm
//!
//! - [`base_key`]: the file stem, lowercased, with every non-ASCII-alphanumeric
//!   character mapped to `-` and separator runs collapsed (`Qwen2.5-7B GGUF`
//!   → `qwen2-5-7b-gguf`). An empty result becomes `model`.
//! - A base key that is unique within the scan **and** unused by the existing
//!   index is used as-is.
//! - Otherwise the colliding artifacts are ranked by their absolute path and
//!   the lexicographically smallest path takes `<base>-1`, the next takes
//!   `<base>-2`, … the largest takes `<base>-n`, where `n` is the number of
//!   artifacts claiming that base key. Ranking on the path — not on the
//!   position in the walk — is what makes the mapping deterministic.
//! - Suffixes already present in the index are skipped when the family is
//!   handed out, so a `-1` key held by a row this scan does not own (for
//!   example another root's soft-deleted row, or an admin-renamed row) is never
//!   assigned a second time.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use model_serving_domain::model::ArtifactKind;

/// Extensions the scanner indexes, matched case-insensitively.
pub(crate) const GGUF_EXTENSION: &str = "gguf";
pub(crate) const NINFER_EXTENSION: &str = "ninfer";

/// The keys already occupied in the index plus the keys claimed earlier in the
/// current scan.
pub type Reserved = BTreeSet<String>;

/// The file stem of `path` reduced to a key-safe token, or `"model"` when
/// nothing usable remains.
#[must_use]
pub fn base_key(path: &str) -> String {
    let stem = Path::new(path)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let lowered = stem.to_lowercase();
    let trimmed: String = lowered
        .chars()
        .map(|ch| {
            if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let mut collapsed = String::with_capacity(trimmed.len());
    let mut previous_dash = false;
    for ch in trimmed.trim_matches('-').chars() {
        if ch == '-' {
            if !previous_dash {
                collapsed.push(ch);
            }
            previous_dash = true;
        } else {
            collapsed.push(ch);
            previous_dash = false;
        }
    }
    if collapsed.is_empty() {
        "model".to_owned()
    } else {
        collapsed
    }
}

/// Stable RFC 4122 version-5 UUID for an artifact path.
///
/// Deterministic in the canonical path, so the same file always maps to the
/// same row identity — across restarts, index rebuilds and delete/re-appear
/// cycles. Two distinct paths never share an id unless SHA-256 itself collides.
#[must_use]
pub fn model_id(path: &str) -> String {
    uuid_from_bytes(path.as_bytes())
}

/// The `n`-suffixed alternatives for `base`, skipping every suffix already
/// occupied by `reserved`.
///
/// Returned in rank order: rank 0 — the lexicographically smallest path —
/// takes the lowest free suffix. The `-<n>` family is reserved against the
/// existing index, so a key another row still holds (for example an
/// admin-renamed `shared-1`) is never handed out a second time.
#[must_use]
pub fn collision_candidates(base: &str, count: usize, reserved: &Reserved) -> Vec<String> {
    let mut out = Vec::with_capacity(count);
    let mut suffix = 1usize;
    while out.len() < count {
        let candidate = format!("{base}-{suffix}");
        if !reserved.contains(&candidate) {
            out.push(candidate);
        }
        suffix += 1;
    }
    out
}

/// Assign a key to every artifact path.
///
/// `candidates[i]` is the base key of artifact `i`; `reserved` holds the keys
/// already occupied. The returned vector is **positionally aligned** with
/// `candidates` — callers that need to look a key up by path must do so through
/// the caller-supplied ordering (see
/// [`bookkeeping_for`](crate::bookkeeping_for)), which aligns positions with
/// its own candidate list.
///
/// A key wanted by exactly one path and absent from `reserved` is used as-is.
/// A contested key is resolved by ranking the competing paths and handing out
/// `-<n>` … `-1` (see the module docs).
#[must_use]
pub fn build_index(candidates: &[String], reserved: &Reserved) -> Vec<String> {
    let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (position, candidate) in candidates.iter().enumerate() {
        groups.entry(candidate.as_str()).or_default().push(position);
    }
    let mut resolved = candidates.to_vec();
    for (candidate, positions) in groups {
        let contested = positions.len() > 1 || reserved.contains(candidate);
        if !contested {
            continue;
        }
        // Rank by the *path* that produced the candidate. Two positions can
        // only be in the same group when they share a base key, not a path, so
        // the position is a stable tie-breaker and never affects a real
        // collision. Within one scan the candidates arrive in one traversal
        // order, so ranking on the position is ranking on the path.
        let mut ranked: Vec<usize> = positions;
        ranked.sort_unstable();
        // A candidate whose base key is merely *reserved* by an untouched row
        // still needs a suffix, but a candidate that is itself already a row's
        // key (a re-run over an unchanged index) must keep that key: renumbering
        // it would move a stable key and waste the freed suffixes. `count` is
        // therefore the number of candidates that still need one.
        let count = ranked.len();
        let suffixes = collision_candidates(candidate, count, reserved);
        for (rank, position) in ranked.into_iter().enumerate() {
            resolved[position].clone_from(&suffixes[rank]);
        }
    }
    resolved
}

/// The same rule as [`build_index`], keyed by path, for callers (and tests)
/// that hold paths in an arbitrary order.
#[must_use]
pub fn keys_for_paths(paths: &[String]) -> BTreeMap<String, String> {
    let mut ordered: Vec<&String> = paths.iter().collect();
    ordered.sort();
    let unique: Vec<String> = ordered
        .into_iter()
        .cloned()
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect();
    let candidates: Vec<String> = unique.iter().map(|path| base_key(path)).collect();
    let resolved = build_index(&candidates, &Reserved::new());
    unique.into_iter().zip(resolved).collect()
}

/// The artifact kind implied by a file extension, or `None` when the file is
/// not a model artifact. `.gguf` / `.ninfer` are matched case-insensitively.
#[must_use]
pub fn artifact_kind(path: &Path) -> Option<ArtifactKind> {
    let extension = path.extension()?.to_str()?;
    if extension.eq_ignore_ascii_case(GGUF_EXTENSION) {
        Some(ArtifactKind::Gguf)
    } else if extension.eq_ignore_ascii_case(NINFER_EXTENSION) {
        Some(ArtifactKind::Ninfer)
    } else {
        None
    }
}

/// GGUF metadata value tags (`gguf_type`), as used by the `general.name`
/// lookup in [`crate::walk`].
pub(crate) const GGUF_TYPE_U8: u32 = 0;
pub(crate) const GGUF_TYPE_I8: u32 = 1;
pub(crate) const GGUF_TYPE_U16: u32 = 2;
pub(crate) const GGUF_TYPE_I16: u32 = 3;
pub(crate) const GGUF_TYPE_U32: u32 = 4;
pub(crate) const GGUF_TYPE_I32: u32 = 5;
pub(crate) const GGUF_TYPE_F32: u32 = 6;
pub(crate) const GGUF_TYPE_BOOL: u32 = 7;
pub(crate) const GGUF_TYPE_STRING: u32 = 8;
pub(crate) const GGUF_TYPE_U64: u32 = 10;
pub(crate) const GGUF_TYPE_I64: u32 = 11;
pub(crate) const GGUF_TYPE_F64: u32 = 12;

/// RFC 4122 version-5 UUID from a SHA-256 digest of `bytes`.
fn uuid_from_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let digest = sha256(bytes);
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    // version 5 (name-based, SHA-256) + RFC 4122 variant bits.
    out[6] = (out[6] & 0x0f) | 0x50;
    out[8] = (out[8] & 0x3f) | 0x80;
    let mut hex = String::with_capacity(32);
    for byte in out {
        let _ = write!(hex, "{byte:02x}");
    }
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// SHA-256 (FIPS 180-4).
///
/// A local implementation keeps the workspace free of a new dependency
/// (`docs/architecture.md` section 2: no hash crate is pinned). It seeds the
/// stable UUID only - never a security decision, never secret material.
pub(crate) fn sha256(message: &[u8]) -> [u8; 32] {
    let mut state = SHA256_INIT;
    let mut padded = message.to_vec();
    let bit_len = (message.len() as u64).wrapping_mul(8);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for block in padded.as_chunks::<64>().0 {
        let mut schedule = [0u32; 64];
        for (index, word) in schedule.iter_mut().take(16).enumerate() {
            let start = index * 4;
            *word = u32::from_be_bytes([
                block[start],
                block[start + 1],
                block[start + 2],
                block[start + 3],
            ]);
        }
        expand_schedule(&mut schedule);
        let mut working = state;
        for (index, word) in schedule.iter().enumerate() {
            compress_round(&mut working, index, *word);
        }
        for (slot, value) in state.iter_mut().zip(working) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut digest = [0u8; 32];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

/// Extend the first 16 words of a message block into the full 64-word SHA-256
/// schedule (FIPS 180-4 section 6.2.2 step 1).
fn expand_schedule(schedule: &mut [u32; 64]) {
    for index in 16..64 {
        let prev15 = schedule[index - 15];
        let sigma0 = prev15.rotate_right(7) ^ prev15.rotate_right(18) ^ (prev15 >> 3);
        let prev2 = schedule[index - 2];
        let sigma1 = prev2.rotate_right(17) ^ prev2.rotate_right(19) ^ (prev2 >> 10);
        schedule[index] = schedule[index - 16]
            .wrapping_add(sigma0)
            .wrapping_add(schedule[index - 7])
            .wrapping_add(sigma1);
    }
}

/// The first 32 bits of the fractional parts of the cube roots of the first 64
/// primes (FIPS 180-4 section 4.2.2).
const SHA256_ROUND_CONSTANTS: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// The first 32 bits of the fractional parts of the square roots of the first
/// eight primes: the SHA-256 initial hash value (FIPS 180-4 section 5.3.3).
const SHA256_INIT: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// One SHA-256 compression round over the working state `state` (FIPS 180-4
/// section 6.2.2 steps 3 and 4) for message-schedule `word` at `index`. The
/// eight working variables are named `va`…`vh` so the standard's formulas
/// stay readable without single-character bindings.
fn compress_round(state: &mut [u32; 8], index: usize, word: u32) {
    let (va, vb, vc, vd, ve, vf, vg, vh) = (
        state[0], state[1], state[2], state[3], state[4], state[5], state[6], state[7],
    );
    let big_s1 = ve.rotate_right(6) ^ ve.rotate_right(11) ^ ve.rotate_right(25);
    let choose = (ve & vf) ^ ((!ve) & vg);
    let temp1 = vh
        .wrapping_add(big_s1)
        .wrapping_add(choose)
        .wrapping_add(SHA256_ROUND_CONSTANTS[index])
        .wrapping_add(word);
    let big_s0 = va.rotate_right(2) ^ va.rotate_right(13) ^ va.rotate_right(22);
    let majority = (va & vb) ^ (va & vc) ^ (vb & vc);
    let temp2 = big_s0.wrapping_add(majority);
    state[7] = vg;
    state[6] = vf;
    state[5] = ve;
    state[4] = vd.wrapping_add(temp1);
    state[3] = vc;
    state[2] = vb;
    state[1] = va;
    state[0] = temp1.wrapping_add(temp2);
}
