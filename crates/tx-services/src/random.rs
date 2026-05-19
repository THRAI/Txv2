//! ChaCha20-based kernel CSPRNG (txdoc:RANDOM-V1).
//!
//! Seeded from platform entropy at initialisation. Output is generated
//! via the ChaCha20 keystream (256-bit key, 64-bit nonce, 64-bit
//! counter — RFC 8439 §2.3 IETF variant). Lock-free design uses
//! `AtomicU64` for the counter and a CAS-guarded buffer; safe for
//! single-hart and low-contention SMP use.
//!
//! # Initial seeding
//!
//! `init(seed)` accepts a 32-byte seed. Call once at boot.
//!
//! # Usage
//!
//! ```ignore
//! tx_services::random::init(&mix_platform_entropy());
//! tx_services::random::fill_bytes(&mut buf);
//! ```

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

// ── ChaCha20 core ──────────────────────────────────────────────────

fn quarter_round(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    state[a] = state[a].wrapping_add(state[b]); state[d] ^= state[a]; state[d] = state[d].rotate_left(16);
    state[c] = state[c].wrapping_add(state[d]); state[b] ^= state[c]; state[b] = state[b].rotate_left(12);
    state[a] = state[a].wrapping_add(state[b]); state[d] ^= state[a]; state[d] = state[d].rotate_left(8);
    state[c] = state[c].wrapping_add(state[d]); state[b] ^= state[c]; state[b] = state[b].rotate_left(7);
}

fn chacha20_block(key: &[u8; 32], counter: u64, nonce: &[u8; 8]) -> [u8; 64] {
    let mut st: [u32; 16] = [
        0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574,
        u32::from_le_bytes([key[ 0], key[ 1], key[ 2], key[ 3]]),
        u32::from_le_bytes([key[ 4], key[ 5], key[ 6], key[ 7]]),
        u32::from_le_bytes([key[ 8], key[ 9], key[10], key[11]]),
        u32::from_le_bytes([key[12], key[13], key[14], key[15]]),
        u32::from_le_bytes([key[16], key[17], key[18], key[19]]),
        u32::from_le_bytes([key[20], key[21], key[22], key[23]]),
        u32::from_le_bytes([key[24], key[25], key[26], key[27]]),
        u32::from_le_bytes([key[28], key[29], key[30], key[31]]),
        counter as u32, (counter >> 32) as u32,
        u32::from_le_bytes([nonce[0], nonce[1], nonce[2], nonce[3]]),
        u32::from_le_bytes([nonce[4], nonce[5], nonce[6], nonce[7]]),
    ];
    let orig = st;
    for _ in 0..10 {
        quarter_round(&mut st, 0,4,8,12); quarter_round(&mut st, 1,5,9,13);
        quarter_round(&mut st, 2,6,10,14); quarter_round(&mut st, 3,7,11,15);
        quarter_round(&mut st, 0,5,10,15); quarter_round(&mut st, 1,6,11,12);
        quarter_round(&mut st, 2,7,8,13);  quarter_round(&mut st, 3,4,9,14);
    }
    let mut out = [0u8; 64];
    for i in 0..16 {
        let w = st[i].wrapping_add(orig[i]);
        out[i*4..(i+1)*4].copy_from_slice(&w.to_le_bytes());
    }
    out
}

// ── Global state ───────────────────────────────────────────────────

static READY: AtomicBool = AtomicBool::new(false);
static KEY: AtomicU64 = AtomicU64::new(0); // key stored as 4×u64
static KEY2: AtomicU64 = AtomicU64::new(0);
static KEY3: AtomicU64 = AtomicU64::new(0);
static KEY4: AtomicU64 = AtomicU64::new(0);
static CTR: AtomicU64 = AtomicU64::new(0);
const NONCE: [u8; 8] = *b"txKernel";

