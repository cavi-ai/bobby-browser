use thiserror::Error;
use types::{SkillCommand, SkillFailure, SkillOutcome};

use crate::{
    parse_skill_command, SkillCommandError, SkillContext, SkillRegistry, SkillRegistryError,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCommandReceipt {
    pub alias: &'static str,
    pub skill_name: &'static str,
    pub skill_version: &'static str,
    pub outcome: SkillOutcome,
}

#[derive(Debug, Error)]
pub enum SkillCommandRouterError {
    #[error(transparent)]
    Command(#[from] SkillCommandError),
    #[error(transparent)]
    Registry(#[from] SkillRegistryError),
    #[error("skill execution failed: {0:?}")]
    Execution(SkillFailure),
}

pub struct SkillCommandRouter {
    registry: SkillRegistry,
}

impl SkillCommandRouter {
    pub fn new(registry: SkillRegistry) -> Self {
        Self { registry }
    }

    pub async fn execute(
        &self,
        input: &str,
        context: &SkillContext,
    ) -> Result<SkillCommandReceipt, SkillCommandRouterError> {
        let command = parse_skill_command(input)?;
        let alias = command_alias(&command);
        let skill = self.registry.resolve(alias, context)?;
        let receipt = SkillCommandReceipt {
            alias,
            skill_name: skill.name(),
            skill_version: skill.version(),
            outcome: self
                .registry
                .execute(command, context)
                .await
                .map_err(SkillCommandRouterError::Execution)?,
        };
        Ok(receipt)
    }
}

fn command_alias(command: &SkillCommand) -> &'static str {
    match command {
        SkillCommand::Ghost(_) => "/ghost",
        SkillCommand::ZigZagZig(_) => "/zigzagzig",
    }
}
