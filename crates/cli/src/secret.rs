//! The cloud shared key comes from stdin: piped as-is (one trailing newline dropped), or typed at a
//! hidden `Shared key: ` prompt.

use std::io::{IsTerminal, Read, Write};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use crate::error::{CliError, Result};

pub fn read() -> Result<String> {
    let failed = |error: std::io::Error| CliError::new(error.to_string(), "INTERNAL_ERROR", 1);
    let mut stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut bytes = Vec::new();
        stdin.read_to_end(&mut bytes).map_err(failed)?;
        let value = String::from_utf8_lossy(&bytes);
        let value = value
            .strip_suffix('\n')
            .map_or(&*value, |value| value.strip_suffix('\r').unwrap_or(value));
        return Ok(value.to_owned());
    }

    let mut stderr = std::io::stderr();
    let _ = write!(stderr, "Shared key: ").and_then(|()| stderr.flush());
    crossterm::terminal::enable_raw_mode().map_err(failed)?;
    let typed = read_hidden();
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = writeln!(stderr);
    typed
        .map_err(failed)?
        .ok_or_else(|| CliError::new("Cloud login canceled", "CANCELED", 2))
}

/// `None` when the prompt is canceled with Ctrl-C.
fn read_hidden() -> std::io::Result<Option<String>> {
    let mut value = String::new();
    loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => return Ok(Some(value)),
            KeyCode::Char('j') if control => return Ok(Some(value)),
            KeyCode::Char('c') if control => return Ok(None),
            KeyCode::Backspace => {
                value.pop();
            }
            KeyCode::Char(character) if !control => value.push(character),
            _ => {}
        }
    }
}
