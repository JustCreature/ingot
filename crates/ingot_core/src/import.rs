//! Import options and the plan they produce: which files get copied, and where
//! each one lands below a target's root.
//!
//! Planning does no copying, so the same plan can be shown as a dry run and then
//! handed to the copy engine. Every target stores a file at the same relative path.

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    io::Write,
    path::{Component, Path, PathBuf},
    sync::RwLock,
    time::SystemTime,
};

use chrono::{
    DateTime, Local, NaiveDate, NaiveDateTime,
    format::{Item, StrftimeItems},
};
use serde::{Deserialize, Serialize};

use crate::{
    AssetKey, FileKind, FileMeta, PhotoAsset, ScanResponse,
    replicate::{ReplicationError, ReplicationFailure},
};

/// How files are arranged below the destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Organize {
    IntoOneFolder,
    /// Keeps each file's folder path relative to the import source.
    ByOriginalFolders,
    ByDate(DateFolders),
}

/// A chrono format for date folders, where `/` starts a nested folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DateFolders(String);

impl DateFolders {
    pub const YEAR_DATE: &str = "%Y/%Y-%m-%d";
    pub const YEAR_MONTH_DAY: &str = "%Y/%m/%d";
    pub const YEAR_MONTHDAY: &str = "%Y/%m-%d";
    pub const YEAR_MONTH_NAME_DAY: &str = "%Y/%B/%d";
    pub const DATE: &str = "%Y-%m-%d";
    pub const COMPACT: &str = "%Y%m%d";

    pub fn new(format: &str) -> Result<Self, ReplicationError> {
        check_date_format(format)?;
        Ok(DateFolders(format.to_string()))
    }

    fn render(&self, at: NaiveDate) -> PathBuf {
        PathBuf::from(at.format(&self.0).to_string())
    }
}

impl Default for DateFolders {
    fn default() -> Self {
        DateFolders(Self::YEAR_DATE.to_string())
    }
}

