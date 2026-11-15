use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread;

use config_parser::config_reader::ConfigReader;
use config_parser::config::Config;
use runtime::log::Logger;
use runtime::supervisor::Supervisor;
use runtime::unix::{SelfPipe, TerminalState};
use shell::{Reader, ShellCommand};


fn main() -> Result<(), Box<dyn std::error::Error> > {
	if let Err(error) = run() {
        eprintln!("Error: {}", error);
        std::process::exit(1);
    }
	Ok(())
}

fn run() -> Result<(), Box<dyn std::error::Error> > {
    let conf_path =  "./Run.toml";
	let mut config_reader = ConfigReader::new(conf_path);
	config_reader.read()?;
	let configs = config_reader.parse::<Config>()?;

    let mut sup = Supervisor::new(configs);
    sup.logger = Logger::open("taskmaster.log")?;
    sup.conf_path = PathBuf::from(conf_path);
    sup.log(" ----- Taskmaster started -----");
    sup.bootstrap()?;
    sup.auto_start();

    let inbox = sup.setup_inbox()?;

    println!("taskmaster started - type `quit` to exit, `status` to inspect");

    let terminal = TerminalState::save();
    let (tx, rx) = mpsc::channel::<ShellCommand>();
    let (resp_tx, resp_rx) = mpsc::channel::<String>();
    let shell_inbox = inbox.clone();
    let shell = thread::spawn(move || shell_loop(tx, resp_rx, shell_inbox));
    let mut shell_quit = false;

    loop {
        sup.step()?;

        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, ShellCommand::Quit) {
                shell_quit = true;
            }
            let resp = sup.apply_command(cmd);
            let _ = resp_tx.send(resp);
        }
        if sup.is_shutdown_done() { break; }
    }

    if shell_quit { let _ = shell.join(); }
    if let Some(terminal) = &terminal { terminal.restore(); }
    println!("sayounara!");
	Ok(())
}

fn shell_loop(tx: mpsc::Sender<ShellCommand>, resp_rx: mpsc::Receiver<String>, inbox: Arc<SelfPipe>) {
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
                match resp_rx.recv() {
                    Ok(resp) => println!("\r{resp}"),
                    Err(_) => return,
                }
                if is_quit { return; }
            }
            Err(err) => eprintln!("parse error: {err:?}"),
        }
    }

    let _ = tx.send(ShellCommand::Quit);
    let _ = inbox.notify();
    if let Ok(resp) = resp_rx.recv() {
        println!("\r{resp}");
    }
}
