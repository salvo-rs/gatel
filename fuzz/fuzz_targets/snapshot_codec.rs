#![no_main]
use gatel_model::{
    Hasher,
    snapshot_codec::{MAX_BYTES, decode, encode},
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(snapshot) = decode(data) {
        assert_eq!(encode(&snapshot).unwrap(), data);
    }
    // Also generate a correct envelope so random mutations reach structural
    // decoding instead of spending their entire run at checksum rejection.
    if data.len() > MAX_BYTES - 40 {
        return;
    }
    let mut hash = Hasher::new();
    hash.update_str("gatel.snapshot.codec.v1").update_bytes(data);
    let mut framed = b"GATELS01".to_vec();
    framed.extend_from_slice(hash.finish().as_bytes());
    framed.extend_from_slice(data);
    if let Ok(snapshot) = decode(&framed) {
        assert_eq!(encode(&snapshot).unwrap(), framed);
    }
});
