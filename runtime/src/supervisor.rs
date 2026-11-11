use std::collections::{ HashMap, VecDeque};
use std::ffi::{c_int, c_short};
use std::thread::available_parallelism;
use std::time::{Instant, Duration};
use std::io;
// use crate::process::RuntimeProcess;
use crate::unix::{SIGKILL, send_signal};

use config_parser::config::{Config, ProgramConfig};
use monitoring::{PERF_RECORD_EXIT, PERF_RECORD_FORK, POLLERR, POLLHUP, POLLIN, POLLNVAL, Pid, TaskEventRecord, w_exit_status, w_if_exited, w_if_signaled, w_term_sig, wait4pid};
use monitoring::signalfd::block_sighup;
// use monitoring::signalfd;
use monitoring::{signalfd::SignalFd, PerfCounter, PerfRing, Poller};

/// Internal id for a program, stable for the Supervisor's lifetime.
/// Keeps `pid_map` free of allocated strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProgramId(pub u32);

/// Dispatch key: resolves a reaped pid back to its owning slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PidKey {
    pub program: ProgramId,
    pub slot: usize
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    Normal      { code: i32 },
    Signaled    { sig: c_int }
}

#[derive(Debug, Clone, Copy)]
pub enum SlotState {
    NotStarted,
    Starting    { since: Instant },
    Running     { since: Instant },
    Stopping     { since: Instant, deadline: Instant},
    Exited      { term: Termination, expected: bool },
    Backoff
}

pub struct Slot {
    pub pid: Option<c_int>,
    pub state: SlotState,
    pub tries: u32
}

impl Slot {
    pub fn new() -> Self {
         Self{ pid: None, state: SlotState::NotStarted, tries: 0 }
    }
}
#[derive(Debug, Clone, Copy)]
pub enum SlotOp {
    Spawn(usize),
    Stop(usize),
    Replace(usize)
}

#[derive(Debug, Clone, Copy)]
pub enum Phase {
    WaitingRunning   {idx: usize},
    WaitingExit     {idx: usize, respawn: bool}
}

#[derive(Debug, Default)]
pub struct Pending {
    pub queue: VecDeque<SlotOp>,
    pub current: Option<Phase>
}

impl Pending {

    pub fn grow(from: usize, to: usize) -> Self {
        let queue = (from..to).map(SlotOp::Spawn).collect();
        Self { queue, current: None}
    }

    pub fn replace_all(count: usize) -> Self {
        let queue = (0..count).map(SlotOp::Replace).collect();
        Self { queue, current: None}
    }

        pub fn shrink(from: usize, to: usize) -> Self {
        let queue = (to..from).rev().map(SlotOp::Stop).collect();
        Self { queue, current: None}
    }
}

pub struct Program {
    pub cfg: ProgramConfig,
    pub slots: Vec<Slot>,
    pub pending: Option<Pending>
}

pub struct Supervisor {
    pub programs: Vec<Program>,
    pub names: HashMap<String, ProgramId>,
    pub pid_map: HashMap<c_int, PidKey>,
    pub my_pid: c_int,

    pub rings: Vec<PerfRing>,
    pub counters: Vec<PerfCounter>,
    pub sighup: Option<SignalFd>,
    pub poller: Poller
}

impl Supervisor {
    pub fn new(cfg: Config) -> Self {
        let len = cfg.program.len();
        let mut programs = Vec::with_capacity(len);
        let mut names = HashMap::with_capacity(len);

        for (name, conf) in cfg.program {
            let id = ProgramId(programs.len() as u32);
            let count = conf.process_count.value as usize;
            let slots = {0..count}.map(|_| Slot::new()).collect();
            programs.push(Program {cfg: conf, slots, pending: None});
            names.insert(name, id);
        }
        Self {
            programs,
            names,
            pid_map: HashMap::new(),
            my_pid: std::process::id() as c_int,

            rings: Vec::new(),
            counters: Vec::new(),
            sighup: None,
            poller: Poller::new(),
        }
    }

