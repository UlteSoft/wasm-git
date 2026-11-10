//! Fixed 0.1.4 wire contract: future decoder upgrades must keep this fixture.
//! This is a source regression fixture, not generated qualification output.
use wasm_git::patch::BytePatch;

#[test]
fn release_0_1_4_patch_survives_future_syntax_and_keeps_integrity_checks() {
    // Do not regenerate this baseline when the parser or IR changes.
    let hex = "5747545041543031960e84bfb7b547e04392b4bf285d30c16eb14bad255a8363ac15f95bea40bb1bc0297927b880a6901cc47f41c52f5f6d01644a975d5ee5299fc8bcda4fe6e060000000000000001b000000000000001b0000000100000000000000040000000000000005000000000000000400000000000000050102";
    let bytes: Vec<u8> = hex
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    let old = b"\0asm\x01\0\0\0\0\x11\x0bfuture.tail\0\xff\x41\0\x0b";
    let mut new = old.to_vec();
    new[4] = 2;
    let patch = BytePatch::from_bytes(&bytes).unwrap();
    assert_eq!(patch.to_bytes().unwrap(), bytes);
    assert_eq!(patch.apply(old).unwrap(), new);
    let mut wrong = old.to_vec();
    wrong.push(0);
    assert!(patch.apply(&wrong).is_err());
    for end in 0..bytes.len() {
        assert!(BytePatch::from_bytes(&bytes[..end]).is_err(), "{end}");
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(BytePatch::from_bytes(&trailing).is_err());
    let mut bad_count = bytes.clone();
    bad_count[88..92].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(BytePatch::from_bytes(&bad_count).is_err());
}