fn check_date_format(format: &str) -> Result<(), ReplicationError> {
    if StrftimeItems::new(format).any(|item| matches!(item, Item::Error)) {
        return Err(format!("invalid date format: {format}").into());
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum NameToken {
    Text(String),
    Name,
    Folder,
    Custom,
    FileNumber,
    Sequence { width: usize },
    Date(String),
}

/// A file naming template such as `{date:%Y%m%d}-{custom}-{seq:4}`. Tokens: `{name}`
/// (original name), `{folder}` (source folder name), `{custom}` (custom text),
/// `{filenum}` (trailing digits of the original name), `{seq}` or `{seq:WIDTH}`
/// (import sequence), `{date}` or `{date:FORMAT}`. The extension is added separately.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameTemplate(Vec<NameToken>);

impl NameTemplate {
    pub fn parse(template: &str) -> Result<Self, ReplicationError> {
        let mut tokens = vec![];
        let mut rest = template;
        while let Some(open) = rest.find('{') {
            if open > 0 {
                tokens.push(NameToken::Text(rest[..open].to_string()));
            }
            let close = rest[open..]
                .find('}')
                .ok_or_else(|| format!("unclosed token in template: {template}"))?
                + open;
            tokens.push(parse_token(&rest[open + 1..close])?);
            rest = &rest[close + 1..];
        }
        if !rest.is_empty() {
            tokens.push(NameToken::Text(rest.to_string()));
        }
        if tokens.is_empty() {
            return Err("empty name template".into());
        }
        Ok(NameTemplate(tokens))
    }

    fn render(&self, input: &NameInput) -> String {
        let mut name = String::new();
        for token in &self.0 {
            match token {
                NameToken::Text(text) => name.push_str(text),
                NameToken::Name => name.push_str(input.stem),
                NameToken::Folder => name.push_str(input.folder),
                NameToken::Custom => name.push_str(input.custom),
                NameToken::FileNumber => {
                    let digits = input.stem.len()
                        - input
                            .stem
                            .trim_end_matches(|c: char| c.is_ascii_digit())
                            .len();
                    name.push_str(&input.stem[input.stem.len() - digits..]);
                }
                NameToken::Sequence { width } => {
                    name.push_str(&format!("{:0width$}", input.sequence))
                }
                NameToken::Date(format) => name.push_str(&input.taken.format(format).to_string()),
            }
        }
        name
    }
}

fn parse_token(token: &str) -> Result<NameToken, ReplicationError> {
    let (name, argument) = match token.split_once(':') {
        Some((name, argument)) => (name, Some(argument)),
        None => (token, None),
    };
    match (name, argument) {
        ("name", None) => Ok(NameToken::Name),
        ("folder", None) => Ok(NameToken::Folder),
        ("custom", None) => Ok(NameToken::Custom),
        ("filenum", None) => Ok(NameToken::FileNumber),
        ("seq", None) => Ok(NameToken::Sequence { width: 1 }),
        ("seq", Some(width)) => Ok(NameToken::Sequence {
            width: width
                .parse()
                .map_err(|_| format!("invalid sequence width: {width}"))?,
        }),
        ("date", argument) => {
            let format = argument.unwrap_or("%Y%m%d");
            check_date_format(format)?;
            Ok(NameToken::Date(format.to_string()))
        }
        _ => Err(format!("unknown template token: {{{token}}}").into()),
    }
}

struct NameInput<'a> {
    stem: &'a str,
    folder: &'a str,
    custom: &'a str,
    sequence: u64,
    taken: NaiveDateTime,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExtensionCase {
    #[default]
    AsIs,
    Lower,
    Upper,
}

/// Which scanned files an import takes. Everything is taken by default.
#[derive(Clone, Debug, Default)]
pub struct Selection {
    pub kinds: Option<Vec<FileKind>>,
    pub since: Option<NaiveDate>,
    pub until: Option<NaiveDate>,
    /// Hand-picked files. `None` means no hand-picking.
    pub paths: Option<HashSet<PathBuf>>,
}

#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// A folder below the destination that everything goes into.
    pub subfolder: Option<PathBuf>,
    pub organize: Organize,
    pub rename: Option<NameTemplate>,
    pub custom_text: String,
    pub start_number: u64,
    pub extension_case: ExtensionCase,
    pub selection: Selection,
    /// Leaves out files the import history has already seen on every target.
    pub skip_suspected_duplicates: bool,
    /// Dates files by their modified time, from the directory walk, instead of EXIF.
    /// For an import that starts before any file has been opened, so a photo's folder
    /// and name don't change depending on whether its header has been read yet.
    pub use_file_dates: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        ImportOptions {
            subfolder: None,
            organize: Organize::IntoOneFolder,
            rename: None,
            custom_text: String::new(),
            start_number: 1,
            extension_case: ExtensionCase::AsIs,
            selection: Selection::default(),
            skip_suspected_duplicates: true,
            use_file_dates: false,
        }
    }
}

/// What makes two files suspected duplicates: same original name, capture time and
/// size. Cheap enough to check without reading a single byte of the file.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DuplicateKey {
    name: String,
    taken: NaiveDateTime,
    size: u64,
}

/// Files already imported to every target, kept in a JSON-lines file. Only ever
/// appended to. A damaged line just means that file gets checked by hash again.
pub struct ImportHistory {
    path: PathBuf,
    seen: HashSet<DuplicateKey>,
}

impl ImportHistory {
    pub fn open(path: PathBuf) -> Result<Self, ReplicationError> {
        let seen = match fs::read_to_string(&path) {
            Ok(text) => text
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect(),
            Err(err) if err.kind() == io::ErrorKind::NotFound => HashSet::new(),
            Err(err) => return Err(format!("could not read {}: {err}", path.display()).into()),
        };
        Ok(ImportHistory { path, seen })
    }

