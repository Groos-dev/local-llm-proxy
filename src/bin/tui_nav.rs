use cliclack::{StringCursor, Theme, ThemeState};
use console::{Key, Term};
use std::{fmt::Display, io::Write};

pub enum Flow<T> {
    Value(T),
    Back,
    Exit,
}

struct QuietTheme;

impl Theme for QuietTheme {
    fn format_footer_with_message(&self, state: &ThemeState, message: &str) -> String {
        let line = match state {
            ThemeState::Active if message.is_empty() => "└".to_string(),
            ThemeState::Active => format!("└  {message}"),
            ThemeState::Cancel => "└".to_string(),
            ThemeState::Submit => "│".to_string(),
            ThemeState::Error(err) => format!("└  {err}"),
        };
        format!("{}\n", self.bar_color(state).apply_to(line))
    }
}

pub struct Menu<T> {
    prompt: String,
    items: Vec<(T, String, String)>,
    initial: Option<T>,
}

impl<T: Clone> Menu<T> {
    pub fn new(prompt: impl Display) -> Self {
        Self {
            prompt: prompt.to_string(),
            items: Vec::new(),
            initial: None,
        }
    }

    pub fn item(mut self, value: T, label: impl Display, hint: impl Display) -> Self {
        self.items
            .push((value, label.to_string(), hint.to_string()));
        self
    }

    pub fn initial_value(mut self, value: T) -> Self {
        self.initial = Some(value);
        self
    }

    pub fn interact(self) -> Result<Flow<T>, String>
    where
        T: Eq,
    {
        if self.items.is_empty() {
            return Err("No items added to the list".into());
        }
        let theme = QuietTheme;
        let mut term = Term::stderr();
        let mut cursor = self
            .initial
            .as_ref()
            .and_then(|want| self.items.iter().position(|(value, _, _)| value == want))
            .unwrap_or(0);
        let mut prev_lines = 0usize;
        term.hide_cursor().map_err(|e| e.to_string())?;
        let result = loop {
            let state = ThemeState::Active;
            let mut frame = theme.format_header(&state, &self.prompt);
            for (i, (_, label, hint)) in self.items.iter().enumerate() {
                frame.push_str(&theme.format_select_item(&state, i == cursor, label, hint));
            }
            frame.push_str(&theme.format_footer(&state));
            if let Err(err) = draw_frame(&mut term, &frame, &mut prev_lines) {
                break Err(err);
            }
            match term.read_key_raw() {
                Ok(Key::Escape) => break Ok(Flow::Back),
                Ok(Key::CtrlC) => break Ok(Flow::Exit),
                Ok(Key::Enter) => break Ok(Flow::Value(self.items[cursor].0.clone())),
                Ok(Key::ArrowUp | Key::ArrowLeft | Key::Char('k') | Key::Char('h')) => {
                    if cursor > 0 {
                        cursor -= 1;
                    }
                }
                Ok(Key::ArrowDown | Key::ArrowRight | Key::Char('j') | Key::Char('l')) => {
                    if cursor + 1 < self.items.len() {
                        cursor += 1;
                    }
                }
                Ok(_) => {}
                Err(err) => break Err(err.to_string()),
            }
        };
        let _ = term.show_cursor();
        result
    }
}

pub fn input_flow(
    prompt: impl Display,
    default: &str,
    validate: impl Fn(&String) -> Result<(), String>,
) -> Result<Flow<String>, String> {
    let theme = QuietTheme;
    let prompt = prompt.to_string();
    let mut term = Term::stderr();
    let mut value = StringCursor::default();
    let mut placeholder = StringCursor::default();
    placeholder.extend(default);
    let mut error: Option<String> = None;
    let mut prev_lines = 0usize;
    term.hide_cursor().map_err(|e| e.to_string())?;
    let result = loop {
        let state = match &error {
            Some(err) => ThemeState::Error(err.clone()),
            None => ThemeState::Active,
        };
        let mut frame = theme.format_header(&state, &prompt);
        if value.is_empty() && !default.is_empty() {
            frame.push_str(&theme.format_placeholder(&state, &placeholder));
        } else {
            frame.push_str(&theme.format_input(&state, &value));
        }
        frame.push_str(&theme.format_footer(&state));
        if let Err(err) = draw_frame(&mut term, &frame, &mut prev_lines) {
            break Err(err);
        }
        match term.read_key_raw() {
            Ok(Key::Escape) => break Ok(Flow::Back),
            Ok(Key::CtrlC) => break Ok(Flow::Exit),
            Ok(Key::Enter) => {
                let mut submitted = value.to_string();
                if submitted.is_empty() {
                    submitted = default.to_string();
                }
                match validate(&submitted) {
                    Ok(()) => break Ok(Flow::Value(submitted)),
                    Err(err) => error = Some(err),
                }
            }
            Ok(Key::Char(ch)) if !ch.is_ascii_control() => {
                error = None;
                value.insert(ch);
            }
            Ok(Key::Backspace) => {
                error = None;
                value.delete_left();
            }
            Ok(Key::Del) => {
                error = None;
                value.delete_right();
            }
            Ok(Key::ArrowLeft) => value.move_left(),
            Ok(Key::ArrowRight) => value.move_right(),
            Ok(Key::Home) => value.move_home(),
            Ok(Key::End) => value.move_end(),
            Ok(_) => {}
            Err(err) => break Err(err.to_string()),
        }
    };
    let _ = term.show_cursor();
    result
}

fn draw_frame(term: &mut Term, frame: &str, prev_lines: &mut usize) -> Result<(), String> {
    if *prev_lines > 0 {
        term.clear_last_lines(*prev_lines)
            .map_err(|e| e.to_string())?;
    }
    term.write_all(frame.as_bytes())
        .map_err(|e| e.to_string())?;
    term.flush().map_err(|e| e.to_string())?;
    *prev_lines = frame.lines().count();
    Ok(())
}
