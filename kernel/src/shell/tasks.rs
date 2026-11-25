//! Desktop and task commands: desktop, start, tasks, kill, exit.

use super::{Args, CmdError, CmdResult, Shell};
use crate::{desktop, fs, paging, task};
use core::fmt::Write;

pub(super) fn cmd_desktop(_: &mut Shell, a: &Args, _: &mut dyn Write) -> CmdResult {
    if !a.pos.is_empty() {
        return Err(CmdError::Usage);
    }
    desktop::run()
}

fn need_desktop() -> CmdResult {
    if desktop::is_active() {
        Ok(())
    } else {
        Err("the desktop is not running (start it with 'desktop')".into())
    }
}

pub(super) fn cmd_start(shell: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    need_desktop()?;
    let slot = match a.pos.split_first() {
        None => desktop::open_terminal(&shell.cwd)?,
        Some((file, args)) => {
            // A file, or a program found by name the way the shell finds it ("start clock").
            let mut path = shell.path(file);
            if fs::with(|fs| fs.stat(&path)).is_err() {
                path = shell.find_runnable(file).unwrap_or(path);
            }
            desktop::open(&path, args, &shell.cwd, a.has('e'))?
        }
    };
    writeln!(out, "started task {}", task::id_of(slot))?;
    Ok(())
}

pub(super) fn cmd_tasks(_: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    if !a.pos.is_empty() {
        return Err(CmdError::Usage);
    }
    let current = task::current();
    let foreground = task::foreground();
    writeln!(out, "    ID  STATE    MEMORY  NAME")?;
    for t in task::list() {
        let state = match t.state {
            _ if t.slot == current => "running",
            task::State::Ready => "ready",
            task::State::Sleeping { .. } => "waiting",
            task::State::Dead => "ended",
            task::State::Free => "free",
        };
        let memory = match paging::slot_of_task(t.slot) {
            Some(slot) => alloc::format!("slot {slot}"),
            None => alloc::string::String::from("-"),
        };
        let mark = if t.slot == foreground { '*' } else { ' ' };
        writeln!(out, "  {mark}{:>3}  {state:<8} {memory:<7} {}", t.id, t.name)?;
    }
    writeln!(out, "(* = in front; MEMORY = the program slot of a running program)")?;
    Ok(())
}

pub(super) fn cmd_kill(_: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    if a.pos.is_empty() {
        return Err(CmdError::Usage);
    }
    for &arg in &a.pos {
        let id: u32 = arg.parse().map_err(|_| CmdError::Message(alloc::format!("not a task id: {arg}")))?;
        let slot = task::slot_of(id).ok_or_else(|| CmdError::Message(alloc::format!("no task {id}")))?;
        if slot == task::BOOT {
            return Err(CmdError::Message(alloc::format!("task {id} is the {}; it cannot be closed", task::name_of(slot))));
        }
        if slot == task::current() {
            return Err("to close this terminal, use 'exit'".into());
        }
        if a.has('f') {
            task::kill(slot);
            writeln!(out, "killed task {id}")?;
        } else {
            task::close(slot);
            writeln!(out, "asked task {id} to close")?;
        }
    }
    Ok(())
}

pub(super) fn cmd_exit(shell: &mut Shell, a: &Args, _: &mut dyn Write) -> CmdResult {
    if !a.pos.is_empty() {
        return Err(CmdError::Usage);
    }
    if task::current() == task::BOOT {
        return Err("this is the boot shell, which cannot be closed (exit closes desktop terminals)".into());
    }
    shell.exiting = true;
    Ok(())
}