    pub fn contains(&self, key: &DuplicateKey) -> bool {
        self.seen.contains(key)
    }

    pub fn record(
        &mut self,
        keys: impl IntoIterator<Item = DuplicateKey>,
    ) -> Result<(), ReplicationError> {
        let mut lines = String::new();
        for key in keys {
            if self.seen.insert(key.clone()) {
                lines.push_str(&serde_json::to_string(&key)?);
                lines.push('\n');
            }
        }
        if lines.is_empty() {
            return Ok(());
        }
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut file = fs::File::options()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(lines.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }
}

/// One file the import will copy.
#[derive(Clone, Debug)]
pub struct PlannedFile {
    pub source: PathBuf,
    /// Where it lands below each target's root.
    pub relative: PathBuf,
    pub modified: Option<SystemTime>,
    pub size: u64,
    pub duplicate_key: DuplicateKey,
}

#[derive(Debug, Default)]
pub struct ImportPlan {
    pub files: Vec<PlannedFile>,
    /// Left out by `skip_suspected_duplicates`.
    pub skipped_duplicates: Vec<PathBuf>,
    /// Files that could not be planned, such as ones that can't be read.
    pub failures: Vec<ReplicationFailure>,
    /// The first sequence number this plan didn't use, for planning more files later.
    pub next_number: u64,
}

struct PlannedAsset {
    key: AssetKey,
    captured_at: Option<NaiveDateTime>,
    files: Vec<(FileKind, PathBuf)>,
}

type AssetsAndMeta = (Vec<PlannedAsset>, HashMap<PathBuf, FileMeta>);

fn assets_snapshot(scan: &RwLock<ScanResponse>) -> Result<AssetsAndMeta, ReplicationError> {
    let scan = scan
        .read()
        .map_err(|err| format!("scan store lock poisoned: {err}"))?;
    let mut assets: Vec<PlannedAsset> = scan
        .assets
        .iter()
        .map(|(key, asset): (&AssetKey, &PhotoAsset)| PlannedAsset {
            key: key.clone(),
            captured_at: asset.exif_data.captured_at,
            files: asset.files.get_all().clone(),
        })
        .collect();
    // A file the scanner displaced from its asset is still a photo on the card.
    for collision in &scan.collisions {
        if let Some(asset) = assets.iter_mut().find(|a| a.key == collision.key) {
            asset
                .files
                .push((collision.kind, collision.displaced.clone()));
        }
    }
    Ok((assets, scan.file_meta.clone()))
}

fn safe_relative(path: PathBuf) -> Result<PathBuf, ReplicationError> {
    if path.as_os_str().is_empty() || !path.components().all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(format!("not a path below the destination: {}", path.display()).into());
    }
    Ok(path)
}

fn file_name_with_extension(stem: &str, path: &Path, case: ExtensionCase) -> String {
    match path.extension() {
        None => stem.to_string(),
        Some(extension) => {
            let extension = extension.to_string_lossy();
            let extension = match case {
                ExtensionCase::AsIs => extension.into_owned(),
                ExtensionCase::Lower => extension.to_lowercase(),
                ExtensionCase::Upper => extension.to_uppercase(),
            };
            format!("{stem}.{extension}")
        }
    }
}

struct Stat {
    path: PathBuf,
    kind: FileKind,
    size: u64,
    modified: Option<SystemTime>,
}

