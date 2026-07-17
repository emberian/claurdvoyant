//! TOFU per-endpoint tokens: authenticate the claim "I am endpoint X" without any authority
//! machinery (no seats, no roles, no expiry — law 3 stays intact).
//!
//! The [`crate::task::model::TaskEvent::by`] field is a self-asserted string: anyone who can write
//! the log can stamp an event as any endpoint. This module closes that hole for endpoints that opt
//! in, and leaves solo/human use untouched:
//!
//! - **Bind on first use (TOFU).** An endpoint with no binding yet may present a token on an
//!   identity-bearing event; the first token seen becomes its binding (`endpoint -> sha256(token)`,
//!   stored in `$CLUSTERVISION_HOME/tasks/endpoints.json`). The raw token is never written to disk.
//! - **Enforce after binding.** Once an endpoint is bound, every identity-bearing event stamped as
//!   that endpoint must present the matching token or the append is rejected at the store seam.
//! - **Unbound stays trusted.** An endpoint that never presented a token keeps working with no
//!   token at all — the solo CLI / human path is unchanged (backward compatible). The hole is only
//!   closed for those who opt in; a fleet spawner mints a `CV_TOKEN` per worker to opt them in.
//!
//! Scope: this is **authentication** of the `by` claim, not authorization of what an endpoint may
//! do. Only identity-bearing events (claim/release/propose/pass/refute — see
//! [`crate::task::model::TaskEventKind::is_identity_bearing`]) are gated; bookkeeping verbs
//! (open/note/done/abandon) stay token-optional by design. Binding also only happens on
//! identity-bearing events, so a token on a bookkeeping verb is inert. Rotation is a rebind
//! (delete the endpoint's line from `endpoints.json`); a managed rotate/expiry flow is future work.
//!
//! I/O lives here (reading/writing the sidecar); the hashing is pure. This module is deliberately
//! kept out of the purity fence (`tests/dependency_fence.rs`) precisely because the sidecar read is
//! I/O — the reducer/model/stats stay pure and never see a token.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// The per-endpoint binding sidecar, next to `events.jsonl` under the same flock.
const BINDINGS_FILE: &str = "endpoints.json";

/// Durable map `endpoint -> sha256(token)`. Versioned like the event log so a newer shape refuses
/// rather than silently mis-reading; `version` defaults to 1 for the current format.
#[derive(Debug, Serialize, Deserialize)]
struct Bindings {
    #[serde(default = "one")]
    version: u64,
    /// `endpoint -> lowercase-hex sha256 of the bound token`. BTreeMap for deterministic on-disk
    /// key order (stable diffs, reproducible bytes).
    #[serde(default)]
    endpoints: BTreeMap<String, String>,
}

fn one() -> u64 {
    1
}

impl Default for Bindings {
    fn default() -> Bindings {
        Bindings {
            version: 1,
            endpoints: BTreeMap::new(),
        }
    }
}

impl Bindings {
    /// Load the sidecar for a task dir. A missing file is an empty binding set (no endpoint bound
    /// yet — the fresh-fleet / solo case). A parse failure is loud: the file exists but is garbage,
    /// and silently treating that as "unbound" would reopen the hole for every bound endpoint.
    fn load(dir: &Path) -> Result<Bindings> {
        let path = dir.join(BINDINGS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                let b: Bindings = serde_json::from_str(&raw)
                    .with_context(|| format!("parsing endpoint bindings {}", path.display()))?;
                if b.version != 1 {
                    bail!(
                        "endpoint bindings {} declare version {} (this cv understands 1) — upgrade cv before writing",
                        path.display(),
                        b.version
                    );
                }
                Ok(b)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Bindings::default()),
            Err(e) => Err(e).with_context(|| format!("reading endpoint bindings {}", path.display())),
        }
    }

    /// Persist the sidecar (pretty for human inspection; it is a small, human-facing trust record).
    fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(BINDINGS_FILE);
        let mut json = serde_json::to_string_pretty(self).context("serializing endpoint bindings")?;
        json.push('\n');
        std::fs::write(&path, json).with_context(|| format!("writing endpoint bindings {}", path.display()))?;
        Ok(())
    }
}

/// What the store should do with an about-to-be-written event, once its identity is checked.
#[derive(Debug, PartialEq)]
pub(crate) enum Decision {
    /// No token machinery applies (non-identity event, or an unbound endpoint with no token, or a
    /// bound endpoint whose presented token matched): write the event unchanged.
    Proceed,
    /// Trust-on-first-use: no binding existed and a token was presented — persist this binding,
    /// then write the event.
    Bind { endpoint: String, hash: String },
}

/// Decide whether an event stamped `by` may be written, given whatever token the caller presented.
///
/// `identity_bearing` is [`crate::task::model::TaskEventKind::is_identity_bearing`] for the event's
/// kind; a `false` here means the token is irrelevant and the event proceeds untouched.
///
/// Called under the store's `events.lock`, so the load-decide-(bind) sequence is atomic with the
/// append that follows: a `Bind` decision's [`commit`] runs before the event line is written, both
/// under the same lock.
pub(crate) fn authorize(dir: &Path, by: &str, identity_bearing: bool, token: Option<&str>) -> Result<Decision> {
    if !identity_bearing {
        return Ok(Decision::Proceed);
    }
    let bindings = Bindings::load(dir)?;
    match (bindings.endpoints.get(by), token) {
        // Bound + token presented: authenticate against the first-use binding.
        (Some(expected), Some(tok)) => {
            if &token_hash(tok) == expected {
                Ok(Decision::Proceed)
            } else {
                bail!(
                    "identity rejected: this append is stamped `by: {by}` but the presented token does not match \
                     the token {by} bound on first use — refusing a possible impersonation"
                )
            }
        }
        // Bound + no token: the endpoint opted in, so a tokenless append as it is impersonation.
        (Some(_), None) => bail!(
            "identity rejected: endpoint `{by}` is token-bound — present its token via CV_TOKEN or --token to \
             append identity-bearing events as `{by}`"
        ),
        // Unbound + token: trust on first use, bind it.
        (None, Some(tok)) => Ok(Decision::Bind {
            endpoint: by.to_string(),
            hash: token_hash(tok),
        }),
        // Unbound + no token: trusted by design (solo CLI / human / not-yet-opted-in fleet worker).
        (None, None) => Ok(Decision::Proceed),
    }
}

