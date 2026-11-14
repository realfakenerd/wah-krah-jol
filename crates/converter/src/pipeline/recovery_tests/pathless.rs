use super::*;

#[test]
fn pathless_nonzero_depth_traversal_error_is_required() {
    assert!(traversal_error_is_required(1, false));
    assert!(!traversal_error_is_required(1, true));
    assert!(traversal_error_is_required(0, true));
}

#[test]
fn refusing_data_root_mask_preserves_archived_neighbors() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("Data");
    let vfs = directory.path().join("vfs");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(vfs.join("meshes")).unwrap();
    let archived_neighbor = vfs.join("meshes/archived.nif");
    fs::write(&archived_neighbor, b"archive asset").unwrap();

    assert!(mask_undiscovered_inputs(&data, &vfs, std::slice::from_ref(&data)).is_err());
    assert_eq!(fs::read(archived_neighbor).unwrap(), b"archive asset");
}
