//! Reproducible legal-Wasm generation plus hostile encoding mutations.
//! Never executes Guest instructions. All products belong in the caller's .tmp.
#[path = "support/memtag_gen.rs"]
mod generator;
#[cfg(target_os = "linux")]
use std::{
    fs,
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};
use wasm_git::{
    audit, parse,
    patch::{self, BytePatch},
    standard::Standard,
};
use wasmparser::{Validator, WasmFeatures};

fn replay(old: &[u8], new: &[u8], report: &audit::AuditDiff) {
    let mut rebuilt = Vec::new();
    let (mut a, mut b) = (0, 0);
    for edit in &report.edits {
        assert_eq!(&old[a..edit.old_span.start], &new[b..edit.new_span.start]);
        let hex = |s: &[u8]| s.iter().map(|v| format!("{v:02x}")).collect::<String>();
        assert_eq!(
            edit.old_hex,
            hex(&old[edit.old_span.start..edit.old_span.end])
        );
        assert_eq!(
            edit.new_hex,
            hex(&new[edit.new_span.start..edit.new_span.end])
        );
        rebuilt.extend_from_slice(&old[a..edit.old_span.start]);
        rebuilt.extend_from_slice(&new[edit.new_span.start..edit.new_span.end]);
        a = edit.old_span.end;
        b = edit.new_span.end;
    }
    assert_eq!(&old[a..], &new[b..]);
    rebuilt.extend_from_slice(&old[a..]);
    assert_eq!(rebuilt, new);
    for (a, b) in [(old, new), (new, old)] {
        let bytes = patch::create(a, b).unwrap().to_bytes().unwrap();
        assert_eq!(BytePatch::from_bytes(&bytes).unwrap().apply(a).unwrap(), b);
    }
}
fn check(seed: u64, mode: u8, profile: Standard) {
    let old = generator::module(seed, None);
    let new = generator::module(seed, Some(mode));
    assert_ne!(old, new);
    for bytes in [&old, &new] {
        Validator::new_with_features(WasmFeatures::all())
            .validate_all(bytes)
            .unwrap();
    }
    let report = audit::compare(&old, &new).unwrap();
    assert_eq!(report.old.standard.validation, "valid");
    assert_eq!(report.new.standard.validation, "valid");
    replay(&old, &new, &report);
    let selected = audit::compare_with_standard(&old, &new, profile).unwrap();
    assert_eq!(
        serde_json::to_value(&report.edits).unwrap(),
        serde_json::to_value(&selected.edits).unwrap()
    );
    let a = parse::parse_module(&old).unwrap().resolve().normalize();
    let b = parse::parse_module(&new).unwrap().resolve().normalize();
    assert_eq!(a.functions.len(), b.functions.len());
    let changed = a
        .functions
        .iter()
        .zip(&b.functions)
        .filter(|(a, b)| {
            a.fingerprint.as_ref().unwrap().exact_body_hash
                != b.fingerprint.as_ref().unwrap().exact_body_hash
        })
        .count();
    assert_eq!(changed, usize::from(mode < 5));
    let mut damaged = new.clone();
    let position = 8 + seed as usize % (damaged.len() - 8);
    match seed % 4 {
        0 => damaged[position] ^= 1 << (seed % 8),
        1 => {
            damaged.remove(position);
        }
        2 => {
            damaged.insert(position, 0xff);
        }
        _ => damaged.truncate(position),
    }
    replay(&old, &damaged, &audit::compare(&old, &damaged).unwrap());
}

#[test]
fn legal_generator_and_all_edit_modes_have_independent_replay() {
    for seed in 1..=16 {
        for mode in 0..8 {
            check(seed, mode, Standard::Core3);
        }
    }
}

#[cfg(target_os = "linux")]
fn pin(cpu: usize) {
    unsafe extern "C" {
        fn sched_setaffinity(pid: i32, size: usize, mask: *const usize) -> i32;
    }
    let mut mask = [0usize; 16];
    mask[cpu / usize::BITS as usize] |= 1usize << (cpu % usize::BITS as usize);
    assert_eq!(
        unsafe { sched_setaffinity(0, std::mem::size_of_val(&mask), mask.as_ptr()) },
        0,
        "{}",
        std::io::Error::last_os_error()
    );
}

#[test]
#[ignore = "explicit 10-minute, 16 E-core campaign; run in a 64 GiB zero-swap cgroup"]
#[cfg(target_os = "linux")]
fn timed_16_ecore_campaign() {
    let seconds: u64 = std::env::var("WASM_GIT_MEMTAG_FUZZ_SECONDS")
        .unwrap_or("600".into())
        .parse()
        .unwrap();
    let output = std::path::PathBuf::from(
        std::env::var("WASM_GIT_MEMTAG_FUZZ_OUTPUT").expect("set a project .tmp output directory"),
    );
    fs::create_dir_all(&output).unwrap();
    let gate = Arc::new(Barrier::new(16));
    let started = Instant::now();
    let handles: Vec<_> = (0..16).map(|worker| {
        let gate = gate.clone();
        let output = output.clone();
        thread::spawn(move || {
            pin(16 + worker);
            let mut rng = 0x4646_4d54_4500_0001 ^ ((worker as u64 + 1) * 0x10001);
            gate.wait();
            let start = Instant::now();
            let mut count = 0u64;
            while start.elapsed() < Duration::from_secs(seconds) {
                let seed = generator::random(&mut rng);
                let profile = [Standard::Core1, Standard::Core2, Standard::Core3, Standard::AllKnown][count as usize % 4];
                let mode = (count % 8) as u8;
                if let Err(panic) = std::panic::catch_unwind(|| check(seed, mode, profile)) {
                    fs::write(output.join(format!("failure-{worker}-old.wasm")), generator::module(seed, None)).unwrap();
                    fs::write(output.join(format!("failure-{worker}-new.wasm")), generator::module(seed, Some(mode))).unwrap();
                    fs::write(output.join(format!("failure-{worker}.txt")), format!("seed={seed} mode={mode} iteration={count}")).unwrap();
                    std::panic::resume_unwind(panic);
                }
                count += 1;
                if count.is_multiple_of(1000) { println!("worker={worker} cpu={} rounds={count} elapsed={:.1}", 16 + worker, start.elapsed().as_secs_f64()); }
            }
            serde_json::json!({"worker":worker,"cpu":16+worker,"rounds":count,"elapsed_seconds":start.elapsed().as_secs_f64()})
        })
    }).collect();
    let workers: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let rounds: u64 = workers.iter().map(|w| w["rounds"].as_u64().unwrap()).sum();
    let summary = serde_json::json!({"status":"PASS","seconds_requested":seconds,"elapsed_seconds":started.elapsed().as_secs_f64(),"threads":16,"cpus":"16-31","rounds":rounds,"legal_modules_validated":rounds*2,"hostile_mutations":rounds,"serialized_patch_replays":rounds*4,"instruction_forms":19,"edit_modes":8,"workers":workers});
    fs::write(
        output.join("summary.json"),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .unwrap();
    println!("{summary}");
}