/// Persist a first-use binding. Idempotent for a re-seen endpoint/hash pair; called only for a
/// [`Decision::Bind`], under the store lock, immediately before the event is appended.
pub(crate) fn commit(dir: &Path, endpoint: &str, hash: &str) -> Result<()> {
    let mut bindings = Bindings::load(dir)?;
    bindings.endpoints.insert(endpoint.to_string(), hash.to_string());
    bindings.save(dir)
}

/// The bindings sidecar path for a task dir (test helper).
#[cfg(test)]
fn bindings_path(dir: &Path) -> std::path::PathBuf {
    dir.join(BINDINGS_FILE)
}

// ── SHA-256 (FIPS 180-4), self-contained and pure ────────────────────────────────────────────────
//
// A vendored SHA-256 rather than a `sha2` dependency: cv-core keeps a small, allowlisted dependency
// set (see `tests/dependency_fence.rs`) and cross-compiles to wasm, and a token *hash* (not a
// password KDF) needs only a standard collision-resistant digest. The known-answer tests below pin
// it against the FIPS vectors so it cannot silently drift.

const H0: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

const K: [u32; 64] = [
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

/// Lowercase-hex SHA-256 of `input`. Used to hash tokens before they touch disk (the raw token is
/// never stored). Pure: a function of its bytes only.
pub(crate) fn token_hash(input: &str) -> String {
    let mut h = H0;
    let msg = input.as_bytes();

    // Pre-processing: append 0x80, pad with zeros to 56 mod 64, then the 64-bit big-endian bit
    // length.
    let bit_len = (msg.len() as u64) * 8;
    let mut data = Vec::with_capacity(msg.len() + 9 + 63);
    data.extend_from_slice(msg);
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());

    let mut w = [0u32; 64];
    for chunk in data.chunks_exact(64) {
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([chunk[i * 4], chunk[i * 4 + 1], chunk[i * 4 + 2], chunk[i * 4 + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (hv, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *hv = hv.wrapping_add(v);
        }
    }

    let mut out = String::with_capacity(64);
    for v in h {
        use std::fmt::Write as _;
        let _ = write!(out, "{v:08x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS 180-4 known-answer vectors — pin the vendored digest against the standard so it can
    /// never silently drift into a wrong (but stable) hash.
    #[test]
    fn sha256_matches_fips_vectors() {
        assert_eq!(
            token_hash(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            token_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // The classic 448-bit (56-byte) message — exercises the two-block padding path.
        assert_eq!(
            token_hash("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn unbound_no_token_proceeds_and_binds_nothing() {
        let dir = tmp_dir();
        assert_eq!(authorize(&dir, "agent:solo", true, None).unwrap(), Decision::Proceed);
        assert!(!bindings_path(&dir).exists(), "no token → no sidecar written");
    }

    #[test]
    fn first_use_binds_then_matching_token_authenticates_and_wrong_token_rejects() {
        let dir = tmp_dir();
        // First use with a token → Bind.
        let Decision::Bind { endpoint, hash } = authorize(&dir, "agent:w", true, Some("s3cret")).unwrap() else {
            panic!("first use with a token should bind");
        };
        assert_eq!(endpoint, "agent:w");
        assert_eq!(hash, token_hash("s3cret"));
        commit(&dir, &endpoint, &hash).unwrap();

        // Bound + matching token → Proceed.
        assert_eq!(
            authorize(&dir, "agent:w", true, Some("s3cret")).unwrap(),
            Decision::Proceed
        );
        // Bound + wrong token → reject.
        let err = authorize(&dir, "agent:w", true, Some("guess")).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
        // Bound + no token → reject (opted-in endpoint must always present its token).
        let err = authorize(&dir, "agent:w", true, None).unwrap_err();
        assert!(err.to_string().contains("token-bound"), "{err}");
    }

    #[test]
    fn non_identity_events_ignore_tokens_entirely() {
        let dir = tmp_dir();
        // Bind agent:w.
        commit(&dir, "agent:w", &token_hash("s3cret")).unwrap();
        // A non-identity event stamped by the bound endpoint, no token: still proceeds (bookkeeping
        // verbs are token-optional by design), and a wrong token on a non-identity event is inert.
        assert_eq!(authorize(&dir, "agent:w", false, None).unwrap(), Decision::Proceed);
        assert_eq!(
            authorize(&dir, "agent:w", false, Some("wrong")).unwrap(),
            Decision::Proceed
        );
    }

    #[test]
    fn raw_token_never_touches_disk() {
        let dir = tmp_dir();
        commit(&dir, "agent:w", &token_hash("super-secret-token")).unwrap();
        let on_disk = std::fs::read_to_string(bindings_path(&dir)).unwrap();
        assert!(
            !on_disk.contains("super-secret-token"),
            "raw token must not be stored: {on_disk}"
        );
        assert!(
            on_disk.contains(&token_hash("super-secret-token")),
            "hash is what is stored"
        );
    }

    fn tmp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cv-identity-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
