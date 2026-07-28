use skill_runtime::{
    parse_skill_command, SkillCommand, SkillCommandError, SkillGhostCommand, SkillZigZagZigCommand,
};

#[test]
fn aliases_parse_exactly_and_unknown_commands_fail_closed() {
    assert_eq!(
        parse_skill_command("/ghost"),
        Ok(SkillCommand::Ghost(SkillGhostCommand::On))
    );
    assert_eq!(
        parse_skill_command(" /ghost\tstatus "),
        Ok(SkillCommand::Ghost(SkillGhostCommand::Status))
    );
    assert_eq!(
        parse_skill_command("/zigzagzig stop"),
        Ok(SkillCommand::ZigZagZig(SkillZigZagZigCommand::Stop))
    );
    assert!(matches!(
        parse_skill_command("/ghost maybe"),
        Err(SkillCommandError::UnknownAction(_))
    ));
    assert!(matches!(
        parse_skill_command("/ghost on now"),
        Err(SkillCommandError::TrailingTokens)
    ));
    assert!(matches!(
        parse_skill_command("ghost on"),
        Err(SkillCommandError::UnknownCommand(_))
    ));
}
