//! PKCE (RFC 7636, S256) and the random values a login sends.
//!
//! Adapted from Pi `packages/ai/src/auth/oauth/pkce.ts` (MIT) and OpenAI Codex
//! `codex-rs/login/src/pkce.rs` (Apache-2.0, Copyright OpenAI): a 32-byte random verifier,
//! base64url without padding, and its SHA-256 as the challenge.
//!
//! None of these values is ever logged: the verifier is a secret until the code exchange is done,
//! and `state` and `nonce` would let someone forge a callback.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore as _;
use sha2::{Digest as _, Sha256};
use std::fmt;

/// A PKCE verifier and its S256 challenge.
#[derive(Clone)]
pub struct Pkce {
    /// Sent only in the token exchange.
    pub verifier: String,
    /// `BASE64URL(SHA256(verifier))`, sent in the authorize URL.
    pub challenge: String,
}

impl fmt::Debug for Pkce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pkce")
            .field("verifier", &"<redacted>")
            .field("challenge", &self.challenge)
            .finish()
    }
}

impl Pkce {
    /// A fresh verifier (32 random bytes) and its challenge.
    pub fn generate() -> Self {
        let verifier = random_base64url(32);
        let challenge = challenge_for(&verifier);
        Self {
            verifier,
            challenge,
        }
    }
}

/// The S256 challenge for a verifier.
pub fn challenge_for(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// `bytes` random bytes from the thread's CSPRNG, base64url without padding (`state`, `nonce`).
pub fn random_base64url(bytes: usize) -> String {
    URL_SAFE_NO_PAD.encode(random_bytes(bytes))
}

/// `bytes` random bytes as lowercase hex (the Codex login's `state`).
pub fn random_hex(bytes: usize) -> String {
    use fmt::Write as _;
    random_bytes(bytes)
        .iter()
        .fold(String::with_capacity(bytes * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0_u8; n];
    rand::rng().fill_bytes(&mut buf);
    buf
}
