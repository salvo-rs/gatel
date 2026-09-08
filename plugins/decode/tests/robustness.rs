#[path = "support/fuzz_cases.rs"]
mod fuzz_cases;

#[tokio::test]
async fn deterministic_mutations_exercise_the_same_invariants_as_libfuzzer() {
    for selector in 0..16 {
        for payload in [b"".as_slice(), b"hello", b"\x00\xff\x01\x7f", &[0; 256]] {
            let mut data = vec![selector];
            data.extend_from_slice(payload);
            fuzz_cases::exercise(&data).await;
        }
    }
    let mut state = 0x12345678_u32;
    for index in 0..256 {
        let mut data = vec![index as u8];
        for _ in 0..index % 128 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            data.push(state as u8);
        }
        fuzz_cases::exercise(&data).await;
    }
}