    /// Spawn one slot under its program's current cfg.
    ///
    /// Low-level primitive: not called by bootstrap or reload directly.
    /// Caller (the rolling driver) ensures the slot is in NotStarted,
    /// Exited, or Backoff when this is invoked.
    pub fn spawn_slot(&mut self, program: ProgramId, slot_idx: usize) -> io::Result<()> {
        let prog_idx = program.0 as usize;

        // Scope the cfg borrow so we can take &mut self for the recording step.
        let pid = {
            let conf = &self.programs[prog_idx].cfg;
            let mut rp = crate::process::RuntimeProcess::new(conf);
            rp.start().map_err(|e| io::Error::other(e.to_string()))? as c_int
        };
        let slot = &mut self.programs[prog_idx].slots[slot_idx];
        slot.pid = Some(pid);
        slot.state = SlotState::Starting { since: (Instant::now()) };
        
        self.pid_map.insert(pid, PidKey { program, slot: slot_idx });
        Ok(())
    }

    /// Ask one slot to stop.
    ///
    /// Sends the program's configured `exit_signal` and transitions the slot
    /// to `Stopping` with a deadline. `tick()` (S07) escalates to SIGKILL
    /// once the deadline passes. Perf EXIT + `wait4` later flips the slot
    /// to `Exited`.
    ///
    /// Idempotent for slots that aren't actively running: a NotStarted,
    /// Exited, Backoff, or already-Stopping slot is left untouched.
    pub fn stop_slot(&mut self, program: ProgramId, slot_idx: usize) -> io::Result<()> {
        let prog_idx = program.0 as usize;
        let prog = &self.programs[prog_idx];
        let slot = &prog.slots[slot_idx];

        // Only Starting / Running slots have a live pid worth signaling.
        let pid = match slot.state {
            SlotState::Starting { .. } | SlotState::Running { .. } => {
                slot.pid.expect("Starting/Runing slot must carry a pid")
            }
            _ => return Ok(())
        };

        let sig = prog.cfg.exit_signal as c_int;
        let kill_time = prog.cfg.kill_time as u64;

        send_signal(pid, sig)?;

        let now = Instant::now();
        let slot = &mut self.programs[prog_idx].slots[slot_idx];
        slot.state = SlotState::Stopping { since: now, deadline: now + Duration::from_secs(kill_time) };
        Ok(())
    }

    /// Enforce per-slot deadlines.
    ///
    /// For now: any `Stopping` slot whose deadline has passed gets
    /// `SIGKILL`.  The slot stays `Stopping` — perf EXIT + `wait4`
    /// will flip it to `Exited`.  Signal errors (ESRCH when the child
    /// is already gone) are intentionally swallowed; there's nothing
    /// useful to do about them at this layer.
    pub fn tick(&mut self, now: Instant) {
        for prog in &self.programs {
            for slot in &prog.slots {
                if let SlotState::Stopping { deadline, .. } = slot.state 
                    && now >= deadline
                    && let Some(pid) = slot.pid {
                        let _ = send_signal(pid, SIGKILL);   
                }
            }
        }
    }

    /// Open the event sources and register them with the poller.
    ///
    /// After this returns, the poller watches one perf ring per online CPU
    /// plus the SIGHUP signalfd.  Nothing is spawned yet — that's S13.
    /// Call exactly once, right after `Supervisor::new`.
    pub fn bootstrap(&mut self) -> io::Result<()> {
        // SIGHUP must be blocked on this thread *before* signalfd opens,
        // otherwise the default disposition (terminate) stays armed.
        block_sighup()?;

        let n_cpus = available_parallelism()
            .map_err(|e| io::Error::other(e.to_string()))?
            .get();

        self.rings.reserve(n_cpus);
        self.counters.reserve(n_cpus);

        for cpu in 0..n_cpus {
            let counter = PerfCounter::open_tree_tracker_cpu(Pid(0), cpu as c_int)?;
            let ring = counter.attach_ring(4)?;
            self.poller.register(ring.as_raw_fd());
            self.counters.push(counter);
            self.rings.push(ring);
        }

        let sighup = SignalFd::open_sighup()?;
        self.poller.register(sighup.as_raw_fd());
        self.sighup = Some(sighup);

        Ok(())
    }

