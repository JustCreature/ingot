//! Command line parsing for `ingot import`.

use std::path::PathBuf;

use chrono::NaiveDate;
use ingot_core::{DateFolders, ExtensionCase, FileKind, ImportOptions, NameTemplate, Organize};
use ingot_develop::{DevelopParams, WhiteBalance};

pub const HELP: &str = "\
ingot [folder]           open the app, optionally on a folder or card
ingot import <source> --dest <folder> [options]
ingot gdrive-login       sign in to Google Drive (see docs/guides/google-drive.md)
ingot develop <raw> <out.jpg> [--exposure <stops>] [--contrast <0.1-10>]
              [--skew <-1-1>] [--wb camera|daylight|<r>,<g>,<b>]

Source
  --no-subfolders          only the top folder of <source>
  --add                    leave photos where they are, only build previews
  --eject                  eject the card after a clean import

Destination
  --dest <folder>          where the photos go
  --subfolder <name>       put everything in this folder under the destination
  --organize <mode>        one (default) | original | date
  --date-format <format>   for --organize date: a chrono format, default %Y/%Y-%m-%d
                           e.g. %Y/%m/%d  %Y/%m-%d  %Y/%B/%d  %Y-%m-%d  %Y%m%d
  --second-copy <folder>   also copy here, without slowing the card (repeatable)
  --gdrive                 also upload to __ingot_backups in My Drive, without slowing the card

File renaming
  --rename <template>      tokens: {name} {folder} {custom} {filenum} {seq} {seq:4}
                           {date} {date:%Y-%m-%d}
  --custom-text <text>     value of {custom}
  --start-number <n>       first value of {seq}, default 1
  --extensions <case>      asis (default) | lower | upper

Which files
  --only <kind>            raw | jpeg
  --since <YYYY-MM-DD>     taken on or after
  --until <YYYY-MM-DD>     taken on or before
  --all                    also take files already imported

Other
  --no-previews            copy only
  --dry-run                show where each file would go, change nothing";

#[derive(Debug)]
pub struct Usage(pub String);

#[derive(Debug)]
pub struct ImportArgs {
    pub source: PathBuf,
    pub destination: Option<PathBuf>,
    pub second_copies: Vec<PathBuf>,
    pub gdrive: bool,
    pub include_subfolders: bool,
    pub options: ImportOptions,
    pub previews: bool,
    pub dry_run: bool,
    pub eject: bool,
}

pub fn history_path() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("ingot").join("import-history.jsonl"))
}

fn date(value: &str) -> Result<NaiveDate, Usage> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| Usage(format!("not a YYYY-MM-DD date: {value}")))
}

pub fn triage_path() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("ingot").join("triage.jsonl"))
}

pub fn gdrive_credentials_path() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("ingot").join("gdrive-token.json"))
}

#[derive(Debug)]
pub enum Command {
    Help,
    Ui(Option<PathBuf>),
    Import(Box<ImportArgs>),
    GdriveLogin,
    Develop(Box<DevelopArgs>),
}

#[derive(Debug)]
pub struct DevelopArgs {
    pub raw: PathBuf,
    pub output: PathBuf,
    pub params: DevelopParams,
}

fn number(value: &str) -> Result<f32, Usage> {
    value
        .parse()
        .map_err(|_| Usage(format!("not a number: {value}")))
}

fn parse_develop<'a>(mut args: impl Iterator<Item = &'a String>) -> Result<Command, Usage> {
    let mut params = DevelopParams::default();
    let mut paths = vec![];
    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .map(String::as_str)
                .ok_or_else(|| Usage(format!("{arg} needs a value")))
        };
        match arg.as_str() {
            "--exposure" => params.exposure = number(value()?)?,
            "--contrast" => params.contrast = number(value()?)?,
            "--skew" => params.skew = number(value()?)?,
            "--wb" => {
                params.white_balance = match value()? {
                    "camera" => WhiteBalance::Camera,
                    "daylight" => WhiteBalance::Daylight,
                    custom => {
                        let parts: Vec<&str> = custom.split(',').collect();
                        let [r, g, b] = parts.as_slice() else {
                            return Err(Usage(format!("not a white balance: {custom}")));
                        };
                        WhiteBalance::Custom([number(r)?, number(g)?, number(b)?])
                    }
                }
            }
            flag if flag.starts_with('-') => return Err(Usage(format!("unknown option: {flag}"))),
            path => paths.push(PathBuf::from(path)),
        }
    }
    let mut paths = paths.into_iter();
    match (paths.next(), paths.next(), paths.next()) {
        (Some(raw), Some(output), None) => Ok(Command::Develop(Box::new(DevelopArgs {
            raw,
            output,
            params,
        }))),
        _ => Err(Usage(
            "develop takes a RAW file and an output file".to_string(),
        )),
    }
}

