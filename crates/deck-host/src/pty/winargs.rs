//! Windows command lines: MSVCRT argv quoting, `PATH`/`PATHEXT` program resolution and
//! `.cmd`/`.bat` programs through `cmd.exe /d /v:off /s /c`. Pure string functions (tested
//! on every OS).

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Appends `arg` so `CommandLineToArgvW` / the MSVCRT parser yields it unchanged.
/// `force` quotes even arguments that would not need it.
pub(crate) fn quote_arg(arg: &str, force: bool, out: &mut String) {
    let needs = force || arg.is_empty() || arg.contains([' ', '\t', '\n', '\x0b', '"']);
    if !needs {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
}

/// Whether `program` is a batch script (run through `cmd.exe`).
pub(crate) fn is_batch(program: &Path) -> bool {
    program
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"))
}

/// Resolves a bare program name through `path` (`;`-separated) and `pathext` the way a shell
/// would (CreateProcess alone only appends `.exe`). Names with a directory part, and names
/// not found, are returned unchanged.
pub(crate) fn resolve_program(
    program: &Path,
    path: Option<&str>,
    pathext: Option<&str>,
) -> PathBuf {
    let has_dir = program.components().count() > 1 || program.is_absolute();
    let Some(path) = path.filter(|_| !has_dir) else {
        return program.to_path_buf();
    };
    let exts: Vec<&str> = pathext
        .unwrap_or(".COM;.EXE;.BAT;.CMD")
        .split(';')
        .filter(|e| !e.is_empty())
        .collect();
    let has_ext = program.extension().is_some();
    for dir in path.split(';').filter(|d| !d.is_empty()) {
        let dir = Path::new(dir.trim_matches('"'));
        if has_ext {
            let candidate = dir.join(program);
            if candidate.is_file() {
                return candidate;
            }
        }
        for ext in &exts {
            let mut name = program.as_os_str().to_owned();
            name.push(ext.to_ascii_lowercase());
            let candidate = dir.join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    program.to_path_buf()
}

/// Full command line for CreateProcessW. Batch programs become
/// `<comspec> /d /v:off /s /c ""<program>" "<arg>"…"` (every argument quoted, characters
/// cmd.exe would still interpret inside quotes rejected; delayed expansion off so `!` survives).
pub(crate) fn command_line(program: &Path, args: &[String], comspec: &str) -> Result<String> {
    let program_str = program.to_string_lossy();
    let mut line = String::new();
    if is_batch(program) {
        if let Some(bad) = args
            .iter()
            .find(|a| a.contains(['"', '%', '\r', '\n', '\0']))
        {
            return Err(Error::UnsafeBatchArgument(bad.clone()));
        }
        quote_arg(comspec, false, &mut line);
        line.push_str(" /d /v:off /s /c \"\"");
        line.push_str(&program_str);
        line.push('"');
        for arg in args {
            line.push(' ');
            quote_arg(arg, true, &mut line);
        }
        line.push('"');
    } else {
        // argv[0] is split at the first space unless quoted; it is never escape-parsed.
        line.push('"');
        line.push_str(&program_str);
        line.push('"');
        for arg in args {
            line.push(' ');
            quote_arg(arg, false, &mut line);
        }
    }
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(arg: &str) -> String {
        let mut s = String::new();
        quote_arg(arg, false, &mut s);
        s
    }

    #[test]
    fn msvcrt_quoting() {
        assert_eq!(q("plain"), "plain");
        assert_eq!(q(""), "\"\"");
        assert_eq!(q("a b"), "\"a b\"");
        assert_eq!(q(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(q(r"C:\dir with space\"), r#""C:\dir with space\\""#);
        assert_eq!(q(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(q(r"a\\b c"), r#""a\\b c""#);
        assert_eq!(q(r"no\space"), r"no\space");
    }

    #[test]
    fn exe_and_batch_command_lines() {
        let exe = command_line(
            Path::new(r"C:\Program Files\omp\omp.exe"),
            &["--x".into(), "a b".into()],
            "cmd.exe",
        )
        .unwrap();
        assert_eq!(exe, r#""C:\Program Files\omp\omp.exe" --x "a b""#);
        let bat = command_line(
            Path::new(r"C:\npm\omp.CMD"),
            &["--resume".into(), r"C:\a b\".into(), "x&y".into()],
            r"C:\Windows\System32\cmd.exe",
        )
        .unwrap();
        assert_eq!(
            bat,
            r#"C:\Windows\System32\cmd.exe /d /v:off /s /c ""C:\npm\omp.CMD" "--resume" "C:\a b\\" "x&y"""#
        );
        assert_eq!(
            command_line(
                Path::new(r"C:\npm\omp.cmd"),
                &["--session".into(), r"C:\work\!name!\session.jsonl".into()],
                "cmd.exe",
            )
            .unwrap(),
            r#"cmd.exe /d /v:off /s /c ""C:\npm\omp.cmd" "--session" "C:\work\!name!\session.jsonl"""#
        );
        assert!(matches!(
            command_line(Path::new("x.bat"), &["100%".into()], "cmd.exe"),
            Err(Error::UnsafeBatchArgument(_))
        ));
    }

    #[test]
    fn program_resolution_uses_path_and_pathext() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("omp.cmd"), b"").unwrap();
        std::fs::write(dir.path().join("tool.exe"), b"").unwrap();
        let path = format!("{};{}", "/nonexistent", dir.path().display());
        assert_eq!(
            resolve_program(Path::new("omp"), Some(&path), Some(".EXE;.CMD")),
            dir.path().join("omp.cmd")
        );
        assert_eq!(
            resolve_program(Path::new("tool.exe"), Some(&path), None),
            dir.path().join("tool.exe")
        );
        assert_eq!(
            resolve_program(Path::new("missing"), Some(&path), None),
            PathBuf::from("missing")
        );
        assert!(is_batch(Path::new("x.BAT")) && !is_batch(Path::new("x.exe")));
    }
}
