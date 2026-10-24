//! Expanded changed-path tree: no filesystem access and no unchanged entries.

use qm_types::api::{DiffFile, DiffFileStatus};
use std::collections::BTreeMap;

pub(super) struct Entry {
    pub(super) label: String,
    pub(super) depth: usize,
    pub(super) status: DiffFileStatus,
    pub(super) file: Option<usize>,
}

#[derive(Default)]
struct Node {
    children: BTreeMap<String, Self>,
    file: Option<(usize, DiffFileStatus)>,
}

pub(super) fn entries(files: &[DiffFile]) -> Vec<Entry> {
    let mut root = Node::default();
    for (index, file) in files.iter().enumerate() {
        let mut node = &mut root;
        for part in file.path().unwrap_or("(unknown path)").split('/') {
            node = node.children.entry(part.to_owned()).or_default();
        }
        node.file = Some((index, file.status));
    }
    let mut entries = Vec::new();
    // Iterative traversal also handles paths with thousands of components without recursion.
    let mut stack: Vec<_> = root
        .children
        .iter()
        .rev()
        .map(|(name, node)| (name, node, 0))
        .collect();
    while let Some((name, mut node, depth)) = stack.pop() {
        let mut label = name.clone();
        // Compress folder-only chains, but keep each file as its own clickable leaf.
        while node.file.is_none() && node.children.len() == 1 {
            let (name, child) = node.children.first_key_value().expect("one child");
            if child.file.is_some() {
                break;
            }
            label.push('/');
            label.push_str(name);
            node = child;
        }
        let status = node
            .file
            .map_or_else(|| folder_status(node), |(_, status)| status);
        if !node.children.is_empty() {
            label.push('/');
        }
        entries.push(Entry {
            label,
            depth,
            status,
            file: node.file.map(|(index, _)| index),
        });
        stack.extend(
            node.children
                .iter()
                .rev()
                .map(|(name, node)| (name, node, depth + 1)),
        );
    }
    entries
}

fn folder_status(node: &Node) -> DiffFileStatus {
    let mut status = None;
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        if let Some((_, next)) = node.file {
            if status.is_some_and(|s| s != next) {
                return DiffFileStatus::Modified;
            }
            status = Some(next);
        }
        stack.extend(node.children.values());
    }
    status.unwrap_or_default()
}

pub(super) fn status_label(status: DiffFileStatus) -> (&'static str, &'static str) {
    match status {
        DiffFileStatus::Added => ("A", "Added"),
        DiffFileStatus::Deleted => ("D", "Deleted"),
        DiffFileStatus::Renamed => ("R", "Renamed"),
        DiffFileStatus::Copied => ("C", "Copied"),
        _ => ("M", "Modified"),
    }
}