pub fn parse(args: &[String]) -> Result<Command, Usage> {
    let mut args = args.iter();
    match args.next().map(String::as_str) {
        None => return Ok(Command::Ui(None)),
        Some("help" | "--help" | "-h") => return Ok(Command::Help),
        Some("gdrive-login") => return Ok(Command::GdriveLogin),
        Some("develop") => return parse_develop(args),
        Some("import") => {}
        Some(other) if other.starts_with('-') => {
            return Err(Usage(format!("unknown option: {other}")));
        }
        Some(folder) => {
            let path = PathBuf::from(folder);
            return if path.is_dir() {
                Ok(Command::Ui(Some(path)))
            } else {
                Err(Usage(format!("not a command or a folder: {folder}")))
            };
        }
    }

    let mut source = None;
    let mut destination = None;
    let mut second_copies = vec![];
    let mut gdrive = false;
    let mut include_subfolders = true;
    let mut options = ImportOptions::default();
    let mut organize = None;
    let mut date_format = None;
    let (mut previews, mut dry_run, mut eject, mut add) = (true, false, false, false);

    while let Some(arg) = args.next() {
        let mut value = || {
            args.next()
                .map(String::as_str)
                .ok_or_else(|| Usage(format!("{arg} needs a value")))
        };
        match arg.as_str() {
            "--dest" => destination = Some(PathBuf::from(value()?)),
            "--subfolder" => options.subfolder = Some(PathBuf::from(value()?)),
            "--second-copy" => second_copies.push(PathBuf::from(value()?)),
            "--gdrive" => gdrive = true,
            "--organize" => organize = Some(value()?.to_string()),
            "--date-format" => date_format = Some(value()?.to_string()),
            "--rename" => {
                options.rename =
                    Some(NameTemplate::parse(value()?).map_err(|e| Usage(e.to_string()))?)
            }
            "--custom-text" => options.custom_text = value()?.to_string(),
            "--start-number" => {
                let number = value()?;
                options.start_number = number
                    .parse()
                    .map_err(|_| Usage(format!("not a number: {number}")))?;
            }
            "--extensions" => {
                options.extension_case = match value()? {
                    "asis" => ExtensionCase::AsIs,
                    "lower" => ExtensionCase::Lower,
                    "upper" => ExtensionCase::Upper,
                    other => return Err(Usage(format!("unknown extension case: {other}"))),
                }
            }
            "--only" => {
                options.selection.kinds = Some(vec![match value()? {
                    "raw" => FileKind::Raw,
                    "jpeg" => FileKind::Jpeg,
                    other => return Err(Usage(format!("unknown kind: {other}"))),
                }])
            }
            "--since" => options.selection.since = Some(date(value()?)?),
            "--until" => options.selection.until = Some(date(value()?)?),
            "--all" => options.skip_suspected_duplicates = false,
            "--no-subfolders" => include_subfolders = false,
            "--no-previews" => previews = false,
            "--dry-run" => dry_run = true,
            "--eject" => eject = true,
            "--add" => add = true,
            flag if flag.starts_with('-') => return Err(Usage(format!("unknown option: {flag}"))),
            path if source.is_none() => source = Some(PathBuf::from(path)),
            extra => return Err(Usage(format!("unexpected argument: {extra}"))),
        }
    }

    options.organize = match organize.as_deref() {
        None | Some("one") => Organize::IntoOneFolder,
        Some("original") => Organize::ByOriginalFolders,
        Some("date") => Organize::ByDate(match &date_format {
            Some(format) => DateFolders::new(format).map_err(|e| Usage(e.to_string()))?,
            None => DateFolders::default(),
        }),
        Some(other) => return Err(Usage(format!("unknown organize mode: {other}"))),
    };
    if date_format.is_some() && !matches!(options.organize, Organize::ByDate(_)) {
        return Err(Usage("--date-format needs --organize date".to_string()));
    }

    let source = source.ok_or_else(|| Usage("no source folder given".to_string()))?;
    if add && (destination.is_some() || !second_copies.is_empty() || gdrive) {
        return Err(Usage(
            "--add copies nothing, so it takes no destination".to_string(),
        ));
    }
    if !add && destination.is_none() {
        return Err(Usage(
            "no destination given: use --dest <folder>".to_string(),
        ));
    }

    Ok(Command::Import(Box::new(ImportArgs {
        source,
        destination,
        second_copies,
        gdrive,
        include_subfolders,
        options,
        previews,
        dry_run,
        eject,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(line: &str) -> Result<Box<ImportArgs>, Usage> {
        let args: Vec<String> = line.split_whitespace().map(String::from).collect();
        match parse(&args)? {
            Command::Import(args) => Ok(args),
            other => panic!("expected an import, got {other:?}"),
        }
    }

    #[test]
    fn defaults_go_into_one_folder() {
        let args = parsed("import /card --dest /photos").unwrap();
        assert_eq!(args.options.organize, Organize::IntoOneFolder);
        assert!(args.options.skip_suspected_duplicates);
        assert!(args.include_subfolders && args.previews);
    }

    #[test]
    fn options_are_applied() {
        let args = parsed(
            "import /card --dest /photos --subfolder Wedding --organize date --date-format %Y/%m \
             --rename {custom}-{seq:3} --custom-text Anna --start-number 5 --extensions lower \
             --only raw --since 2026-01-01 --all --second-copy /backup --no-subfolders --gdrive",
        )
        .unwrap();
        assert!(args.gdrive);
        assert_eq!(args.options.subfolder, Some(PathBuf::from("Wedding")));
        assert!(matches!(args.options.organize, Organize::ByDate(_)));
        assert!(args.options.rename.is_some());
        assert_eq!(args.options.start_number, 5);
        assert_eq!(args.options.extension_case, ExtensionCase::Lower);
        assert!(!args.options.skip_suspected_duplicates);
        assert_eq!(args.second_copies, [PathBuf::from("/backup")]);
        assert!(!args.include_subfolders);
    }

    #[test]
    fn develop_options_are_applied() {
        let args: Vec<String> =
            "develop a.CR2 b.jpg --exposure 1.5 --contrast 2 --skew -0.5 --wb 2,1,1.5"
                .split_whitespace()
                .map(String::from)
                .collect();
        let Command::Develop(develop) = parse(&args).unwrap() else {
            panic!("expected develop");
        };
        assert_eq!(develop.params.exposure, 1.5);
        assert_eq!(develop.params.contrast, 2.0);
        assert_eq!(develop.params.skew, -0.5);
        assert_eq!(
            develop.params.white_balance,
            WhiteBalance::Custom([2.0, 1.0, 1.5])
        );
    }

    #[test]
    fn develop_mistakes_are_reported() {
        for line in [
            "develop",
            "develop a.CR2",
            "develop a b c",
            "develop a b --wb 1,2",
            "develop a b --exposure x",
        ] {
            let args: Vec<String> = line.split_whitespace().map(String::from).collect();
            assert!(parse(&args).is_err(), "{line}");
        }
    }

    #[test]
    fn mistakes_are_reported() {
        for line in [
            "import /card",
            "import --dest /photos",
            "import /card --dest /photos --organize sideways",
            "import /card --dest /photos --date-format %Y",
            "import /card --dest /photos --rename {nope}",
            "import /card --dest /photos --since yesterday",
            "import /card --dest /photos --wat",
            "import /card --add --dest /photos",
            "import /card --dest",
            "import /card --add --gdrive",
            "import /card --dest /photos --gdrive Elsewhere",
        ] {
            assert!(parsed(line).is_err(), "{line}");
        }
    }
}
