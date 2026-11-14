use std::sync::{Arc, mpsc};
use std::thread;

use config_parser::config_reader::ConfigReader;
use config_parser::config::Config;
use runtime::log::Logger;
use runtime::supervisor::Supervisor;
use runtime::unix::SelfPipe;
use shell::{Reader, ShellCommand};


fn main() -> Result<(), Box<dyn std::error::Error> > {
	if let Err(error) = run() {
        eprintln!("Error: {}", error);
        std::process::exit(1);
    }
	Ok(())
}

fn run() -> Result<(), Box<dyn std::error::Error> > {
	let mut config_reader = ConfigReader::new("./Run.toml");
	config_reader.read()?;
	let configs = config_reader.parse::<Config>()?;

    let mut sup = Supervisor::new(configs);
    sup.logger = Logger::open("taskmaster.log")?;
    sup.log(" ----- Taskmaster started -----");
    sup.bootstrap()?;
    sup.auto_start();

    let inbox = sup.setup_inbox()?;

    println!("taskmaster started - type `quit` to exit, `status` to inspect");

    let (tx, rx) = mpsc::channel::<ShellCommand>();
    let shell_inbox = inbox.clone();
    thread::spawn(move || shell_loop(tx, shell_inbox));

    loop {
        sup.step()?;

        while let Ok(cmd) = rx.try_recv() {
            let resp = sup.apply_command(cmd);
            println!("\r{resp}");
        }
        if sup.is_shutdown_done() { break; }
    }

    println!("sayounara!");
	Ok(())
}

fn shell_loop(tx: mpsc::Sender<ShellCommand>, inbox: Arc<SelfPipe>) {
    let mut reader = match Reader::new() {
        Ok(r) => r,
        Err(err) => {
            eprintln!("could not initiate shell: {err}");
            let _ = tx.send(ShellCommand::Quit);
            let _ = inbox.notify();
            return ;
        }
    };
    while let Some(line) = reader.readline() {
        match line {
            Ok(cmd) => {
                let is_quit = matches!(cmd, ShellCommand::Quit);
                if tx.send(cmd).is_err() { return; }
                let _ = inbox.notify();
                if is_quit { return; }
            }
            Err(err) => eprintln!("parse error: {err:?}"),
        }
    }

    let _ = tx.send(ShellCommand::Quit);
    let _ = inbox.notify();
}