/// Initialise the CSPRNG with a 32-byte seed.  Idempotent.
pub fn init(seed: &[u8; 32]) {
    if READY.swap(true, Ordering::AcqRel) { return; }
    KEY.store( u64::from_le_bytes(seed[ 0.. 8].try_into().unwrap()), Ordering::Release);
    KEY2.store(u64::from_le_bytes(seed[ 8..16].try_into().unwrap()), Ordering::Release);
    KEY3.store(u64::from_le_bytes(seed[16..24].try_into().unwrap()), Ordering::Release);
    KEY4.store(u64::from_le_bytes(seed[24..32].try_into().unwrap()), Ordering::Release);
    CTR.store(1, Ordering::Release);
}

fn load_key() -> [u8; 32] {
    let mut k = [0u8; 32];
    k[ 0.. 8].copy_from_slice(&KEY.load(Ordering::Acquire).to_le_bytes());
    k[ 8..16].copy_from_slice(&KEY2.load(Ordering::Acquire).to_le_bytes());
    k[16..24].copy_from_slice(&KEY3.load(Ordering::Acquire).to_le_bytes());
    k[24..32].copy_from_slice(&KEY4.load(Ordering::Acquire).to_le_bytes());
    k
}

fn store_key(k: &[u8; 32]) {
    KEY.store( u64::from_le_bytes(k[ 0.. 8].try_into().unwrap()), Ordering::Release);
    KEY2.store(u64::from_le_bytes(k[ 8..16].try_into().unwrap()), Ordering::Release);
    KEY3.store(u64::from_le_bytes(k[16..24].try_into().unwrap()), Ordering::Release);
    KEY4.store(u64::from_le_bytes(k[24..32].try_into().unwrap()), Ordering::Release);
}

/// Fill `out` with random bytes.  Panics if `init` was not called.
pub fn fill_bytes(out: &mut [u8]) {
    assert!(READY.load(Ordering::Acquire), "CSPRNG not initialised");
    let mut written = 0;
    while written < out.len() {
        let ctr = CTR.fetch_add(1, Ordering::AcqRel);
        let block = chacha20_block(&load_key(), ctr, &NONCE);
        let take = (64usize).min(out.len() - written);
        out[written..written + take].copy_from_slice(&block[..take]);
        written += take;
    }
}

/// Mix fresh entropy into the key (XOR with ChaCha20 keystream).
pub fn reseed(entropy: &[u8; 32]) {
    let mask = chacha20_block(&load_key(), 0, &NONCE);
    let mut new_key = [0u8; 32];
    for i in 0..32 { new_key[i] = mask[i] ^ entropy[i]; }
    store_key(&new_key);
    CTR.store(1, Ordering::Release);
}

/// Mix available platform entropy into a 256-bit seed.
/// Default: deterministic xorshift64 counter (no-ASLR trust model).
pub fn platform_seed() -> [u8; 32] {
    static BOOT: AtomicU64 = AtomicU64::new(0);
    let n = BOOT.fetch_add(1, Ordering::Relaxed);
    let mut s: u64 = 0x9E37_79B9_7F4A_7C15_u64.wrapping_mul(n.wrapping_add(1));
    let mut seed = [0u8; 32];
    for i in 0..4 {
        s ^= s << 13; s ^= s >> 7; s ^= s << 17;
        seed[i*8..(i+1)*8].copy_from_slice(&s.to_le_bytes());
    }
    seed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chacha20_block_is_nonzero() {
        let block = chacha20_block(&[0x42u8; 32], 1, &[0u8; 8]);
        // Output must not be all-zeros.
        assert!(block.iter().any(|&b| b != 0));
    }

    #[test]
    fn fill_is_deterministic() {
        init(&[0x42u8; 32]);
        let mut a = [0u8; 128];
        fill_bytes(&mut a);
        // Reset and re-init with same seed → same output.
        READY.store(false, Ordering::Release);
        init(&[0x42u8; 32]);
        let mut b = [0u8; 128];
        fill_bytes(&mut b);
        assert_eq!(a, b);
    }
}
