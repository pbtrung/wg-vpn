//! "Fuzz-lite" stress testing (docs/milestones.md §5 "Fuzz the config
//! parser"). A real `cargo-fuzz`/libFuzzer harness needs a nightly
//! toolchain that isn't available in this environment (no `rustup`, and
//! this is a distro-packaged stable Rust) — this substitutes a
//! PRNG-driven mutation stress test against the same public parsing
//! entry points, runnable on stable. Swap in real `cargo-fuzz` targets
//! (`fuzz_targets/parse_wg_quick.rs`, etc.) once a nightly toolchain is
//! available; the seed corpus and mutation strategy here would carry
//! over directly.
//!
//! The property under test is narrow but load-bearing: these functions
//! must never panic on untrusted input (a compromised or corrupted
//! storage object is untrusted by design — wg-client.md §10). A parse
//! failure is fine and expected; a panic is not.

use wg_common::{discovery, render, topology};

/// splitmix64 — deterministic, dependency-free, good enough avalanche
/// for mutation selection (not cryptographic).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() as usize) % n.max(1)
    }
}

fn mutate(rng: &mut Rng, seed: &str, mutations: usize) -> String {
    let mut bytes = seed.as_bytes().to_vec();
    for _ in 0..mutations {
        if bytes.is_empty() {
            bytes.push(b'{');
            continue;
        }
        let idx = rng.below(bytes.len());
        match rng.below(4) {
            0 => bytes[idx] = (rng.next() % 256) as u8,
            1 => bytes.insert(idx, (rng.next() % 256) as u8),
            2 => {
                bytes.remove(idx);
            }
            _ => {
                // Duplicate a chunk, the kind of mutation that tends to
                // trip up hand-rolled scanners (unbalanced braces/sections).
                let end = (idx + 1 + rng.below(8)).min(bytes.len());
                let chunk: Vec<u8> = bytes[idx..end].to_vec();
                let insert_at = rng.below(bytes.len() + 1);
                for (i, b) in chunk.into_iter().enumerate() {
                    bytes.insert((insert_at + i).min(bytes.len()), b);
                }
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn wg_quick_seeds() -> Vec<String> {
    let kp = wg_common::keys::Keypair::generate();
    let peer = wg_common::keys::Keypair::generate();
    let psk = wg_common::keys::generate_preshared_key();
    vec![
        format!(
            "[Interface]\nPrivateKey = {}\nAddress = 10.0.0.1/32\nListenPort = 51820\n",
            kp.private_key_base64()
        ),
        format!(
            "[Interface]\nPrivateKey = {}\nAddress = 10.0.0.1/32\nListenPort = 51820\nDNS = 1.1.1.1\nMTU = 1420\n\n[Peer]\nPublicKey = {}\nPresharedKey = {psk}\nAllowedIPs = 10.0.0.2/32, 10.20.0.0/16\nEndpoint = example.com:51820\nPersistentKeepalive = 25\n",
            kp.private_key_base64(),
            peer.public_key_base64(),
        ),
    ]
}

fn topology_seeds() -> Vec<String> {
    vec![
        r#"{"r2_config":{"endpoint":"https://x","read_write_access_key_id":"a","read_write_secret_access_key":"b","region":"auto","bucket":"c"},"nodes":[{"hostname":"a","wg_config":{"tunnel_address":"10.0.0.1/32","endpoint":"1.2.3.4:51820"}},{"hostname":"b","wg_config":{"tunnel_address":"10.0.0.2/32"}}],"master":["a"]}"#.to_string(),
    ]
}

fn discovery_seeds() -> Vec<String> {
    let id = wg_common::base32::random_id();
    vec![format!(
        r#"{{"schema_version":1,"revision":"{id}","current":{{"id":"{id}","nodes":{{"a":"{id}"}}}},"previous":null}}"#
    )]
}

const ITERATIONS: usize = 3000;

#[test]
fn parse_wg_quick_never_panics_on_mutated_input() {
    let seeds = wg_quick_seeds();
    let mut rng = Rng(1);
    for i in 0..ITERATIONS {
        let seed = &seeds[rng.below(seeds.len())];
        let mutations = 1 + rng.below(6);
        let text = mutate(&mut rng, seed, mutations);
        let result = std::panic::catch_unwind(|| render::parse_wg_quick(&text));
        assert!(result.is_ok(), "iteration {i} panicked on input: {text:?}");
    }
}

#[test]
fn topology_parse_never_panics_on_mutated_input() {
    let seeds = topology_seeds();
    let mut rng = Rng(2);
    for i in 0..ITERATIONS {
        let seed = &seeds[rng.below(seeds.len())];
        let mutations = 1 + rng.below(6);
        let text = mutate(&mut rng, seed, mutations);
        let result = std::panic::catch_unwind(|| topology::parse(&text));
        assert!(result.is_ok(), "iteration {i} panicked on input: {text:?}");
    }
}

#[test]
fn discovery_parse_never_panics_on_mutated_input() {
    let seeds = discovery_seeds();
    let mut rng = Rng(3);
    for i in 0..ITERATIONS {
        let seed = &seeds[rng.below(seeds.len())];
        let mutations = 1 + rng.below(6);
        let text = mutate(&mut rng, seed, mutations);
        let result = std::panic::catch_unwind(|| discovery::parse(&text));
        assert!(result.is_ok(), "iteration {i} panicked on input: {text:?}");
    }
}
