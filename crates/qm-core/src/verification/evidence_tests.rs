use super::*;
use qm_types::data_dir::{DataDir, migrate_directory};
use qm_types::ids::TaskId;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::PathBuf;

struct Fixture {
    root: PathBuf,
    paths: TaskPaths,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("qm-evidence-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        private_folder(&root).unwrap();
        let data = DataDir::with_scratch(root.join("home"), root.join("scratch"));
        let paths = data.task(TaskId::new(42).unwrap());
        private_folder(&paths.root.join("evidence")).unwrap();
        fs::write(paths.root.join("evidence/unique.png"), b"legacy").unwrap();
        Self { root, paths }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn conflict_includes_unique_legacy_evidence_and_scratch_files() {
    let fixture = Fixture::new("conflict");
    let paths = &fixture.paths;
    let legacy = paths.root.join("evidence");
    ensure_scratch_folder(&paths.evidence).unwrap();
    assert!(migrate_directory(&legacy, &paths.evidence).is_err());
    let files = manifest(paths);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, legacy.join("unique.png").to_string_lossy());
    assert!(
        brief_lines(paths, &files)
            .iter()
            .any(|line| line.contains("unique.png"))
    );
    fs::write(paths.evidence.join("new.png"), b"scratch").unwrap();
    let files = manifest(paths);
    assert_eq!(files.len(), 2);
    assert!(files.iter().any(|file| file.path == "new.png"));
}

#[test]
fn successful_migration_does_not_duplicate_compatibility_link() {
    let fixture = Fixture::new("success");
    let paths = &fixture.paths;
    migrate_directory(&paths.root.join("evidence"), &paths.evidence).unwrap();
    let files = manifest(paths);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "unique.png");
}

#[test]
fn an_unsafe_artifacts_folder_is_not_read_but_legacy_is_still_listed() {
    let fixture = Fixture::new("unsafe-artifacts");
    let paths = &fixture.paths;
    ensure_scratch_folder(&paths.evidence).unwrap();
    fs::write(paths.evidence.join("untrusted.png"), b"untrusted").unwrap();
    let scratch = paths.artifacts.clone();
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o755)).unwrap();
    let files = manifest(paths);
    assert_eq!(files.len(), 1);
    assert!(files[0].path.ends_with("unique.png"));
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_dir_all(&paths.evidence).unwrap();
    symlink(paths.root.join("evidence"), &paths.evidence).unwrap();
    assert_eq!(manifest(paths).len(), 1);
}

#[test]
fn legacy_links_and_linked_ancestors_are_not_traversed() {
    let fixture = Fixture::new("legacy-links");
    let paths = &fixture.paths;
    let legacy = paths.root.join("evidence");
    let elsewhere = fixture.root.join("elsewhere");
    fs::rename(&legacy, &elsewhere).unwrap();
    symlink(&elsewhere, &legacy).unwrap();
    assert_eq!(manifest(paths), Vec::<EvidenceFile>::new());
    fs::remove_file(&legacy).unwrap();
    fs::rename(&elsewhere, &legacy).unwrap();
    let moved = fixture.root.join("moved-task");
    fs::rename(&paths.root, &moved).unwrap();
    symlink(&moved, &paths.root).unwrap();
    assert_eq!(manifest(paths), Vec::<EvidenceFile>::new());
}
