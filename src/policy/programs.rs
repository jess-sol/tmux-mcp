//! awk and sed programs: whether one stays plain text processing. Each test
//! is strict — it may refuse a program that is plain, never pass one that
//! runs a command or opens a file of its own.

// --- Public API ---

/// True when an awk program can run no command and open no file: no
/// `system`, `getline`, `|` (pipes and `||` alike), `>` (redirects and
/// comparisons alike) or `@` (gawk's `@include`, `@load` and indirect calls).
pub fn awk_is_plain(program: &str) -> bool {
    !["system", "getline", "|", ">", "@"].iter().any(|s| program.contains(s))
}

/// True when a GNU sed script holds only commands that touch no file and
/// run nothing: no `e`, `r`, `R`, `w`, `W`, no `s///e` or `s///w`, and
/// nothing this reader does not know.
pub fn sed_is_plain(script: &str) -> bool {
    Sed { chars: script.chars().collect(), at: 0 }.plain().is_some()
}

// --- sed ---

struct Sed {
    chars: Vec<char>,
    at: usize,
}

impl Sed {
    /// Some when every command is plain; None at the first that is not.
    fn plain(&mut self) -> Option<()> {
        loop {
            self.skip(|c| c.is_whitespace() || c == ';');
            let Some(c) = self.peek() else { return Some(()) };
            if c == '#' {
                self.skip(|c| c != '\n');
                continue;
            }
            self.addresses()?;
            self.skip(|c| c == ' ' || c == '\t' || c == '!');
            if self.peek() == Some('{') {
                self.at += 1;
                continue;
            }
            self.command()?;
            self.skip(|c| c == ' ' || c == '\t');
            match self.peek() {
                None | Some(';' | '\n' | '}' | '#') => {}
                Some(_) => return None,
            }
        }
    }

    fn addresses(&mut self) -> Option<()> {
        if self.address()? && self.peek() == Some(',') {
            self.at += 1;
            if matches!(self.peek(), Some('+' | '~')) {
                self.at += 1;
                self.skip(|c| c.is_ascii_digit());
            } else if !self.address()? {
                return None;
            }
        }
        Some(())
    }

    /// Reads one address; Some(false) when there is none.
    fn address(&mut self) -> Option<bool> {
        match self.peek() {
            Some(c) if c.is_ascii_digit() => {
                self.skip(|c| c.is_ascii_digit());
                if self.peek() == Some('~') {
                    self.at += 1;
                    self.skip(|c| c.is_ascii_digit());
                }
            }
            Some('$') => self.at += 1,
            Some('/') => {
                self.at += 1;
                self.until('/')?;
                self.skip(|c| c == 'I' || c == 'M');
            }
            Some('\\') => {
                self.at += 1;
                let delimiter = self.next()?;
                self.until(delimiter)?;
                self.skip(|c| c == 'I' || c == 'M');
            }
            _ => return Some(false),
        }
        Some(true)
    }

    fn command(&mut self) -> Option<()> {
        match self.next()? {
            '}' | '=' | 'd' | 'D' | 'g' | 'G' | 'h' | 'H' | 'n' | 'N' | 'p' | 'P' | 'x' | 'z' | 'F' => {}
            'l' | 'q' | 'Q' | 'L' => {
                self.skip(|c| c == ' ' || c == '\t');
                self.skip(|c| c.is_ascii_digit());
            }
            // A label ends at the first `;`, blank or newline, the shortest
            // reading, so no command hides inside one.
            ':' | 'b' | 't' | 'T' => self.skip(|c| !c.is_whitespace() && c != ';' && c != '}'),
            's' => {
                let delimiter = self.delimiter()?;
                self.until(delimiter)?;
                self.until(delimiter)?;
                self.skip(|c| matches!(c, 'g' | 'p' | 'i' | 'I' | 'm' | 'M') || c.is_ascii_digit());
            }
            'y' => {
                let delimiter = self.delimiter()?;
                self.until(delimiter)?;
                self.until(delimiter)?;
            }
            _ => return None,
        }
        Some(())
    }

    fn delimiter(&mut self) -> Option<char> {
        self.next().filter(|&c| c != '\n' && c != '\\')
    }

    /// Moves past the next `delimiter` that no backslash escapes.
    fn until(&mut self, delimiter: char) -> Option<()> {
        loop {
            match self.next()? {
                '\\' => _ = self.next()?,
                c if c == delimiter => return Some(()),
                '\n' => return None,
                _ => {}
            }
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.at += 1;
        Some(c)
    }

    fn skip(&mut self, f: impl Fn(char) -> bool) {
        while self.peek().is_some_and(&f) {
            self.at += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_awk_program_is_plain_only_without_commands_and_files() {
        let rows = [
            ("{print $1}", true),
            ("NR > 1 {print}", false),
            ("BEGIN{system(\"id\")}", false),
            ("BEGIN{print \"id\" | \"sh\"}", false),
            ("{print > \"/tmp/x\"}", false),
            ("{getline line < \"/etc/shadow\"}", false),
            ("@include \"x\"", false),
        ];
        for (program, want) in rows {
            assert_eq!(awk_is_plain(program), want, "{program}");
        }
    }

    #[test]
    fn a_sed_script_is_plain_only_without_commands_and_files() {
        let rows = [
            ("s/a/b/", true),
            ("s/hello/world/g", true),
            ("s|a/b|c|2", true),
            ("s/a\\/b/c/", true),
            ("/x/p", true),
            ("1,5d", true),
            ("$!N; P; D", true),
            ("0,/re/s//x/", true),
            ("/a/,+2{s/x/y/;p}", true),
            ("y/abc/xyz/", true),
            (":a;N;ba", true),
            ("# comment\ns/a/b/", true),
            ("s/a/b/w /tmp/x", false),
            ("s/a/b/e", false),
            ("s/a/b/ge", false),
            ("e id", false),
            ("1e id", false),
            ("r /etc/shadow", false),
            ("R /etc/shadow", false),
            ("w /tmp/x", false),
            ("W /tmp/x", false),
            ("/x/w /tmp/x", false),
            ("s/a/b/;w /tmp/x", false),
            ("a text", false),
            (":a;w /tmp/x", false),
            ("s/[/]/x/", false),
            ("s/a/b", false),
            ("v", false),
        ];
        for (script, want) in rows {
            assert_eq!(sed_is_plain(script), want, "{script}");
        }
    }
}
