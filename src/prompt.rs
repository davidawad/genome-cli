//! User interaction behind a trait, so first-run questions and SSH passphrase
//! prompts can be driven by tests (no TTY assumptions).

use std::io::{BufRead, IsTerminal, Write};

use zeroize::Zeroizing;

pub trait Prompter {
    /// Can we ask questions (stdin and stderr are terminals)?
    fn interactive(&self) -> bool;
    /// Print a message to the user (stderr).
    fn say(&self, text: &str);
    /// Ask a yes/no question; `default` on empty input or when not interactive.
    fn confirm(&self, question: &str, default: bool) -> bool;
    /// Read a secret without echo; `None` when not interactive or cancelled.
    fn secret(&self, prompt: &str) -> Option<Zeroizing<String>>;
}

/// The real terminal.
pub struct Tty;

impl Prompter for Tty {
    fn interactive(&self) -> bool {
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }

    fn say(&self, text: &str) {
        eprintln!("{text}");
    }

    fn confirm(&self, question: &str, default: bool) -> bool {
        if !self.interactive() {
            return default;
        }
        eprint!("{question} [{}] ", if default { "Y/n" } else { "y/N" });
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).is_err() {
            return default;
        }
        answer(&line, default)
    }

    fn secret(&self, prompt: &str) -> Option<Zeroizing<String>> {
        if !self.interactive() {
            return None;
        }
        rpassword::prompt_password(prompt).ok().map(Zeroizing::new)
    }
}

/// Interpret a yes/no answer line.
pub fn answer(line: &str, default: bool) -> bool {
    match line.trim().to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    }
}

/// Scripted answers and a transcript, for tests.
#[cfg(test)]
pub struct Scripted {
    pub interactive: bool,
    pub answers: std::cell::RefCell<std::collections::VecDeque<String>>,
    pub transcript: std::cell::RefCell<String>,
}

#[cfg(test)]
impl Scripted {
    pub fn new(interactive: bool, answers: &[&str]) -> Self {
        Self {
            interactive,
            answers: std::cell::RefCell::new(answers.iter().map(|s| s.to_string()).collect()),
            transcript: std::cell::RefCell::new(String::new()),
        }
    }

    fn next(&self) -> Option<String> {
        self.answers.borrow_mut().pop_front()
    }
}

#[cfg(test)]
impl Prompter for Scripted {
    fn interactive(&self) -> bool {
        self.interactive
    }

    fn say(&self, text: &str) {
        self.transcript.borrow_mut().push_str(text);
        self.transcript.borrow_mut().push('\n');
    }

    fn confirm(&self, question: &str, default: bool) -> bool {
        self.say(question);
        if !self.interactive {
            return default;
        }
        self.next().map_or(default, |a| answer(&a, default))
    }

    fn secret(&self, prompt: &str) -> Option<Zeroizing<String>> {
        self.say(prompt);
        if !self.interactive {
            return None;
        }
        self.next().map(Zeroizing::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers() {
        assert!(answer("\n", true) && answer("Y\r\n", false) && answer("yes", false));
        assert!(!answer("n", true) && !answer("nope", true) && !answer("", false));
    }
}
