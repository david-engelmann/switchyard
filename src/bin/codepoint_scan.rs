//! Scan text files for em dash and en dash codepoints.
//!
//! A std only Rust port of the setup scanner. It counts U+2014 and
//! U+2013 in every file under the scan root and exits 1 when either
//! count is nonzero. What it counts, the counts it prints, the
//! directory names it excludes below the scan root, and the binary
//! suffixes it skips match the setup scanner, and four differences
//! from that scanner stand named here as deliberate behaviour of
//! the port. The excluded directory names add target, the cargo
//! output directory, because in a Rust repo that is the generated
//! directory the gitignore names, and the scanner must be able to
//! run over a tree the local gate has already built in. The scan
//! root defaults to the current directory, where the setup scanner
//! defaulted to the root of the repo it shipped in. The setup pins
//! name the root explicitly when they drive the scanner, and the
//! workflow scan step names none and scans the checkout directory
//! it runs in. A scan root that does not stand is a scan error at
//! exit 2, where the setup scanner exited 0 with zero counts. A
//! file that cannot be read is a clean scan error at exit 2, where
//! the setup scanner let the failure escape as a traceback at
//! exit 1. The walk excludes directories by name below the scan
//! root, so the name of an ancestor of the root can never exclude
//! the root itself, and every file in the tree is scanned,
//! whatever its name or suffix, apart from the known binary
//! suffixes, and the walk follows no directory symlink, and a
//! file that does not decode as UTF-8 fails the scan loudly
//! instead of passing unseen.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const EM_DASH: char = '\u{2014}';
const EN_DASH: char = '\u{2013}';
const BINARY_SUFFIXES: [&str; 13] = [
    "png", "jpg", "jpeg", "gif", "ico", "webp", "pdf", "zip", "gz", "tar", "whl", "so", "dylib",
];
const EXCLUDED_DIRS: [&str; 9] = [
    ".git",
    "__pycache__",
    ".pytest_cache",
    ".venv",
    "venv",
    "node_modules",
    "build",
    "dist",
    "target",
];

struct ScanError(String);

fn has_binary_suffix(path: &Path) -> bool {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) => BINARY_SUFFIXES.contains(&ext.to_lowercase().as_str()),
        None => false,
    }
}

fn collect(root: &Path, out: &mut Vec<PathBuf>) -> Result<(), ScanError> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    let entries = std::fs::read_dir(root)
        .map_err(|err| ScanError(format!("cannot read {}, {err}", root.display())))?;
    for entry in entries {
        let entry =
            entry.map_err(|err| ScanError(format!("cannot read {}, {err}", root.display())))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| ScanError(format!("cannot read {}, {err}", path.display())))?;
        if file_type.is_dir() {
            dirs.push(path);
        } else if !file_type.is_symlink() || !path.is_dir() {
            files.push(path);
        }
    }
    dirs.sort();
    files.sort();
    for file in files {
        if !has_binary_suffix(&file) {
            out.push(file);
        }
    }
    for dir in dirs {
        let name = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !EXCLUDED_DIRS.contains(&name.as_str()) {
            collect(&dir, out)?;
        }
    }
    Ok(())
}

fn scan(root: &Path) -> Result<(usize, usize, String), ScanError> {
    let mut files = Vec::new();
    collect(root, &mut files)?;
    let mut em_count = 0;
    let mut en_count = 0;
    let mut hits = String::new();
    for path in &files {
        let bytes = std::fs::read(path)
            .map_err(|err| ScanError(format!("cannot read {}, {err}", path.display())))?;
        let text = std::str::from_utf8(&bytes).map_err(|err| {
            ScanError(format!("cannot decode {} as UTF-8, {err}", path.display()))
        })?;
        let file_em = text.matches(EM_DASH).count();
        let file_en = text.matches(EN_DASH).count();
        if file_em > 0 || file_en > 0 {
            hits.push_str(&format!(
                "{}: em dash {file_em}, en dash {file_en}\n",
                path.display()
            ));
        }
        em_count += file_em;
        en_count += file_en;
    }
    Ok((em_count, en_count, hits))
}

fn main() -> ExitCode {
    // The root is a path, taken from args_os so a root whose
    // name is not Unicode scans instead of panicking the way
    // std::env::args would on the argument.
    let root = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    match scan(&root) {
        Ok((em_count, en_count, hits)) => {
            let output =
                format!("{hits}em dash U+2014 count {em_count}\nen dash U+2013 count {en_count}\n");
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            if let Err(err) = lock.write_all(output.as_bytes()) {
                report_scan_error(&format!("cannot write the scan output, {err}"));
                return ExitCode::from(2);
            }
            if em_count > 0 || en_count > 0 {
                ExitCode::from(1)
            } else {
                ExitCode::from(0)
            }
        }
        Err(ScanError(message)) => {
            report_scan_error(&message);
            ExitCode::from(2)
        }
    }
}

/// Write one scan error line to a locked stderr, fallibly: the
/// error stream can itself be the stream that failed, and a
/// failed report of the failure changes nothing the caller can
/// still act on (SY-A42).
fn report_scan_error(message: &str) {
    let stderr = std::io::stderr();
    let mut lock = stderr.lock();
    let _ = lock.write_all(format!("scan error, {message}\n").as_bytes());
}
