//! Independent legal-binary producer for the uwvm2-ros extension fuzz campaign.
pub const OPS: &[(u8, u8, u8)] = &[
    (0x20, 0, 1),
    (0x21, 1, 1),
    (0x22, 2, 1),
    (0x23, 0, 1),
    (0x24, 0, 1),
    (0x25, 2, 1),
    (0x26, 2, 1),
    (0x27, 1, 1),
    (0x28, 1, 1),
    (0x29, 2, 1),
    (0x2a, 2, 1),
    (0x2b, 2, 0),
    (0x2c, 2, 0),
    (0x2d, 1, 1),
    (0x2e, 2, 1),
    (0x2f, 2, 1),
    (0x36, 3, 1),
    (0x37, 4, 1),
    (0x38, 4, 1),
];
pub fn leb(mut n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = n as u8 & 127;
        n >>= 7;
        out.push(byte | if n == 0 { 0 } else { 128 });
        if n == 0 {
            return out;
        }
    }
}
fn unsigned(n: u64, pad: bool) -> Vec<u8> {
    let mut out = leb(n);
    if pad {
        *out.last_mut().unwrap() |= 128;
        out.push(0);
    }
    out
}
fn signed(mut n: i64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = n as u8 & 127;
        n >>= 7;
        let done = (n == 0 && byte & 64 == 0) || (n == -1 && byte & 64 != 0);
        out.push(byte | if done { 0 } else { 128 });
        if done {
            return out;
        }
    }
}
fn frame(module: &mut Vec<u8>, id: u8, data: &[u8]) {
    module.push(id);
    module.extend(leb(data.len() as u64));
    module.extend(data);
}
fn string(s: &str) -> Vec<u8> {
    [leb(s.len() as u64), s.as_bytes().to_vec()].concat()
}
fn custom(module: &mut Vec<u8>, name: &str, data: &[u8]) {
    frame(module, 0, &[string(name), data.to_vec()].concat());
}
pub fn random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}
pub fn module(seed: u64, edit: Option<u8>) -> Vec<u8> {
    let mut rng = seed;
    let wide = seed & 1 != 0;
    let shared = seed & 2 != 0;
    let imported = seed & 4 != 0;
    let count = 2 + (seed as usize % 4);
    let mut out = b"\0asm\x01\0\0\0".to_vec();
    frame(&mut out, 1, &[1, 0x60, 0, 0]);
    let flags = if wide { 4 } else { 0 } | if shared { 3 } else { 0 };
    let mut mem = vec![flags, 1];
    if shared {
        mem.push(2);
    }
    if imported {
        frame(
            &mut out,
            2,
            &[
                vec![1],
                string("env"),
                string("memory"),
                vec![2],
                mem.clone(),
            ]
            .concat(),
        );
    }
    frame(&mut out, 3, &[leb(count as u64), vec![0; count]].concat());
    frame(
        &mut out,
        5,
        &[
            vec![if imported { 1 } else { 2 }],
            if imported { mem } else { mem.repeat(2) },
        ]
        .concat(),
    );
    let mut exports = leb(count as u64);
    for f in 0..count {
        exports.extend(string(&format!("f{f}")));
        exports.push(0);
        exports.extend(leb(f as u64));
    }
    frame(&mut out, 7, &exports);
    let mut code = leb(count as u64);
    let mut names = leb(count as u64);
    for f in 0..count {
        names.extend(leb(f as u64));
        names.extend(string(&format!("符号::{f}")));
        let mut body = vec![0, 0x02, 0x40];
        let constant = if wide { 0x42 } else { 0x41 };
        let value = random(&mut rng) as i64;
        body.push(constant);
        let base = if wide { value } else { value as i32 as i64 };
        body.extend(signed(
            base ^ if f == 0 && edit == Some(0) {
                0x10000
            } else {
                0
            },
        ));
        body.push(0x1a);
        // A fixed anchor supplies separately testable operation/memory edits.
        body.extend_from_slice(&[constant, 0, constant, 16, 0xff]);
        body.extend(leb(if f == 0 && edit == Some(1) {
            0x0101_002c
        } else {
            0x0101_002b
        }));
        body.push(u8::from(f == 0 && edit == Some(2)));
        if !(f == 0 && edit == Some(4)) {
            body.extend_from_slice(&[0x41, 17, 0x1a]);
        }
        if f == 0 && edit == Some(3) {
            body.extend_from_slice(&[0x41, 18, 0x1a]);
        }
        // Every generated module includes all 19 instructions, in random order.
        let shift = random(&mut rng) as usize % OPS.len();
        for offset in 0..OPS.len() {
            let &(op, inputs, outputs) = &OPS[(offset + shift) % OPS.len()];
            for _ in 0..inputs {
                let value = random(&mut rng) as i64;
                body.push(constant);
                body.extend(signed(if wide { value } else { value as i32 as i64 }));
            }
            body.push(0xff);
            body.extend(unsigned(
                0x0101_0000 + u64::from(op),
                f == 0 && edit == Some(7),
            ));
            body.extend(unsigned(random(&mut rng) & 1, seed & 8 != 0));
            body.extend(std::iter::repeat_n(0x1a, outputs as usize));
        }
        // An if/else and a standard SIMD constant exercise mixed grammars.
        body.extend_from_slice(&[0x41, 1, 0x04, 0x40, 0x01, 0x05, 0x01, 0x0b, 0xfd, 0x0c]);
        body.extend_from_slice(&[0; 16]);
        body.extend_from_slice(&[0x1a, 0x0b, 0x0b]);
        code.extend(leb(body.len() as u64));
        code.extend(body);
    }
    frame(&mut out, 10, &code);
    custom(
        &mut out,
        "name",
        &[vec![1], leb(names.len() as u64), names].concat(),
    );
    let address = (random(&mut rng) & 0x7fff) + u64::from(edit == Some(5));
    custom(
        &mut out,
        "uwvm.memtag.linear.globals",
        &[vec![2, 1, 1], leb(address), leb(16)].concat(),
    );
    let mut opaque: Vec<_> = (0..64).map(|_| random(&mut rng) as u8).collect();
    if edit == Some(6) {
        opaque[31] ^= 0x80;
    }
    custom(&mut out, "vendor.audit", &opaque);
    out
}
