use criterion::{criterion_group, criterion_main, Criterion};
use std::path::Path;
use std::process::Command;
use std::hint::black_box;

const EXE: &str = env!("CARGO_BIN_EXE_ukmm-extractool");

fn bench_extract_small_byml(c: &mut Criterion) {
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("mods")
        .join("nx")
        .join("Elemental_Bows_-_Switch_Port")
        .join("Actor")
        .join("ActorInfo.product.sbyml");

    if !file.exists() {
        eprintln!("WARN: fixture not found, skipping bench: {}", file.display());
        return;
    }

    let file_str = file.to_string_lossy().to_string();
    c.bench_function("extract_ActorInfo_sbyml", |b| {
        b.iter(|| {
            let output = Command::new(EXE)
                .arg(black_box(&file_str))
                .output()
                .expect("failed to execute");
            assert!(output.status.success());
        })
    });
}

fn bench_extract_medium_byml(c: &mut Criterion) {
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("mods")
        .join("wiiu")
        .join("Second_Wind_v1.9.14-alpha-7")
        .join("Event")
        .join("EventInfo.product.sbyml");

    if !file.exists() {
        eprintln!("WARN: fixture not found, skipping bench: {}", file.display());
        return;
    }

    let file_str = file.to_string_lossy().to_string();
    c.bench_function("extract_EventInfo_sbyml_20k", |b| {
        b.iter(|| {
            let output = Command::new(EXE)
                .arg(black_box(&file_str))
                .output()
                .expect("failed to execute");
            assert!(output.status.success());
        })
    });
}

criterion_group!(benches, bench_extract_small_byml, bench_extract_medium_byml);
criterion_main!(benches);
