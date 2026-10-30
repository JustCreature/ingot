//! Command line parsing for `ingot import`.

use std::path::PathBuf;

use chrono::NaiveDate;
use ingot_core::{DateFolders, ExtensionCase, FileKind, ImportOptions, NameTemplate, Organize};

pub const HELP: &str = "\
ingot import <source> --dest <folder> [options]
ingot gdrive-login       sign in to Google Drive (see docs/guides/google-drive.md)

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

pub fn gdrive_credentials_path() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("ingot").join("gdrive-token.json"))
}

#[derive(Debug)]
pub enum Command {
    Help,
    Import(Box<ImportArgs>),
    GdriveLogin,
}

pub fn parse(args: &[String]) -> Result<Command, Usage> {
    let mut args = args.iter();
    match args.next().map(String::as_str) {
        None | Some("help" | "--help" | "-h") => return Ok(Command::Help),
        Some("gdrive-login") => return Ok(Command::GdriveLogin),
        Some("import") => {}
        Some(other) => return Err(Usage(format!("unknown command: {other}"))),
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
