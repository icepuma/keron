//! Interactive yes/no prompt shown before `--execute` applies a plan.
//! Mirrors `OpenTofu`: `Only 'yes' will be accepted to approve.` so a
//! quick `y` or accidental Enter cannot trigger an apply.

use std::io::{self, BufRead, Write};

pub fn prompt_yes_no<R: BufRead, W: Write>(stdin: &mut R, stdout: &mut W) -> io::Result<bool> {
    writeln!(stdout)?;
    writeln!(stdout, "Do you want to perform these actions?")?;
    writeln!(stdout, "  Only 'yes' will be accepted to approve.")?;
    writeln!(stdout)?;
    write!(stdout, "  Enter a value: ")?;
    stdout.flush()?;
    let line = read_line_or_eof(stdin, "approval prompt")?;
    Ok(line.trim() == "yes")
}

pub fn prompt_force<R: BufRead, W: Write>(
    stdin: &mut R,
    stdout: &mut W,
    count: usize,
) -> io::Result<bool> {
    writeln!(stdout)?;
    writeln!(
        stdout,
        "{count} change(s) would overwrite or remove existing filesystem objects that keron cannot prove it owns."
    )?;
    writeln!(stdout, "  Only 'force' will be accepted to continue.")?;
    writeln!(stdout)?;
    write!(stdout, "  Enter a value: ")?;
    stdout.flush()?;
    let line = read_line_or_eof(stdin, "force prompt")?;
    Ok(line.trim() == "force")
}

pub fn prompt_precheck_continue<R: BufRead, W: Write>(
    stdin: &mut R,
    stdout: &mut W,
) -> io::Result<bool> {
    writeln!(stdout)?;
    writeln!(stdout, "Do you still want to proceed?")?;
    writeln!(stdout, "  Only 'yes' will be accepted to continue.")?;
    writeln!(stdout)?;
    write!(stdout, "  Enter a value: ")?;
    stdout.flush()?;
    let line = read_line_or_eof(stdin, "precheck prompt")?;
    Ok(line.trim() == "yes")
}

/// Read a line from `stdin` or surface a clear EOF error.
///
/// `BufRead::read_line` returns `Ok(0)` when the stream ends before
/// any byte is read — that's `</dev/null` and `</dev/closed`-fd. We
/// treat that as a hard error so a user staring at "Apply cancelled."
/// can tell whether they answered the prompt or whether keron never
/// got a chance to ask. Piped `echo yes | keron apply` still works
/// because at least one byte reaches us before EOF.
fn read_line_or_eof<R: BufRead>(stdin: &mut R, label: &str) -> io::Result<String> {
    let mut line = String::new();
    let n = stdin.read_line(&mut line)?;
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "{label}: stdin closed before any input was read (interactive answer required; pipe `yes` / `force` for non-interactive runs)"
            ),
        ));
    }
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(input: &str) -> bool {
        let mut sin = io::Cursor::new(input.as_bytes());
        let mut sout = Vec::new();
        prompt_yes_no(&mut sin, &mut sout).unwrap()
    }

    #[test]
    fn yes_approves() {
        assert!(ask("yes\n"));
    }

    #[test]
    fn yes_with_trailing_whitespace_approves() {
        assert!(ask("  yes  \n"));
    }

    #[test]
    fn capitalised_yes_does_not_approve() {
        assert!(!ask("YES\n"));
    }

    #[test]
    fn short_y_does_not_approve() {
        assert!(!ask("y\n"));
    }

    #[test]
    fn no_does_not_approve() {
        assert!(!ask("no\n"));
    }

    #[test]
    fn empty_input_surfaces_eof_error() {
        let mut sin = io::Cursor::new(b"".to_vec());
        let mut sout = Vec::new();
        let err = prompt_yes_no(&mut sin, &mut sout).expect_err("EOF should error");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        let msg = format!("{err}");
        assert!(msg.contains("stdin closed"), "got: {msg}");
    }

    #[test]
    fn yes_without_trailing_newline_still_approves() {
        let mut sin = io::Cursor::new(b"yes".to_vec());
        let mut sout = Vec::new();
        assert!(prompt_yes_no(&mut sin, &mut sout).unwrap());
    }

    #[test]
    fn force_requires_literal_force() {
        let mut sin = io::Cursor::new(b"force\n".to_vec());
        let mut sout = Vec::new();
        assert!(prompt_force(&mut sin, &mut sout, 1).unwrap());

        let mut sin = io::Cursor::new(b"yes\n".to_vec());
        let mut sout = Vec::new();
        assert!(!prompt_force(&mut sin, &mut sout, 1).unwrap());
    }

    #[test]
    fn precheck_continue_requires_literal_yes() {
        let mut sin = io::Cursor::new(b"yes\n".to_vec());
        let mut sout = Vec::new();
        assert!(prompt_precheck_continue(&mut sin, &mut sout).unwrap());

        let mut sin = io::Cursor::new(b"y\n".to_vec());
        let mut sout = Vec::new();
        assert!(!prompt_precheck_continue(&mut sin, &mut sout).unwrap());
    }
}