    /// Walk one program's `Pending` queue as far as it can go in one call.
    ///
    /// Each iteration of the inner `loop`:
    /// 1. If `current` is None → pop the next op, start it, set `current`.
    /// 2. If `current` is a WaitingRunning that has stabilized → clear it.
    /// 3. If `current` is a WaitingExit whose slot is Exited → either chain
    ///    the respawn (then_spawn=true) or clear it.
    /// 4. If queue empty + current None → walk done, clear `pending`.
    ///
    /// When the current phase is still pending, return `Ok(())` and wait
    /// for the next `step()`.
    pub fn advance_rolling(&mut self, pid: ProgramId) -> io::Result<()> {
        let prog_idx = pid.0 as usize;
        if self.programs[prog_idx].pending.is_none() {
            return Ok(());
        }
        let now = Instant::now();

        loop {
            let current = self.programs[prog_idx].pending.as_ref().unwrap().current;

            match current {
                None => {
                    let op = self.programs[prog_idx].pending.as_mut().unwrap().queue.pop_front();
                    match op {
                        None => {
                            self.programs[prog_idx].pending = None;
                            return Ok(());
                        }
                        Some(SlotOp::Spawn(idx)) => {
                            self.spawn_slot(pid, idx)?;
                            self.set_current(pid, Phase::WaitingRunning { idx });
                        }
                        Some(SlotOp::Stop(idx)) => {
                            self.stop_slot(pid, idx)?;
                            self.set_current(pid, Phase::WaitingExit { idx, respawn: false });
                        }
                        Some(SlotOp::Replace(idx)) => {
                            self.stop_slot(pid, idx)?;
                            self.set_current(pid, Phase::WaitingExit { idx, respawn: true });
                        }
                    }
                }
                Some(Phase::WaitingRunning { idx }) => {
                    if !self.has_stabilized(pid, idx, now) {
                        return Ok(());
                    }
                    self.clear_current(pid);
                }
                Some(Phase::WaitingExit { idx, respawn }) => {
                    let exited = matches!(
                        self.programs[prog_idx].slots[idx].state,
                        SlotState::Exited { .. }
                    );
                    if !exited {
                        return Ok(())
                    }
                    if respawn {
                        self.spawn_slot(pid, idx)?;
                        self.set_current(pid, Phase::WaitingRunning { idx });
                    } else {
                        self.clear_current(pid);
                    }
                }
            }
        }
    }

    fn set_current(&mut self, pid: ProgramId, phase: Phase) {
        if let Some(p) = &mut self.programs[pid.0 as usize].pending {
            p.current = Some(phase);
        }
    }

    fn clear_current(&mut self, pid: ProgramId) {
        if let Some(p) = &mut self.programs[pid.0 as usize].pending {
            p.current = None;
        }
    }

    fn has_stabilized(&mut self, program: ProgramId, slot_idx: usize, now: Instant) -> bool {
        let prog_idx = program.0 as usize;
        let runtime = Duration::from_secs(self.programs[prog_idx].cfg.runtime as u64);
        let slot = &mut self.programs[prog_idx].slots[slot_idx];
        match slot.state {
            SlotState::Starting { since } if  now.duration_since(since) >= runtime => {
                slot.state = SlotState::Running { since: now };
                true
            },
            SlotState::Running { .. } => true,
            _ => false
        }
    }

    /// Full record parsing + reap lands in S10.  For now just empty the ring
    /// so the kernel can keep writing to it.
    fn drain_ring(&mut self, idx: usize) -> io::Result<()> {
        let my_pid = self.my_pid;
        let mut exits: Vec<c_int> = Vec::new();

        self.rings[idx].drain(|header, bytes| {
            match header.type_ {
                PERF_RECORD_FORK => {
                    if let Some(record) = TaskEventRecord::try_from_bytes(bytes)
                        && record.ppid as c_int == my_pid {
                        eprintln!("New child {} created", record.pid);
                    }
                }
                PERF_RECORD_EXIT => {
                    if let Some(record) = TaskEventRecord::try_from_bytes(bytes)
                        && record.ppid as c_int == my_pid {
                            exits.push(record.pid as c_int);
                    }
                }
                _ => {}
            }
        });

        for pid in exits {
            self.reap_and_dispatch(pid)?;
        }
        Ok(())
    }