/// Works out what an import will do. It uses the sizes and dates the scan already
/// has, so with a normal scan this touches no disk at all and a slow card can't hold
/// it up. Only a file the scan has no data for is statted. The order is stable: by
/// capture time, then name, so a repeated plan numbers files the same way. RAW and
/// JPEG of a pair share one sequence number.
pub fn plan_import(
    scan: &RwLock<ScanResponse>,
    source_root: &Path,
    options: &ImportOptions,
    history: Option<&ImportHistory>,
) -> Result<ImportPlan, ReplicationError> {
    let mut plan = ImportPlan::default();
    let mut assets = Vec::new();

    let (snapshot, file_meta) = assets_snapshot(scan)?;
    for asset in snapshot {
        let mut stats = Vec::with_capacity(asset.files.len());
        for (kind, path) in &asset.files {
            let known = match file_meta.get(path) {
                Some(meta) => Ok(*meta),
                None => fs::metadata(path).map(|metadata| FileMeta {
                    size: metadata.len(),
                    modified: metadata.modified().ok(),
                }),
            };
            match known {
                Ok(meta) => stats.push(Stat {
                    path: path.clone(),
                    kind: *kind,
                    size: meta.size,
                    modified: meta.modified,
                }),
                Err(err) => plan.failures.push(ReplicationFailure {
                    source: Some(path.clone()),
                    target: None,
                    error: format!("failed to read {}: {err}", path.display()).into(),
                }),
            }
        }
        // A frame with no EXIF date uses the file's own, which also comes from the
        // camera's clock.
        let file_date = stats
            .iter()
            .filter_map(|stat| stat.modified)
            .min()
            .map(|at| DateTime::<Local>::from(at).naive_local());
        let taken = if options.use_file_dates {
            file_date.or(asset.captured_at)
        } else {
            asset.captured_at.or(file_date)
        };
        if let Some(taken) = taken {
            assets.push((taken, asset.key, stats));
        } else {
            for stat in stats {
                plan.failures.push(ReplicationFailure {
                    source: Some(stat.path),
                    target: None,
                    error: "no capture date and no file date".into(),
                });
            }
        }
    }
    assets.sort_by(|a, b| (a.0, &a.1.stem, &a.1.dir).cmp(&(b.0, &b.1.stem, &b.1.dir)));

    let selection = &options.selection;
    let mut sequence = options.start_number;
    for (taken, _key, stats) in assets {
        let date = taken.date();
        if selection.since.is_some_and(|since| date < since)
            || selection.until.is_some_and(|until| date > until)
        {
            continue;
        }
        let mut used_sequence = false;
        for stat in stats {
            if selection
                .kinds
                .as_ref()
                .is_some_and(|kinds| !kinds.contains(&stat.kind))
                || selection
                    .paths
                    .as_ref()
                    .is_some_and(|paths| !paths.contains(&stat.path))
            {
                continue;
            }

            let duplicate_key = DuplicateKey {
                name: stat
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                taken,
                size: stat.size,
            };
            if options.skip_suspected_duplicates
                && history.is_some_and(|history| history.contains(&duplicate_key))
            {
                plan.skipped_duplicates.push(stat.path);
                continue;
            }

            used_sequence = true;
            match planned_relative(&stat.path, source_root, options, sequence, taken) {
                Ok(relative) => plan.files.push(PlannedFile {
                    source: stat.path,
                    relative,
                    modified: stat.modified,
                    size: stat.size,
                    duplicate_key,
                }),
                Err(error) => plan.failures.push(ReplicationFailure {
                    source: Some(stat.path),
                    target: None,
                    error,
                }),
            }
        }
        if used_sequence {
            sequence += 1;
        }
    }
    plan.next_number = sequence;
    Ok(plan)
}

