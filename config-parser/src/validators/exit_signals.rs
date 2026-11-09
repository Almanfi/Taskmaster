use serde::{Deserialize};

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
#[repr(i32)]
pub enum ExitSignals {
	Hup = 1,
	Int = 2,
	Quit = 3,
	Kill = 9,
	Term = 15,
}

#[cfg(test)]
mod tests {
	use super::ExitSignals;
	use std::ffi::c_int;
	use serde::Deserialize;

	#[derive(Deserialize)]
	struct TestConfig {
		exit_signal: ExitSignals,
	}

	#[test]
	fn serde_deserializes_each_supported_signal() {
		let term: TestConfig = toml::from_str("exit_signal = \"TERM\"")
			.expect("TERM must deserialize");
		let kill: TestConfig = toml::from_str("exit_signal = \"KILL\"")
			.expect("KILL must deserialize");
		let int: TestConfig = toml::from_str("exit_signal = \"INT\"")
			.expect("INT must deserialize");
		let hup: TestConfig = toml::from_str("exit_signal = \"HUP\"")
			.expect("HUP must deserialize");
		let quit: TestConfig = toml::from_str("exit_signal = \"QUIT\"")
			.expect("QUIT must deserialize");

		assert!(matches!(term.exit_signal, ExitSignals::Term));
		assert!(matches!(kill.exit_signal, ExitSignals::Kill));
		assert!(matches!(int.exit_signal, ExitSignals::Int));
		assert!(matches!(hup.exit_signal, ExitSignals::Hup));
		assert!(matches!(quit.exit_signal, ExitSignals::Quit));
	}

	#[test]
	fn serde_rejects_unsupported_or_lowercase_signals() {
		for signal in ["STOP", "term"] {
			let configuration = format!("exit_signal = \"{signal}\"");
			assert!(toml::from_str::<TestConfig>(&configuration).is_err());
		}
	}

	#[test]
	fn signal_discriminants_match_posix_numbers() {
		assert_eq!(ExitSignals::Hup  as c_int,  1);
		assert_eq!(ExitSignals::Int  as c_int,  2);
		assert_eq!(ExitSignals::Quit as c_int,  3);
		assert_eq!(ExitSignals::Kill as c_int,  9);
		assert_eq!(ExitSignals::Term as c_int, 15);
	}
}
