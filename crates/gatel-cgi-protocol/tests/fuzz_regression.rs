#[path = "support/fuzz_cases.rs"]
mod cases;

#[test]
fn fuzz_invariants_run_without_a_fuzzer_toolchain() {
    for seed in [
        b"cContent-Type: text/plain\nContent-Length: 3\n\nabc".as_slice(),
        b"cStatus: 999 Bad\n\n",
        b"d\x01\x03\x00\x01\x00\x08\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00",
        b"ehello world",
    ] {
        cases::exercise(seed);
    }
    for size in [0, 1, 127, 128, 255, 4096, 65535, 65536, 131071] {
        let mut data = vec![b'x'; size + 1];
        data[0] = b'e';
        cases::exercise(&data);
    }
    let mut state = 17u64;
    for size in 0..1024 {
        let data: Vec<u8> = (0..size)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        cases::exercise(&data);
    }
}
