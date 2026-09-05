//! End-to-end scalar versus NEON Helm payload decode probe.

use std::hint::black_box;
use std::io::Read;
use std::time::Instant;

use sofka::benchsupport as bs;

const ITERATIONS: usize = 20_000;

fn decode<E: base64::Engine>(engine: &E, wire: &str) -> bool {
    let Ok(inner) = engine.decode(wire) else {
        return false;
    };
    let Ok(gzip) = engine.decode(inner) else {
        return false;
    };
    let mut decoder = flate2::read::GzDecoder::new(gzip.as_slice());
    let mut json = Vec::new();
    if decoder.read_to_end(&mut json).is_err() {
        return false;
    }
    sofka::helm::parse_release_json(&json)
}

fn run<E: base64::Engine>(engine: &E, wire: &str) -> f64 {
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        assert!(black_box(decode(engine, black_box(wire))));
    }
    start.elapsed().as_secs_f64() * 1e6 / ITERATIONS as f64
}

fn main() {
    let secret = bs::helm_secret(1);
    let wire = secret
        .data
        .pointer("/data/release")
        .unwrap()
        .as_str()
        .unwrap();
    let scalar = base64::engine::general_purpose::STANDARD;
    let neon = base64::engine::Simd::standard(Default::default());
    // Alternate order across rounds to make drift visible.
    for round in 0..6 {
        if round % 2 == 0 {
            println!(
                "round {round}: scalar {:.3} us, neon {:.3} us",
                run(&scalar, wire),
                run(&neon, wire)
            );
        } else {
            let neon_time = run(&neon, wire);
            let scalar_time = run(&scalar, wire);
            println!("round {round}: scalar {scalar_time:.3} us, neon {neon_time:.3} us");
        }
    }
}
