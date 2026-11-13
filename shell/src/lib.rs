#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellCommand {
    Status,
    Start(String),
    Stop(String),
    Restart(String),
    Reload,
    Quit
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    Empty,
    Unknown(String),
    MissingName(&'static str),
    ExtraArgs
}

pub fn parse(line: &str) -> Result<ShellCommand, ParseError> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(ParseError::Empty);
    }

    let mut parts = trimmed.split_whitespace();
    let cmd = parts.next().expect("non empty after trim");
    let result = match cmd {
        "status"=> ShellCommand::Status,
        "start"=> ShellCommand::Start(parts.next().ok_or(ParseError::MissingName("start"))?.into()),
        "stop"=> ShellCommand::Stop(parts.next().ok_or(ParseError::MissingName("stop"))?.into()),
        "restart"=> ShellCommand::Restart(parts.next().ok_or(ParseError::MissingName("restart"))?.into()),
        "reload"=> ShellCommand::Reload,
        "quit" | "exit" => ShellCommand::Quit,
        _ => return Err(ParseError::Unknown(cmd.into()))
    };
    
    if parts.next().is_some() {
        return Err(ParseError::ExtraArgs);
    }
    Ok(result)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_commands() {
        assert_eq!(parse("status").unwrap(), ShellCommand::Status);
        assert_eq!(parse("reload").unwrap(), ShellCommand::Reload);
        assert_eq!(parse("quit"  ).unwrap(), ShellCommand::Quit);
        assert_eq!(parse("exit"  ).unwrap(), ShellCommand::Quit);
    }

    #[test]
    fn parses_commands_with_name() {
        assert_eq!(parse("start web").unwrap(),  ShellCommand::Start  ("web".into()));
        assert_eq!(parse("stop db" ).unwrap(),   ShellCommand::Stop   ("db".into()));
        assert_eq!(parse("restart x").unwrap(),  ShellCommand::Restart("x".into()));
    }

    #[test]
    fn tolerates_whitespace() {
        assert_eq!(parse("   start   web   ").unwrap(),
                   ShellCommand::Start("web".into()));
    }

    #[test]
    fn rejects_empty_and_whitespace() {
        assert_eq!(parse(""     ), Err(ParseError::Empty));
        assert_eq!(parse("   \t"), Err(ParseError::Empty));
    }

    #[test]
    fn rejects_unknown_command() {
        assert_eq!(parse("deploy foo"), Err(ParseError::Unknown("deploy".into())));
    }

    #[test]
    fn rejects_missing_name() {
        assert_eq!(parse("start"  ), Err(ParseError::MissingName("start"  )));
        assert_eq!(parse("stop"   ), Err(ParseError::MissingName("stop"   )));
        assert_eq!(parse("restart"), Err(ParseError::MissingName("restart")));
    }

    #[test]
    fn rejects_extra_arguments() {
        assert_eq!(parse("status x"       ), Err(ParseError::ExtraArgs));
        assert_eq!(parse("start x y"      ), Err(ParseError::ExtraArgs));
    }
}