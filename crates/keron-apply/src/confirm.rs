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
    let mut line = String::new();
    stdin.read_line(&mut line)?;
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
    let mut line = String::new();
    stdin.read_line(&mut line)?;
    Ok(line.trim() == "force")
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
    fn empty_input_does_not_approve() {
        assert!(!ask(""));
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
}
