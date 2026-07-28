use thiserror::Error;
use types::{SkillCommand, SkillGhostCommand, SkillZigZagZigCommand};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SkillCommandError {
    #[error("skill command is empty")]
    EmptyCommand,
    #[error("unknown skill command: {0}")]
    UnknownCommand(String),
    #[error("unknown skill action: {0}")]
    UnknownAction(String),
    #[error("skill command has trailing tokens")]
    TrailingTokens,
}

pub fn parse_skill_command(input: &str) -> Result<SkillCommand, SkillCommandError> {
    let tokens: Vec<_> = input.split_ascii_whitespace().collect();
    let Some(alias) = tokens.first() else {
        return Err(SkillCommandError::EmptyCommand);
    };
    if tokens.len() > 2 {
        return Err(SkillCommandError::TrailingTokens);
    }
    let action = tokens.get(1).copied();
    match (*alias, action) {
        ("/ghost", None | Some("on")) => Ok(SkillCommand::Ghost(SkillGhostCommand::On)),
        ("/ghost", Some("off")) => Ok(SkillCommand::Ghost(SkillGhostCommand::Off)),
        ("/ghost", Some("status")) => Ok(SkillCommand::Ghost(SkillGhostCommand::Status)),
        ("/zigzagzig", None | Some("run")) => {
            Ok(SkillCommand::ZigZagZig(SkillZigZagZigCommand::Run))
        }
        ("/zigzagzig", Some("status")) => {
            Ok(SkillCommand::ZigZagZig(SkillZigZagZigCommand::Status))
        }
        ("/zigzagzig", Some("stop")) => Ok(SkillCommand::ZigZagZig(SkillZigZagZigCommand::Stop)),
        ("/ghost" | "/zigzagzig", Some(action)) => {
            Err(SkillCommandError::UnknownAction(action.into()))
        }
        (alias, _) => Err(SkillCommandError::UnknownCommand(alias.into())),
    }
}
