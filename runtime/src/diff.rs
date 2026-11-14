use config_parser::config::{Config, ProgramConfig, ProgramDelta};


#[derive(Debug)]
pub enum DiffAction {
    Keep    { name: String },
    Add     { name: String, cfg: ProgramConfig },
    Remove  { name: String },
    Scale   { name: String, old_count: usize, new_count: usize },
    Replace { name: String, old_cfg: ProgramConfig, new_cfg: ProgramConfig },
}

pub struct ConfigDiff {
    pub actions: Vec<DiffAction>,
}

impl ConfigDiff {
    pub fn compute(current: &Config, new: &Config) -> Self {
        let mut actions: Vec<DiffAction> = Vec::new();

        for (name, conf) in &current.program {
            match new.program.get(name) {
                Some(c) => {
                    match c.delta(conf) {
                        ProgramDelta::Unchanged => { actions.push(DiffAction::Keep { name: name.clone() })}
                        ProgramDelta::Changed => {
                            actions.push(DiffAction::Replace { 
                                name: name.clone(),
                                old_cfg: conf.clone(),
                                new_cfg: c.clone() })
                        }
                        ProgramDelta::ReplicasOnly => {
                            actions.push(DiffAction::Scale { 
                                name: name.clone(),
                                old_count: conf.process_count.value as usize,
                                new_count: c.process_count.value as usize });
                        }
                    }
                }
                None => {
                    actions.push(DiffAction::Remove { name: name.clone() })
                }
            }
        }
        
        for (name, conf) in &new.program {
            if !current.program.contains_key(name) {
                actions.push(DiffAction::Add {
                    name: name.clone(),
                    cfg: conf.clone()
                })
            }
        }

        Self { actions }
    }

    pub fn is_noop(&self) -> bool {
        self.actions.iter().all(|a| matches!(a, DiffAction::Keep { .. }))
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use config_parser::config::Config;
    use config_parser::validators::{
        Command, EnvironmentVariables, ExitCodes, ExitSignals,
        ProcessCount, RestartPolicy, Umask, WorkingDir,
    };
    use std::collections::HashMap;

    fn sample_cfg(count: u64) -> ProgramConfig {
        ProgramConfig {
            command:          Command::try_from("/bin/sh".to_string()).unwrap(),
            chdir:            WorkingDir::try_from("/".to_string()).unwrap(),
            umask:            Some(Umask::try_from("022".to_string()).unwrap()),
            auto_start:       true,
            restart_protocol: RestartPolicy::Never,
            exit_codes:       ExitCodes::try_from(vec![0]).unwrap(),
            runtime:          0,
            restart_count:    0,
            exit_signal:      ExitSignals::Term,
            kill_time:        0,
            stdout:           None,
            stderr:           None,
            env:              Some(EnvironmentVariables::try_from(HashMap::new()).unwrap()),
            process_count:    ProcessCount::try_from(count).unwrap(),
        }
    }

    fn one_program(name: &str, cfg: ProgramConfig) -> Config {
        let mut c = Config { program: HashMap::new() };
        c.program.insert(name.into(), cfg);
        c
    }

    #[test]
    fn identical_configs_emit_only_keep() {
        let a = one_program("web", sample_cfg(2));
        let b = one_program("web", sample_cfg(2));

        let diff = ConfigDiff::compute(&a, &b);

        assert!(diff.is_noop());
        assert_eq!(diff.actions.len(), 1);
        assert!(matches!(diff.actions[0], DiffAction::Keep { ref name } if name == "web"));
    }

    #[test]
    fn only_process_count_difference_emits_scale() {
        let a = one_program("web", sample_cfg(2));
        let b = one_program("web", sample_cfg(5));

        let diff = ConfigDiff::compute(&a, &b);

        assert_eq!(diff.actions.len(), 1);
        assert!(matches!(
            diff.actions[0],
            DiffAction::Scale { ref name, old_count: 2, new_count: 5 } if name == "web"
        ));
    }

    #[test]
    fn other_field_difference_emits_replace() {
        let a = one_program("web", sample_cfg(2));
        let mut new_cfg = sample_cfg(2);
        new_cfg.auto_start = false;
        let b = one_program("web", new_cfg);

        let diff = ConfigDiff::compute(&a, &b);

        assert_eq!(diff.actions.len(), 1);
        assert!(matches!(diff.actions[0], DiffAction::Replace { ref name, .. } if name == "web"));
    }

    #[test]
    fn new_program_emits_add() {
        let a = one_program("web", sample_cfg(1));
        let mut b = one_program("web", sample_cfg(1));
        b.program.insert("db".into(), sample_cfg(1));

        let diff = ConfigDiff::compute(&a, &b);

        assert_eq!(diff.actions.len(), 2);
        assert!(diff.actions.iter().any(|a| matches!(a, DiffAction::Keep { name } if name == "web")));
        assert!(diff.actions.iter().any(|a| matches!(a, DiffAction::Add  { name, .. } if name == "db")));
    }

    #[test]
    fn missing_program_emits_remove() {
        let mut a = one_program("web", sample_cfg(1));
        a.program.insert("db".into(), sample_cfg(1));
        let b = one_program("web", sample_cfg(1));

        let diff = ConfigDiff::compute(&a, &b);

        assert_eq!(diff.actions.len(), 2);
        assert!(diff.actions.iter().any(|a| matches!(a, DiffAction::Keep   { name    } if name == "web")));
        assert!(diff.actions.iter().any(|a| matches!(a, DiffAction::Remove { name    } if name == "db")));
    }

    #[test]
    fn replace_wins_over_scale_when_both_differ() {
        let a = one_program("web", sample_cfg(2));
        let mut new_cfg = sample_cfg(5);              // different count
        new_cfg.auto_start = false;                    // AND different field
        let b = one_program("web", new_cfg);

        let diff = ConfigDiff::compute(&a, &b);

        assert_eq!(diff.actions.len(), 1);
        assert!(matches!(diff.actions[0], DiffAction::Replace { .. }),
                "both-differ must emit Replace, not Scale");
    }
}