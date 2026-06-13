//! Date-based destination routing for replication targets. Pure and I/O-free:
//! it takes the capture date as a parameter so the caller decides the source.

use std::path::{Path, PathBuf};

use chrono::{Datelike, NaiveDate};

#[derive(Clone, Debug)]
pub enum TargetKind {
    LocalNvme,
    LocalSpinning,
    Network,
}

#[derive(Clone, Debug)]
pub struct Target {
    pub root: PathBuf,
    pub kind: TargetKind,
    pub write_permits: usize,
}

pub fn build_destination_path(
    target: &Target,
    captured: NaiveDate,
    file: &Path,
) -> Option<PathBuf> {
    let year_dir = captured.year().to_string();
    let last_dir = captured.format("%Y-%m-%d").to_string();
    let file_name = file.file_name()?;
    Some(target.root.join(year_dir).join(last_dir).join(file_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_target_dir_test() {
        let target = Target {
            root: "/test_target_root".into(),
            kind: TargetKind::LocalNvme,
            write_permits: 16,
        };
        let date = NaiveDate::from_ymd_opt(2026, 5, 22).unwrap();
        let got =
            build_destination_path(&target, date, Path::new("card/DCIM/100CANON/IMG_001.CR2"));
        assert!(got.is_some());
        assert_eq!(
            got.unwrap(),
            PathBuf::from("/test_target_root/2026/2026-05-22/IMG_001.CR2")
        );
    }
}
