//! Shared test fixtures: programmatic temp-dir trees used across module tests.
//! Compiled only under `cfg(test)`.

use std::{
    fs::{self, File},
    path::Path,
};

pub(crate) fn build_tree(root: &Path, files: &[&str]) {
    if let Err(err) = fs::create_dir_all(root) {
        panic!("error building test tree: {err}")
    };

    for file in files {
        if let Err(err) = File::create(root.join(file)) {
            panic!("error creating test file: {err}")
        };
    }
}

pub(crate) fn build_default_dir_structure(tmp_dir: &Path) {
    build_tree(
        tmp_dir.join("source/DCIM/100CANON").as_path(),
        &[
            "IMG_1800.CR2",
            "IMG_1800.JPG",
            "IMG_1868.CR2",
            "IMG_1868.JPG",
            "IMG_1875.CR2",
            "IMG_1875.JPG",
            "IMG_1881.CR2",
            "IMG_1881.JPG",
            "IMG_1891.CR2",
            "IMG_1891.JPG",
            "IMG_1907.CR2",
            "IMG_1907.JPG",
            "IMG_1939.CR2",
            "IMG_1915.JPG",
        ],
    );

    build_tree(
        tmp_dir.join("source/DCIM/101CANON").as_path(),
        &[
            "IMG_1800.CR2",
            "IMG_1800.JPG",
            "IMG_1939.CR2",
            "IMG_1915.JPG",
        ],
    );
}
