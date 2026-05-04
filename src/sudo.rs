// SPDX-FileCopyrightText: 2026 deploy-rx contributors
//
// SPDX-License-Identifier: MPL-2.0

use serde::{de, Deserialize, Deserializer, Serialize};
use std::fmt;
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum SudoParseError {
    #[error("sudo command must not be empty")]
    Empty,
    #[error("sudo argv must end with `-u` or `--user` so deploy-rx can append the target user")]
    MissingUserFlag,
    #[error("legacy sudo string contains unterminated quote; use structured sudo = [\"program\", \"arg\", ...] instead")]
    UnterminatedQuote,
    #[error("legacy sudo string ends with a dangling escape; use structured sudo = [\"program\", \"arg\", ...] instead")]
    DanglingEscape,
    #[error("legacy sudo string contains shell syntax `{0}`; use structured sudo = [\"program\", \"arg\", ...] instead")]
    ComplexSyntax(char),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SudoCommand {
    argv: Vec<String>,
}

impl SudoCommand {
    pub fn new(argv: Vec<String>) -> Result<Self, SudoParseError> {
        if argv.is_empty() {
            return Err(SudoParseError::Empty);
        }

        let command = Self { argv };
        command.validate()?;
        Ok(command)
    }

    pub fn default_sudo() -> Self {
        Self {
            argv: vec!["sudo".to_string(), "-u".to_string()],
        }
    }

    pub fn parse_legacy(input: &str) -> Result<Self, SudoParseError> {
        Self::new(split_legacy_sudo(input)?)
    }

    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    fn sudo_index(&self) -> Option<usize> {
        self.argv.iter().position(|program| {
            Path::new(program)
                .file_name()
                .map(|program| program == "sudo")
                .unwrap_or(false)
        })
    }

    pub fn is_sudo(&self) -> bool {
        self.sudo_index().is_some()
    }

    fn validate(&self) -> Result<(), SudoParseError> {
        if self.is_sudo() && !matches!(self.argv.last().map(String::as_str), Some("-u" | "--user"))
        {
            return Err(SudoParseError::MissingUserFlag);
        }

        Ok(())
    }

    pub fn argv_for_user(&self, user: &str, interactive: bool) -> Vec<String> {
        let mut argv = self.argv.clone();

        if interactive {
            if let Some(sudo_index) = self.sudo_index() {
                let mut insert_at = sudo_index + 1;

                let has_stdin_flag = argv
                    .iter()
                    .skip(sudo_index + 1)
                    .any(|arg| arg == "-S" || arg == "--stdin");
                if !has_stdin_flag {
                    argv.insert(insert_at, "-S".to_string());
                    insert_at += 1;
                }

                let has_prompt_flag = argv
                    .iter()
                    .skip(sudo_index + 1)
                    .any(|arg| arg == "-p" || arg.starts_with("--prompt"));
                if !has_prompt_flag {
                    argv.insert(insert_at, "-p".to_string());
                    argv.insert(insert_at + 1, String::new());
                }
            }
        }

        argv.push(user.to_string());
        argv
    }
}

impl Serialize for SudoCommand {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.argv.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SudoCommand {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SudoCommandVisitor;

        impl<'de> de::Visitor<'de> for SudoCommandVisitor {
            type Value = SudoCommand;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a sudo argv array or a legacy sudo string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                SudoCommand::parse_legacy(value).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_str(&value)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                let mut argv = Vec::new();

                while let Some(arg) = seq.next_element::<String>()? {
                    argv.push(arg);
                }

                SudoCommand::new(argv).map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_any(SudoCommandVisitor)
    }
}

fn is_complex_shell_syntax(ch: char) -> bool {
    matches!(
        ch,
        '|' | '&' | ';' | '<' | '>' | '(' | ')' | '$' | '`' | '\n' | '\r'
    )
}

fn split_legacy_sudo(input: &str) -> Result<Vec<String>, SudoParseError> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut token_started = false;

    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
                token_started = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                token_started = true;
            }
            '\\' if !in_single => {
                let escaped = chars.next().ok_or(SudoParseError::DanglingEscape)?;
                current.push(escaped);
                token_started = true;
            }
            ch if ch.is_whitespace() && !in_single && !in_double => {
                if token_started {
                    tokens.push(current);
                    current = String::new();
                    token_started = false;
                }
            }
            ch if !in_single && !in_double && is_complex_shell_syntax(ch) => {
                return Err(SudoParseError::ComplexSyntax(ch));
            }
            ch => {
                current.push(ch);
                token_started = true;
            }
        }
    }

    if in_single || in_double {
        return Err(SudoParseError::UnterminatedQuote);
    }

    if token_started {
        tokens.push(current);
    }

    if tokens.is_empty() {
        return Err(SudoParseError::Empty);
    }

    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_legacy_sudo_string() {
        let sudo = SudoCommand::parse_legacy("sudo -u").unwrap();
        assert_eq!(sudo.argv(), &["sudo".to_string(), "-u".to_string()]);
    }

    #[test]
    fn parses_legacy_quotes() {
        let sudo = SudoCommand::parse_legacy("sudo -S -p \"\" -u").unwrap();
        assert_eq!(
            sudo.argv(),
            &[
                "sudo".to_string(),
                "-S".to_string(),
                "-p".to_string(),
                String::new(),
                "-u".to_string(),
            ]
        );
    }

    #[test]
    fn rejects_shell_syntax() {
        assert_eq!(
            SudoCommand::parse_legacy("sudo -u root; rm -rf /").unwrap_err(),
            SudoParseError::ComplexSyntax(';')
        );
    }

    #[test]
    fn deserializes_structured_argv() {
        let sudo: SudoCommand = serde_json::from_str(r#"["doas","-u"]"#).unwrap();
        assert_eq!(sudo.argv(), &["doas".to_string(), "-u".to_string()]);
    }

    #[test]
    fn serializes_as_argv_array() {
        let sudo = SudoCommand::new(vec!["sudo".to_string(), "-u".to_string()]).unwrap();
        assert_eq!(serde_json::to_string(&sudo).unwrap(), r#"["sudo","-u"]"#);
    }

    #[test]
    fn rejects_bare_sudo_command() {
        assert_eq!(
            SudoCommand::new(vec!["sudo".to_string()]).unwrap_err(),
            SudoParseError::MissingUserFlag
        );
    }

    #[test]
    fn rejects_sudo_when_user_slot_is_not_last() {
        assert_eq!(
            SudoCommand::new(vec![
                "env".to_string(),
                "sudo".to_string(),
                "-u".to_string(),
                "-H".to_string(),
            ])
            .unwrap_err(),
            SudoParseError::MissingUserFlag
        );
    }

    #[test]
    fn treats_full_path_sudo_as_sudo_for_interactive_mode() {
        let sudo =
            SudoCommand::new(vec!["/run/wrappers/bin/sudo".to_string(), "-u".to_string()]).unwrap();

        assert_eq!(
            sudo.argv_for_user("root", true),
            vec![
                "/run/wrappers/bin/sudo".to_string(),
                "-S".to_string(),
                "-p".to_string(),
                String::new(),
                "-u".to_string(),
                "root".to_string(),
            ]
        );
    }

    #[test]
    fn treats_wrapped_sudo_as_sudo_for_interactive_mode() {
        let sudo = SudoCommand::new(vec![
            "env".to_string(),
            "sudo".to_string(),
            "-u".to_string(),
        ])
        .unwrap();

        assert_eq!(
            sudo.argv_for_user("root", true),
            vec![
                "env".to_string(),
                "sudo".to_string(),
                "-S".to_string(),
                "-p".to_string(),
                String::new(),
                "-u".to_string(),
                "root".to_string(),
            ]
        );
    }
}
