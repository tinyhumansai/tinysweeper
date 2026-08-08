//! temp
use crate::graph::{build, types::SourceFile};

#[test]
fn measure() {
    let mut files = Vec::new();
    for entry in walkdir(".") {
        if let Ok(text) = std::fs::read_to_string(&entry) {
            files.push(SourceFile::new(entry.trim_start_matches("./"), text));
        }
    }
    let g = build("tinyhumansai/tinysweeper", &files).unwrap();
    println!("files={} nodes={} edges={}", files.len(), g.nodes.len(), g.edges.len());
    println!("{:?}", g.coverage);
    println!("resolution rate = {:.4}", g.coverage.import_resolution_rate());
    let mut by_reason = std::collections::BTreeMap::new();
    for u in &g.unresolved { *by_reason.entry(format!("{:?}", u.reason)).or_insert(0) += 1; }
    println!("{by_reason:?}");
    for u in g.unresolved.iter().filter(|u| format!("{:?}", u.reason) != "External") {
        println!("  {} :: {} :: {:?}", u.path, u.specifier, u.reason);
    }
}

fn walkdir(root: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_string()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path().to_string_lossy().to_string();
            if p.contains("/.git") || p.contains("/target") || p.contains("/worktrees") { continue; }
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) { stack.push(p); } else { out.push(p); }
        }
    }
    out
}
