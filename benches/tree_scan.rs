use std::{collections::HashMap, fs};

use criterion::{Criterion, criterion_group, criterion_main};
use latte_lens::tree;

fn benchmark_tree_scan(criterion: &mut Criterion) {
    let directory = tempfile::tempdir().expect("create benchmark fixture");
    fs::write(directory.path().join(".gitignore"), "ignored/\n").unwrap();
    for directory_index in 0..20 {
        for file_index in 0..50 {
            let parent = directory.path().join(format!("src/{directory_index}"));
            fs::create_dir_all(&parent).unwrap();
            fs::write(
                parent.join(format!("file-{file_index}.rs")),
                "pub fn fixture() {}\n",
            )
            .unwrap();
        }
    }

    criterion.bench_function("scan 1k file ignored-aware tree", |bencher| {
        bencher.iter(|| tree::scan(directory.path(), &HashMap::new()).unwrap())
    });
}

criterion_group!(benches, benchmark_tree_scan);
criterion_main!(benches);