/// Where one file lands below a target's root. Pure: no disk access, so it is safe
/// to call on every frame to show the destination.
pub fn planned_relative(
    source: &Path,
    source_root: &Path,
    options: &ImportOptions,
    sequence: u64,
    taken: NaiveDateTime,
) -> Result<PathBuf, ReplicationError> {
    let mut relative = PathBuf::new();
    if let Some(subfolder) = &options.subfolder {
        relative.push(subfolder);
    }
    let source_dir = source.parent().unwrap_or(Path::new(""));
    match &options.organize {
        Organize::IntoOneFolder => {}
        Organize::ByOriginalFolders => {
            let inner = source_dir.strip_prefix(source_root).map_err(|_| {
                format!(
                    "{} is not inside the import source {}",
                    source.display(),
                    source_root.display()
                )
            })?;
            relative.push(inner);
        }
        Organize::ByDate(folders) => relative.push(folders.render(taken.date())),
    }

    let stem = source
        .file_stem()
        .ok_or_else(|| format!("no file name in {}", source.display()))?
        .to_string_lossy();
    let name = match &options.rename {
        Some(template) => {
            let folder = source_dir
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default();
            let name = template.render(&NameInput {
                stem: &stem,
                folder: &folder,
                custom: &options.custom_text,
                sequence,
                taken,
            });
            if name.is_empty() || name.contains(['/', '\\', '\0']) {
                return Err(format!("template produced an unusable name: {name:?}").into());
            }
            name
        }
        None => stem.into_owned(),
    };
    relative.push(file_name_with_extension(
        &name,
        source,
        options.extension_case,
    ));
    safe_relative(relative)
}

#[cfg(test)]
mod tests {
    use tempfile::{TempDir, tempdir};

    use super::*;

