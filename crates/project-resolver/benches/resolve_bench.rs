use criterion::{criterion_group, criterion_main, Criterion};
use std::path::PathBuf;

fn bench_resolve_simple_app(c: &mut Criterion) {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/simple-app/tsconfig.app.json");

    c.bench_function("resolve_simple_app", |b| {
        b.iter(|| ngc_project_resolver::resolve_project(&fixture).expect("fixture should resolve"))
    });
}

criterion_group!(benches, bench_resolve_simple_app);
criterion_main!(benches);
