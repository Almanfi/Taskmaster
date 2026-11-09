use crate::validators::{
    Command, EnvironmentVariables, ExitCodes, ExitSignals, ProcessCount, RestartPolicy, Stderr,
    Stdout, Umask, WorkingDir,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;

/// How one program's config relates to another after a reload parse.
/// Maps 1:1 to the diff actions Keep / Scale / Replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramDelta {
    Unchanged,
    ReplicasOnly,
    Changed,
}

#[derive(Debug)]
pub enum ConfigValidationError {
    NoCommandGiven,
    ConfigInvalid(String),
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct Config {
    pub program: HashMap<String, ProgramConfig>,
}

impl fmt::Display for ConfigValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCommandGiven => write!(f, "No valid Command was given"),
            Self::ConfigInvalid(reason) => write!(f, "Config validation error: {reason}"),
        }
    }
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct ProgramConfig {
    pub command: Command,
    pub chdir: WorkingDir,
    pub umask: Option<Umask>,
    pub auto_start: bool,                //
    pub restart_protocol: RestartPolicy, //
    pub exit_codes: ExitCodes,           //
    pub runtime: u32,                    //
    pub restart_count: u32,              //
    pub exit_signal: ExitSignals,        //
    pub kill_time: u32,                  //
    pub stdout: Option<Stdout>,
    pub stderr: Option<Stderr>,
    pub env: Option<EnvironmentVariables>,
    pub process_count: ProcessCount, //
}

impl ProgramConfig {
    pub fn delta(&self, other: &Self) -> ProgramDelta {
        if !self.identity_matches(other) {
            return ProgramDelta::Changed;
        }
        if self.process_count != other.process_count {
            return ProgramDelta::ReplicasOnly;
        }
        ProgramDelta::Unchanged
    }

    /// Check if two program configs are identical in all aspects.
    fn identity_matches(&self, other: &Self) -> bool {
        self.command == other.command
            && self.chdir == other.chdir
            && self.umask == other.umask
            && self.auto_start == other.auto_start
            && self.restart_protocol == other.restart_protocol
            && self.exit_codes == other.exit_codes
            && self.runtime == other.runtime
            && self.restart_count == other.restart_count
            && self.exit_signal == other.exit_signal
            && self.kill_time == other.kill_time
            && self.stdout == other.stdout
            && self.stderr == other.stderr
            && self.env == other.env
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validators::{
        Command, EnvironmentVariables, ExitCodes, ExitSignals, ProcessCount, RestartPolicy, Umask,
        WorkingDir,
    };
    use std::collections::HashMap;

    fn sample_program_config() -> ProgramConfig {
        ProgramConfig {
            command: Command::try_from("/bin/sh".to_string()).expect("sh must exist"),
            chdir: WorkingDir::try_from("/".to_string()).expect("/ exists"),
            umask: Some(Umask::try_from("022".to_string()).expect("valid umask")),
            process_count: ProcessCount::try_from(1).expect("1 is valid"),
            auto_start: true,
            restart_protocol: RestartPolicy::Never,
            exit_codes: ExitCodes::try_from(vec![0]).expect("valid code"),
            runtime: 0,
            restart_count: 0,
            exit_signal: ExitSignals::Term,
            kill_time: 0,
            stdout: None,
            stderr: None,
            env: Some(EnvironmentVariables::try_from(HashMap::new()).expect("empty env")),
        }
    }

    #[test]
    fn program_config_equals_its_clone() {
        let a = sample_program_config();
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn two_independently_built_program_configs_are_equal() {
        assert_eq!(sample_program_config(), sample_program_config());
    }

    #[test]
    fn differing_field_breaks_equality() {
        let a = sample_program_config();
        let mut b = sample_program_config();
        b.auto_start = false;
        assert_ne!(a, b);
    }


    #[test]
    fn delta_is_unchanged_for_identical_configs() {
        let a = sample_program_config();
        let b = sample_program_config();
        assert_eq!(a.delta(&b), ProgramDelta::Unchanged);
    }

    #[test]
    fn delta_is_replicas_only_when_only_process_count_differs() {
        let a = sample_program_config();
        let mut b = sample_program_config();
        b.process_count = ProcessCount::try_from(5).expect("5 is valid");
        assert_eq!(a.delta(&b), ProgramDelta::ReplicasOnly);
    }

    #[test]
    fn delta_is_changed_when_any_other_field_differs() {
        let a = sample_program_config();
        let mut b = sample_program_config();
        b.auto_start = !b.auto_start;
        assert_eq!(a.delta(&b), ProgramDelta::Changed);
    }

    #[test]
    fn delta_is_changed_even_when_process_count_also_differs() {
        // Edge case: both identity and replicas changed → still Changed.
        let a = sample_program_config();
        let mut b = sample_program_config();
        b.auto_start    = !b.auto_start;
        b.process_count = ProcessCount::try_from(9).expect("9 is valid");
        assert_eq!(a.delta(&b), ProgramDelta::Changed);
    }
}