    fn taken(day: u32, second: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 5, day)
            .unwrap()
            .and_hms_opt(12, 0, second)
            .unwrap()
    }

    /// Builds a card with `(relative path, kind, capture time)` files.
    fn card(files: &[(&str, FileKind, NaiveDateTime)]) -> (TempDir, RwLock<ScanResponse>) {
        let tmp = tempdir().unwrap();
        let mut assets: HashMap<AssetKey, PhotoAsset> = HashMap::new();
        for (relative, kind, at) in files {
            let path = tmp.path().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, relative.as_bytes()).unwrap();
            let key = AssetKey {
                dir: path.parent().unwrap().to_path_buf(),
                stem: path.file_stem().unwrap().to_string_lossy().to_lowercase(),
            };
            match assets.get_mut(&key) {
                Some(asset) => asset.files.insert(*kind, path).unwrap(),
                None => {
                    let mut asset = PhotoAsset::new(*kind, path);
                    asset.exif_data.captured_at = Some(*at);
                    assets.insert(key, asset);
                }
            }
        }
        (
            tmp,
            RwLock::new(ScanResponse {
                assets,
                collisions: vec![],
                errors: vec![],
                ignored: vec![],
                file_meta: HashMap::new(),
            }),
        )
    }

    fn relatives(plan: &ImportPlan) -> Vec<String> {
        let mut paths: Vec<String> = plan
            .files
            .iter()
            .map(|file| file.relative.to_string_lossy().into_owned())
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn organize_modes_place_files_as_asked() {
        let (tmp, scan) = card(&[
            ("DCIM/100CANON/IMG_1.CR2", FileKind::Raw, taken(22, 0)),
            ("DCIM/101CANON/IMG_2.JPG", FileKind::Jpeg, taken(23, 0)),
        ]);
        let plan_with = |organize, subfolder: Option<&str>| {
            let options = ImportOptions {
                organize,
                subfolder: subfolder.map(PathBuf::from),
                ..Default::default()
            };
            relatives(&plan_import(&scan, tmp.path(), &options, None).unwrap())
        };

        assert_eq!(
            plan_with(Organize::IntoOneFolder, Some("Wedding")),
            ["Wedding/IMG_1.CR2", "Wedding/IMG_2.JPG"]
        );
        assert_eq!(
            plan_with(Organize::ByOriginalFolders, None),
            ["DCIM/100CANON/IMG_1.CR2", "DCIM/101CANON/IMG_2.JPG"]
        );
        assert_eq!(
            plan_with(Organize::ByDate(DateFolders::default()), None),
            ["2026/2026-05-22/IMG_1.CR2", "2026/2026-05-23/IMG_2.JPG"]
        );
        assert_eq!(
            plan_with(
                Organize::ByDate(DateFolders::new(DateFolders::YEAR_MONTH_DAY).unwrap()),
                Some("Trip")
            ),
            ["Trip/2026/05/22/IMG_1.CR2", "Trip/2026/05/23/IMG_2.JPG"]
        );
    }

    #[test]
    fn rename_numbers_by_capture_time_and_a_pair_shares_its_number() {
        let (tmp, scan) = card(&[
            ("a/IMG_9999.CR2", FileKind::Raw, taken(22, 0)),
            ("a/IMG_9999.JPG", FileKind::Jpeg, taken(22, 0)),
            ("b/IMG_0001.CR2", FileKind::Raw, taken(22, 5)),
        ]);
        let options = ImportOptions {
            rename: Some(NameTemplate::parse("{custom}-{date}-{seq:3}_{filenum}").unwrap()),
            custom_text: "Anna".into(),
            start_number: 7,
            extension_case: ExtensionCase::Lower,
            ..Default::default()
        };

        let plan = plan_import(&scan, tmp.path(), &options, None).unwrap();

        assert_eq!(
            relatives(&plan),
            [
                "Anna-20260522-007_9999.cr2",
                "Anna-20260522-007_9999.jpg",
                "Anna-20260522-008_0001.cr2",
            ]
        );
    }

    #[test]
    fn paths_that_leave_the_destination_are_rejected() {
        let (tmp, scan) = card(&[("IMG_1.CR2", FileKind::Raw, taken(22, 0))]);
        for options in [
            ImportOptions {
                subfolder: Some("../outside".into()),
                ..Default::default()
            },
            ImportOptions {
                subfolder: Some("/etc".into()),
                ..Default::default()
            },
            ImportOptions {
                rename: Some(NameTemplate::parse("a/b").unwrap()),
                ..Default::default()
            },
            ImportOptions {
                organize: Organize::ByDate(DateFolders::new("../%Y").unwrap()),
                ..Default::default()
            },
        ] {
            let plan = plan_import(&scan, tmp.path(), &options, None).unwrap();
            assert!(plan.files.is_empty(), "{options:?}");
            assert_eq!(plan.failures.len(), 1, "{options:?}");
        }
    }

    #[test]
    fn a_scanned_folder_plans_without_touching_the_disk_again() {
        let tmp = tempdir().unwrap();
        let card = tmp.path().join("card");
        fs::create_dir_all(&card).unwrap();
        fs::write(card.join("IMG_1.CR2"), b"12345").unwrap();
        fs::write(card.join("IMG_1.JPG"), b"123").unwrap();
        let scan = RwLock::new(crate::scan_source_dir(&card));

        // With the card gone, only data from the scan can answer.
        fs::remove_dir_all(&card).unwrap();
        let options = ImportOptions {
            organize: Organize::ByDate(DateFolders::default()),
            use_file_dates: true,
            ..Default::default()
        };
        let plan = plan_import(&scan, &card, &options, None).unwrap();

        assert!(plan.failures.is_empty(), "{:?}", plan.failures);
        assert_eq!(plan.files.len(), 2);
        let total: u64 = plan.files.iter().map(|file| file.size).sum();
        assert_eq!(total, 8);
        assert_eq!(plan.next_number, 2);
    }

    #[test]
    fn file_dates_win_over_exif_only_when_asked() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("IMG_1.JPG"), b"x").unwrap();
        let mut response = crate::scan_source_dir(tmp.path());
        response
            .assets
            .values_mut()
            .next()
            .unwrap()
            .exif_data
            .captured_at = Some(taken(22, 0));
        let scan = RwLock::new(response);

        let plan_with = |use_file_dates| {
            let options = ImportOptions {
                organize: Organize::ByDate(DateFolders::new(DateFolders::DATE).unwrap()),
                use_file_dates,
                ..Default::default()
            };
            relatives(&plan_import(&scan, tmp.path(), &options, None).unwrap())
        };
        assert_eq!(plan_with(false), ["2026-05-22/IMG_1.JPG"]);
        assert_ne!(plan_with(true), ["2026-05-22/IMG_1.JPG"]);
    }

    #[test]
    fn numbering_can_continue_for_files_added_later() {
        let (tmp, scan) = card(&[
            ("IMG_1.JPG", FileKind::Jpeg, taken(22, 0)),
            ("IMG_2.JPG", FileKind::Jpeg, taken(22, 1)),
        ]);
        let pick = |name: &str, start| {
            let options = ImportOptions {
                rename: Some(NameTemplate::parse("{seq:2}").unwrap()),
                start_number: start,
                selection: Selection {
                    paths: Some(HashSet::from([tmp.path().join(name)])),
                    ..Default::default()
                },
                ..Default::default()
            };
            plan_import(&scan, tmp.path(), &options, None).unwrap()
        };
        let first = pick("IMG_2.JPG", 1);
        assert_eq!(relatives(&first), ["01.JPG"]);
        assert_eq!(relatives(&pick("IMG_1.JPG", first.next_number)), ["02.JPG"]);
    }

    #[test]
    fn bad_templates_and_formats_fail_up_front() {
        assert!(NameTemplate::parse("{nope}").is_err());
        assert!(NameTemplate::parse("{seq").is_err());
        assert!(NameTemplate::parse("{seq:x}").is_err());
        assert!(NameTemplate::parse("").is_err());
        assert!(NameTemplate::parse("{date:%Q}").is_err());
        assert!(DateFolders::new("%Q").is_err());
    }

    #[test]
    fn selection_filters_by_kind_date_and_hand_picked_paths() {
        let (tmp, scan) = card(&[
            ("IMG_1.CR2", FileKind::Raw, taken(21, 0)),
            ("IMG_1.JPG", FileKind::Jpeg, taken(21, 0)),
            ("IMG_2.CR2", FileKind::Raw, taken(22, 0)),
            ("IMG_3.CR2", FileKind::Raw, taken(23, 0)),
        ]);
        let plan_with = |selection| {
            let options = ImportOptions {
                selection,
                ..Default::default()
            };
            relatives(&plan_import(&scan, tmp.path(), &options, None).unwrap())
        };

        assert_eq!(
            plan_with(Selection {
                kinds: Some(vec![FileKind::Jpeg]),
                ..Default::default()
            }),
            ["IMG_1.JPG"]
        );
        assert_eq!(
            plan_with(Selection {
                since: Some(taken(22, 0).date()),
                until: Some(taken(22, 0).date()),
                ..Default::default()
            }),
            ["IMG_2.CR2"]
        );
        assert_eq!(
            plan_with(Selection {
                paths: Some(HashSet::from([tmp.path().join("IMG_3.CR2")])),
                ..Default::default()
            }),
            ["IMG_3.CR2"]
        );
    }

    #[test]
    fn history_skips_only_what_was_recorded_and_survives_a_reopen() {
        let (tmp, scan) = card(&[
            ("IMG_1.CR2", FileKind::Raw, taken(22, 0)),
            ("IMG_2.CR2", FileKind::Raw, taken(22, 1)),
        ]);
        let history_path = tmp.path().join("state/history.jsonl");
        let options = ImportOptions::default();

        let mut history = ImportHistory::open(history_path.clone()).unwrap();
        let plan = plan_import(&scan, tmp.path(), &options, Some(&history)).unwrap();
        assert_eq!(plan.files.len(), 2);
        history
            .record([plan.files[0].duplicate_key.clone()])
            .unwrap();

        let history = ImportHistory::open(history_path).unwrap();
        let plan = plan_import(&scan, tmp.path(), &options, Some(&history)).unwrap();
        assert_eq!(relatives(&plan), ["IMG_2.CR2"]);
        assert_eq!(plan.skipped_duplicates.len(), 1);

        let everything = ImportOptions {
            skip_suspected_duplicates: false,
            ..Default::default()
        };
        let plan = plan_import(&scan, tmp.path(), &everything, Some(&history)).unwrap();
        assert_eq!(plan.files.len(), 2);
    }
}
