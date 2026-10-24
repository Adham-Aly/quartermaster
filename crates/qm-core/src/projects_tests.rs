use super::find_folders;
use std::path::PathBuf;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qm-projects-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(dir).unwrap()
}

#[test]
fn a_plain_folder_is_found_after_any_repository_of_the_same_name() {
    let root = scratch("find");
    std::fs::create_dir_all(root.join("notes")).unwrap();
    std::fs::create_dir_all(root.join("code/Notes/.git")).unwrap();
    std::fs::create_dir_all(root.join("code/other")).unwrap();
    let found = find_folders(std::slice::from_ref(&root), "notes");
    // Exact case first, then a repository before a plain folder.
    assert_eq!(found, vec![root.join("notes"), root.join("code/Notes")]);
    std::fs::create_dir_all(root.join("deep/notes/.git")).unwrap();
    let found = find_folders(std::slice::from_ref(&root), "notes");
    assert_eq!(
        found,
        vec![
            root.join("deep/notes"),
            root.join("notes"),
            root.join("code/Notes")
        ]
    );
    assert_eq!(
        find_folders(std::slice::from_ref(&root), "missing"),
        Vec::<PathBuf>::new()
    );
    let _ = std::fs::remove_dir_all(root);
}