    /// Called with a pid that perf flagged as exited.  Reaps the zombie
    /// via `wait4` to read the status, then transitions the owning slot
    /// to `Exited`.
    ///
    /// `expected` stays `false` for now — restart policy against
    /// `exit_codes` is S15's job.
    fn reap_and_dispatch(&mut self, pid: c_int) -> io::Result<()> {
        let status = wait4pid(pid, 0)?;

        let termination = if w_if_exited(status) {
            Termination::Normal { code: w_exit_status(status) }
        } else if w_if_signaled(status) {
            Termination::Signaled { sig: w_term_sig(status) as c_int }
        } else {
            return Ok(());
        };

        let key = match self.pid_map.remove(&pid) {
            Some(pid) => pid,
            None => return Ok(()),
        };

        let slot = &mut self.programs[key.program.0 as usize].slots[key.slot];
        slot.pid = None;
        slot.state = SlotState::Exited { term: termination, expected: false };
        Ok(())
    }

    /// Full reload lands in S22.  For now just read the signalfd so it
    /// doesn't stay perpetually readable.
    fn drain_sighup(&mut self) -> io::Result<()> {
        if let Some(sfd) = &self.sighup {
            let _ = sfd.drain()?;
        }
        Ok(())
    }

    fn next_timeout_ms(&self) -> c_int {
        let now = Instant::now();
        let soonest = self.programs.iter()
            .flat_map(|p| p.slots.iter())
            .filter_map(|s| match s.state {
                SlotState::Stopping { deadline, ..} => Some(deadline),
                _ => None,
            }).min();
    
        match soonest {
            None                             => -1,
            Some(t) if t <= now     => 0,
            Some(t)                 => { (t - now).as_millis().min(c_int::MAX as u128) as c_int }
        }
    }

    /// One iteration of the event loop.
    ///
    /// Order: drive → tick → poll → dispatch.  Running the driver before
    /// blocking ensures the first iteration after bootstrap kicks off the
    /// auto-start walks *before* poll goes to sleep.
    pub fn step(&mut self) -> io::Result<()> {
        // 1. Drive pending rolling walks (empty stub today).
        for i in 0..self.programs.len() {
            self.advance_rolling(ProgramId(i as u32))?;
        }

        // 2. Enforce deadline
        self.tick(Instant::now());

        // 3. Find the next timeout
        let timeout_ms = self.next_timeout_ms();

        // 4. Poll for events
        let ready_pairs: Vec<(usize, c_short)> = self.poller.wait(timeout_ms)?.iter().collect();

        // 5. Dispatch events
        let n_rings = self.rings.len();
        for (i, revents) in ready_pairs {
            if revents & (POLLERR | POLLNVAL) != 0 { continue; }
            if revents & (POLLIN | POLLHUP) == 0 { continue; }

            if i < n_rings {
                self.drain_ring(i)?;
            } else {
                self.drain_sighup()?;
            }
        }
        Ok(())
    }

    pub fn enqueue_grow(&mut self, pid: ProgramId, from: usize, to: usize) {
        self.programs[pid.0 as usize].pending = Some(Pending::grow(from, to));
    }

    pub fn enqueue_replace_all(&mut self, pid: ProgramId, count: usize) {
        self.programs[pid.0 as usize].pending = Some(Pending::replace_all(count));
    }

    pub fn enqueue_shrink(&mut self, pid: ProgramId, from: usize, to: usize) {
        self.programs[pid.0 as usize].pending = Some(Pending::shrink(from, to));
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
    use monitoring::wait4pid;
    use crate::unix::SIGNAL_HANDLER_TEST_LOCK;
    use std::os::unix::fs::PermissionsExt;
    use std::fs;
    use std::collections::HashMap;

    fn program_cfg(count: u64) -> ProgramConfig {
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

    #[test]
    fn new_builds_one_program_per_entry_and_slots_match_process_count() {
        let mut cfg = Config { program: HashMap::new() };
        cfg.program.insert("a".into(), program_cfg(2));
        cfg.program.insert("b".into(), program_cfg(3));

        let s = Supervisor::new(cfg);

        assert_eq!(s.programs.len(), 2);
        assert_eq!(s.names.len(), 2);
        assert!(s.pid_map.is_empty());

        let total_slots: usize = s.programs.iter().map(|p| p.slots.len()).sum();
        assert_eq!(total_slots, 5);

        for p in &s.programs {
            assert!(p.pending.is_none());
            for slot in &p.slots {
                assert!(slot.pid.is_none());
                assert_eq!(slot.tries, 0);
                assert!(matches!(slot.state, SlotState::NotStarted));
            }
        }

        assert!(s.rings.is_empty());
        assert!(s.counters.is_empty());
        assert!(s.sighup.is_none());
        assert!(s.my_pid > 0);
    }

    #[test]
    fn names_map_points_at_valid_program_ids() {
        let mut cfg = Config { program: HashMap::new() };
        cfg.program.insert("only".into(), program_cfg(1));

        let s = Supervisor::new(cfg);
        let id = s.names.get("only").copied().expect("name must be registered");
        assert!((id.0 as usize) < s.programs.len());
    }

    #[test]
    fn spawn_slot_records_pid_and_transitions_to_starting() {
        let mut raw = Config { program: HashMap::new() };
        let mut c = program_cfg(1);
        c.command = Command::try_from("/bin/true".to_string()).unwrap();
        raw.program.insert("fast".into(), c);

        let mut sup = Supervisor::new(raw);
        let id = sup.names["fast"];

        sup.spawn_slot(id, 0).expect("spawn_slot must succeed");

        let slot = &sup.programs[id.0 as usize].slots[0];
        let pid = slot.pid.expect("pid must be recorded");
        assert!(matches!(slot.state, SlotState::Starting { .. }));
        assert_eq!(
            sup.pid_map.get(&pid).copied(),
            Some(PidKey { program: id, slot: 0 }),
        );

        // Reap so the zombie doesn't leak into later tests.
        let _ = wait4pid(pid, 0);
    }

    #[test]
    fn stop_slot_signals_pid_and_transitions_to_stopping() {
        let mut raw = Config { program: HashMap::new() };
        let mut c = program_cfg(1);
        c.command      = Command::try_from("/bin/sleep 10".to_string()).unwrap();
        c.kill_time    = 5;
        c.exit_signal  = ExitSignals::Term;
        raw.program.insert("long".into(), c);

        let mut sup = Supervisor::new(raw);
        let id = sup.names["long"];

        sup.spawn_slot(id, 0).expect("spawn_slot");

        let before = Instant::now();
        sup.stop_slot(id, 0).expect("stop_slot");
        let after  = Instant::now();

        let slot = &sup.programs[id.0 as usize].slots[0];
        match slot.state {
            SlotState::Stopping { since, deadline } => {
                assert!(since >= before && since <= after);
                assert_eq!(deadline - since, Duration::from_secs(5));
            }
            other => panic!("expected Stopping, got {:?}", other),
        }

        // Reap so no zombie leaks into other tests.
        if let Some(pid) = slot.pid {
            let _ = wait4pid(pid, 0);
        }
    }

    #[test]
    fn stop_slot_is_a_noop_on_a_not_started_slot() {
        let mut raw = Config { program: HashMap::new() };
        raw.program.insert("idle".into(), program_cfg(1));

        let mut sup = Supervisor::new(raw);
        let id = sup.names["idle"];

        // No spawn. Slot stays NotStarted.
        sup.stop_slot(id, 0).expect("noop stop must succeed");

        let slot = &sup.programs[id.0 as usize].slots[0];
        assert!(matches!(slot.state, SlotState::NotStarted));
        assert!(slot.pid.is_none());
    }

    #[test]
    fn tick_before_deadline_preserves_stopping_state() {
        let mut raw = Config { program: HashMap::new() };
        let mut c = program_cfg(1);
        c.command     = Command::try_from("/bin/sleep 10".to_string()).unwrap();
        c.kill_time   = 60;
        c.exit_signal = ExitSignals::Term;
        raw.program.insert("sleeper".into(), c);

        let mut sup = Supervisor::new(raw);
        let id = sup.names["sleeper"];
        sup.spawn_slot(id, 0).unwrap();
        sup.stop_slot(id, 0).unwrap();

        // Deadline is 60s away → tick must not escalate.
        sup.tick(Instant::now());

        let state = sup.programs[id.0 as usize].slots[0].state;
        assert!(matches!(state, SlotState::Stopping { .. }));

        let pid = sup.programs[id.0 as usize].slots[0].pid.unwrap();
        let _ = wait4pid(pid, 0);
    }

    #[test]
    fn tick_escalates_to_sigkill_after_deadline() {
        let _lock = SIGNAL_HANDLER_TEST_LOCK.lock().expect("signal lock");

        // Child that ignores SIGTERM — only SIGKILL will take it down.
        let dir = std::env::temp_dir().join(format!(
            "taskmaster-tick-escalate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        ));
        fs::create_dir(&dir).expect("temp dir");
        let script = dir.join("resist.sh");
        fs::write(&script, "#!/bin/sh\ntrap '' TERM\nsleep 30\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();

        let mut raw = Config { program: HashMap::new() };
        let mut c = program_cfg(1);
        c.command     = Command::try_from(script.to_string_lossy().into_owned()).unwrap();
        c.chdir       = WorkingDir::try_from(dir.to_string_lossy().into_owned()).unwrap();
        c.kill_time   = 0;                     // immediate escalation window
        c.exit_signal = ExitSignals::Term;
        raw.program.insert("resist".into(), c);

        let mut sup = Supervisor::new(raw);
        let id = sup.names["resist"];
        sup.spawn_slot(id, 0).expect("spawn");

        // Let the script set its trap before SIGTERM arrives.
        std::thread::sleep(Duration::from_millis(100));

        sup.stop_slot(id, 0).expect("stop_slot sends SIGTERM");

        // kill_time == 0 → deadline is `since`, so any tick after now escalates.
        sup.tick(Instant::now() + Duration::from_millis(1));

        let pid = sup.programs[id.0 as usize].slots[0].pid.unwrap();
        let status = wait4pid(pid, 0).expect("reap");
        assert!(monitoring::w_if_signaled(status),
                "child must die from a signal, not a normal exit");
        assert_eq!(monitoring::w_term_sig(status), 9,
                "child must die from SIGKILL, not SIGTERM");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bootstrap_opens_rings_signalfd_and_leaves_slots_not_started() {
        let _lock = SIGNAL_HANDLER_TEST_LOCK.lock().expect("signal lock");

        let mut raw = Config { program: HashMap::new() };
        raw.program.insert("idle".into(), program_cfg(2));

        let mut sup = Supervisor::new(raw);
        sup.bootstrap().expect("bootstrap must succeed");

        let n_cpus = std::thread::available_parallelism().unwrap().get();
        assert_eq!(sup.rings.len(), n_cpus);
        assert_eq!(sup.counters.len(), n_cpus);
        assert!(sup.sighup.is_some());

        // Nothing spawned — all slots still NotStarted.
        for p in &sup.programs {
            for s in &p.slots {
                assert!(s.pid.is_none());
                assert!(matches!(s.state, SlotState::NotStarted));
            }
        }
    }

    #[test]
    fn step_drains_a_ring_after_a_child_exits() {
        let _lock = SIGNAL_HANDLER_TEST_LOCK.lock().expect("signal lock");

        let mut raw = Config { program: HashMap::new() };
        let mut c = program_cfg(1);
        c.command = Command::try_from("/bin/true".to_string()).unwrap();
        raw.program.insert("fast".into(), c);

        let mut sup = Supervisor::new(raw);
        sup.bootstrap().expect("bootstrap");

        let id  = sup.names["fast"];
        sup.spawn_slot(id, 0).expect("spawn");
        let pid = sup.programs[id.0 as usize].slots[0].pid.unwrap();

        let initial_tails: Vec<u64> =
            sup.rings.iter().map(|r| r.data_tail()).collect();

        let deadline = Instant::now() + Duration::from_millis(500);
        let mut advanced = false;
        while Instant::now() < deadline {
            sup.step().expect("step");
            advanced = sup.rings.iter().enumerate()
                .any(|(i, r)| r.data_tail() > initial_tails[i]);
            if advanced { break; }
        }
        assert!(advanced, "step() should have drained at least one ring after the child exited");

        let _ = wait4pid(pid, 0);
    }

    #[test]
    fn step_reaps_direct_child_and_marks_slot_exited() {
        let _lock = SIGNAL_HANDLER_TEST_LOCK.lock().expect("signal lock");

        let mut raw = Config { program: HashMap::new() };
        let mut c = program_cfg(1);
        c.command = Command::try_from("/bin/true".to_string()).unwrap();
        raw.program.insert("fast".into(), c);

        let mut sup = Supervisor::new(raw);
        sup.bootstrap().expect("bootstrap");

        let id  = sup.names["fast"];
        sup.spawn_slot(id, 0).expect("spawn");
        let pid = sup.programs[id.0 as usize].slots[0].pid.unwrap();

        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            sup.step().expect("step");
            let s = &sup.programs[id.0 as usize].slots[0];
            if matches!(s.state, SlotState::Exited { .. }) { break; }
            if Instant::now() > deadline {
                panic!("slot never reached Exited within 500 ms");
            }
        }

        let slot = &sup.programs[id.0 as usize].slots[0];
        assert!(matches!(
            slot.state,
            SlotState::Exited { term: Termination::Normal { code: 0 }, expected: false }
        ));
        assert!(slot.pid.is_none());
        assert!(!sup.pid_map.contains_key(&pid));
    }

    #[test]
    fn pending_grow_queues_spawn_ops_in_order() {
        let p = Pending::grow(0, 3);
        assert!(p.current.is_none());
        let ops: Vec<SlotOp> = p.queue.into_iter().collect();
        assert_eq!(ops.len(), 3);
        assert!(matches!(ops[0], SlotOp::Spawn(0)));
        assert!(matches!(ops[1], SlotOp::Spawn(1)));
        assert!(matches!(ops[2], SlotOp::Spawn(2)));
    }

    #[test]
    fn pending_grow_from_nonzero_offset() {
        let p = Pending::grow(2, 5);
        let ops: Vec<SlotOp> = p.queue.into_iter().collect();
        assert!(matches!(ops.as_slice(),
            [SlotOp::Spawn(2), SlotOp::Spawn(3), SlotOp::Spawn(4)]));
    }

    #[test]
    fn pending_replace_all_queues_replace_ops_from_zero() {
        let p = Pending::replace_all(3);
        let ops: Vec<SlotOp> = p.queue.into_iter().collect();
        assert_eq!(ops.len(), 3);
        for (i, op) in ops.iter().enumerate() {
            assert!(matches!(op, SlotOp::Replace(idx) if *idx == i));
        }
    }

    #[test]
    fn pending_shrink_queues_stops_from_highest_to_lowest() {
        let p = Pending::shrink(5, 2);
        let ops: Vec<SlotOp> = p.queue.into_iter().collect();
        assert!(matches!(ops.as_slice(),
            [SlotOp::Stop(4), SlotOp::Stop(3), SlotOp::Stop(2)]));
    }

    #[test]
    fn pending_builders_handle_empty_ranges() {
        assert!(Pending::grow(3, 3).queue.is_empty());
        assert!(Pending::shrink(2, 2).queue.is_empty());
        assert!(Pending::replace_all(0).queue.is_empty());
    }

    #[test]
    fn advance_rolling_grows_slots_in_order_and_clears_pending() {
        let _lock = SIGNAL_HANDLER_TEST_LOCK.lock().expect("signal lock");

        let mut raw = Config { program: HashMap::new() };
        let mut c = program_cfg(2);
        c.command = Command::try_from("/bin/sleep 10".to_string()).unwrap();
        c.runtime = 0;                      // Starting → Running on first check
        raw.program.insert("growing".into(), c);

        let mut sup = Supervisor::new(raw);
        sup.bootstrap().expect("bootstrap");

        let id = sup.names["growing"];
        sup.enqueue_grow(id, 0, 2);

        // One step walks the whole queue.
        sup.step().expect("step");

        let p = &sup.programs[id.0 as usize];
        assert!(matches!(p.slots[0].state, SlotState::Running { .. }),
            "slot 0 should be Running after grow");
        assert!(matches!(p.slots[1].state, SlotState::Running { .. }),
            "slot 1 should be Running after grow");
        assert!(p.pending.is_none(), "walk should be complete");
        assert!(p.slots[0].pid.is_some() && p.slots[1].pid.is_some());

        // Cleanup — kill the sleeps so later tests aren't hanging on zombies.
        use crate::unix::{send_signal, SIGKILL};
        for i in 0..sup.programs[id.0 as usize].slots.len() {
            if let Some(pid) = sup.programs[id.0 as usize].slots[i].pid {
                let _ = send_signal(pid, SIGKILL);
            }
        }
        // Pump step() briefly so reap_and_dispatch cleans up.
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            sup.step().ok();
            let any_alive = sup.programs[id.0 as usize].slots.iter().any(|s| s.pid.is_some());
            if !any_alive { break; }
        }
    }
}