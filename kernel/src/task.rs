//! Tasks and the scheduler: several things running at once on the one CPU.
//!
//! A task is a thread of kernel code with its own stack: a terminal (a shell), an editor, or a
//! program and the code that runs it. Every task runs in ring 0, like everything else. A task
//! that runs a program also has its own page tables, so each program sees its own memory at
//! `PROGRAM_BASE` (see `paging.rs`).
//!
//! Switching: every interrupt saves all registers on the current stack (`isr_common`), so the
//! saved stack pointer is a complete record of a task that is not running. `schedule` picks the
//! next task and returns its stack pointer (and page tables), and `isr_common` resumes it.
//! The scheduler runs after every IRQ (the 100 Hz timer gives each task a 10 ms time slice, round
//! robin) and on `int 0x81`, which tasks use to give up the CPU (`yield_now`, sleeping, waiting
//! for input, exiting).
//!
//! Why this is safe without more locking: `SpinLock` disables interrupts while it is held, so a
//! task can only be interrupted (and switched away from) while it holds no lock. For the same
//! reason the scheduler may take the task table's lock: whoever it interrupted cannot hold it.
//! A task must never yield while it holds a lock (`yield_now` checks).
//!
//! Slot 0 is the boot task (the boot shell, later the desktop), which runs on the boot stack.
//! Slot 1 is the idle task, which halts the CPU when nothing else is ready.

use crate::interrupts::InterruptFrame;
use crate::sync::SpinLock;
use crate::{console, cpu, gdt, pit};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub const MAX_TASKS: usize = 16;
/// Kernel stack of each task (the boot task keeps the 64 KiB boot stack).
pub const STACK_SIZE: usize = 64 * 1024;
/// The boot task's slot.
pub const BOOT: usize = 0;
const IDLE: usize = 1;

const RFLAGS_IF: u64 = 1 << 9;
/// Bit 1 of RFLAGS is always set.
const RFLAGS_RESERVED: u64 = 1 << 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Free,
    Ready,
    /// Waiting until the tick count reaches `until`, or for input / Ctrl-C / a close request.
    Sleeping { until: u64, wake_on_input: bool },
    /// Ended; its stack and screen are freed by `reap`.
    Dead,
}

#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct StackChunk([u8; 16]);

struct Task {
    /// Shown to users; never reused. The boot task is 0.
    id: u32,
    name: String,
    state: State,
    /// Saved stack pointer (pointing at an `InterruptFrame`) while not running.
    rsp: u64,
    /// Page tables the task runs with.
    cr3: u64,
    stack: Option<Box<[StackChunk]>>,
}

impl Task {
    const EMPTY: Task = Task { id: 0, name: String::new(), state: State::Free, rsp: 0, cr3: 0, stack: None };
}

struct Tasks {
    slots: [Task; MAX_TASKS],
    next_id: u32,
}

static TASKS: SpinLock<Tasks> = SpinLock::new(Tasks { slots: [const { Task::EMPTY }; MAX_TASKS], next_id: 1 });

/// The running task's slot. `program.rs` reads it from assembly.
pub static CURRENT: AtomicUsize = AtomicUsize::new(BOOT);
/// The task whose screen is shown and which gets the keyboard.
static FOREGROUND: AtomicUsize = AtomicUsize::new(BOOT);
/// The scheduler is set up.
static RUNNING: AtomicBool = AtomicBool::new(false);
/// Ctrl-C for a task's program (set from the keyboard IRQ).
static BREAK: [AtomicBool; MAX_TASKS] = [const { AtomicBool::new(false) }; MAX_TASKS];
/// The task has been asked to close.
static CLOSE: [AtomicBool; MAX_TASKS] = [const { AtomicBool::new(false) }; MAX_TASKS];

/// The software interrupt that enters the scheduler.
pub const VEC_YIELD: u64 = 0x81;

/// Why the scheduler runs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The timer: the time slice is over.
    Tick,
    /// Another IRQ (the keyboard): only switch if the CPU was idle.
    Irq,
    /// The task gave up the CPU.
    Yield,
}

/// Where `isr_common` continues: a stack pointer, and page tables to load first (0 = keep).
#[repr(C)]
pub struct Switch {
    pub rsp: u64,
    pub cr3: u64,
}

/// Sets up the boot task and the idle task. Needs the heap; call before enabling interrupts.
pub fn init() {
    let cr3 = cpu::read_cr3();
    {
        let mut tasks = TASKS.lock();
        let boot = &mut tasks.slots[BOOT];
        boot.id = 0;
        boot.name = String::from("shell");
        boot.state = State::Ready;
        boot.cr3 = cr3;
    }
    let (stack, rsp) = new_stack(idle_entry as *const () as u64, 0);
    let mut tasks = TASKS.lock();
    let idle = &mut tasks.slots[IDLE];
    idle.name = String::from("idle");
    idle.state = State::Ready;
    idle.cr3 = cr3;
    idle.stack = Some(stack);
    idle.rsp = rsp;
    drop(tasks);
    RUNNING.store(true, Ordering::Release);
}

extern "C" fn idle_entry(_: u64) -> ! {
    loop {
        cpu::enable_and_hlt();
    }
}

/// Allocates a stack with an `InterruptFrame` on top that starts `entry(arg)` with interrupts
/// enabled. Returns the stack and the stack pointer to resume.
fn new_stack(entry: u64, arg: u64) -> (Box<[StackChunk]>, u64) {
    let mut stack = vec![StackChunk([0; 16]); STACK_SIZE / 16].into_boxed_slice();
    let top = stack.as_mut_ptr() as u64 + STACK_SIZE as u64;
    // The entry function sees a call frame: rsp + 8 is 16-aligned, with a 0 return address.
    let entry_rsp = top - 16 - 8;
    let frame_addr = (entry_rsp - 64 - size_of::<InterruptFrame>() as u64) & !15;
    let frame = InterruptFrame {
        r15: 0,
        r14: 0,
        r13: 0,
        r12: 0,
        r11: 0,
        r10: 0,
        r9: 0,
        r8: 0,
        rbp: 0,
        rdi: arg,
        rsi: 0,
        rdx: 0,
        rcx: 0,
        rbx: 0,
        rax: 0,
        vector: 0,
        error_code: 0,
        rip: entry,
        cs: gdt::KERNEL_CODE as u64,
        rflags: RFLAGS_IF | RFLAGS_RESERVED,
        rsp: entry_rsp,
        ss: gdt::KERNEL_DATA as u64,
    };
    unsafe {
        (entry_rsp as *mut u64).write(0);
        (frame_addr as *mut InterruptFrame).write(frame);
    }
    (stack, frame_addr)
}

type Entry = Box<dyn FnOnce() + Send>;

extern "C" fn task_entry(arg: u64) -> ! {
    let entry = unsafe { Box::from_raw(arg as *mut Entry) };
    entry();
    exit()
}

/// Starts a new task running `f`, with its own screen. Returns its slot.
pub fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> Result<usize, &'static str> {
    reap();
    let entry: Box<Entry> = Box::new(Box::new(f));
    let arg = Box::into_raw(entry) as u64;
    let (stack, rsp) = new_stack(task_entry as *const () as u64, arg);
    let name = String::from(name);
    let cr3 = crate::paging::kernel_cr3();

    let mut tasks = TASKS.lock();
    let Some(slot) = (0..MAX_TASKS).find(|&i| i != BOOT && i != IDLE && tasks.slots[i].state == State::Free) else {
        drop(tasks);
        drop(unsafe { Box::from_raw(arg as *mut Entry) });
        return Err("too many tasks");
    };
    // Not runnable until it has a screen.
    tasks.slots[slot].state = State::Dead;
    drop(tasks);
    BREAK[slot].store(false, Ordering::Relaxed);
    CLOSE[slot].store(false, Ordering::Relaxed);
    crate::input::clear_queue(slot);
    console::create_screen(slot);

    let mut tasks = TASKS.lock();
    let id = tasks.next_id;
    tasks.next_id += 1;
    let task = &mut tasks.slots[slot];
    task.id = id;
    task.name = name;
    task.rsp = rsp;
    task.cr3 = cr3;
    task.stack = Some(stack);
    task.state = State::Ready;
    Ok(slot)
}

/// Frees the stacks and screens of tasks that have ended.
pub fn reap() {
    let current = current();
    loop {
        let found = {
            let mut tasks = TASKS.lock();
            (0..MAX_TASKS)
                .find(|&i| i != current && tasks.slots[i].state == State::Dead && tasks.slots[i].stack.is_some())
                .map(|i| {
                    let task = &mut tasks.slots[i];
                    let stack = task.stack.take();
                    let name = core::mem::take(&mut task.name);
                    (i, stack, name)
                })
        };
        let Some((slot, stack, name)) = found else { break };
        // Free outside the lock.
        drop(stack);
        drop(name);
        console::remove_screen(slot);
        crate::paging::release_task(slot);
        crate::input::clear_queue(slot);
        crate::program::forget(slot);
        TASKS.lock().slots[slot].state = State::Free;
    }
}

/// Ends the running task.
pub fn exit() -> ! {
    cpu::cli();
    TASKS.lock().slots[current()].state = State::Dead;
    switch_away();
    unreachable!("a dead task was resumed");
}

/// Gives the CPU to the scheduler with interrupts off (`int 0x81`). Returns when this task runs
/// again, with the interrupt flag as it was.
fn switch_away() {
    if crate::sync::locks_held() != 0 {
        panic!("a task tried to give up the CPU while holding a lock");
    }
    unsafe { core::arch::asm!("int 0x81") };
}

/// Lets other ready tasks run first.
pub fn yield_now() {
    if RUNNING.load(Ordering::Acquire) {
        switch_away();
    }
}

fn sleep_state(state: State) {
    let restore = cpu::interrupts_enabled();
    cpu::cli();
    TASKS.lock().slots[current()].state = state;
    switch_away();
    if restore {
        cpu::sti();
    }
}

/// Sleeps until the tick count reaches `until`, or until Ctrl-C or a close request.
pub fn sleep_until(until: u64) {
    if !RUNNING.load(Ordering::Acquire) {
        while pit::ticks() < until {
            cpu::hlt();
        }
        return;
    }
    if pit::ticks() < until {
        sleep_state(State::Sleeping { until, wake_on_input: false });
    }
}

/// Waits a little for input: until the next tick, or a key on the PS/2 keyboard.
pub fn wait_input() {
    if !RUNNING.load(Ordering::Acquire) {
        cpu::cli();
        if crate::keyboard::has_scancode() {
            cpu::sti();
        } else {
            cpu::enable_and_hlt();
        }
        return;
    }
    sleep_state(State::Sleeping { until: pit::ticks() + 1, wake_on_input: true });
}

/// Called from `interrupt_dispatch` with interrupts off, at the end of an IRQ or on `int 0x81`.
/// `frame` is where the interrupted task's registers were saved.
pub fn schedule(frame: *mut InterruptFrame, reason: Reason) -> Switch {
    let stay = Switch { rsp: frame as u64, cr3: 0 };
    if !RUNNING.load(Ordering::Acquire) {
        return stay;
    }
    let current = current();
    let mut tasks = TASKS.lock();
    tasks.slots[current].rsp = frame as u64;

    // Wake sleepers.
    let now = pit::ticks();
    let input = crate::keyboard::has_scancode();
    for (i, task) in tasks.slots.iter_mut().enumerate() {
        if let State::Sleeping { until, wake_on_input } = task.state {
            let flagged = BREAK[i].load(Ordering::Relaxed) || CLOSE[i].load(Ordering::Relaxed);
            if now >= until || (wake_on_input && input) || flagged {
                task.state = State::Ready;
            }
        }
    }

    if reason == Reason::Irq && current != IDLE && tasks.slots[current].state == State::Ready {
        return stay;
    }
    // Round robin: the next ready task after the current one; the idle task only if none.
    let next = (1..=MAX_TASKS)
        .map(|step| (current + step) % MAX_TASKS)
        .find(|&i| i != IDLE && tasks.slots[i].state == State::Ready)
        .unwrap_or(IDLE);
    if next == current {
        return stay;
    }
    CURRENT.store(next, Ordering::Relaxed);
    let task = &tasks.slots[next];
    let cr3 = if task.cr3 != cpu::read_cr3() { task.cr3 } else { 0 };
    Switch { rsp: task.rsp, cr3 }
}

pub fn current() -> usize {
    CURRENT.load(Ordering::Relaxed)
}

pub fn foreground() -> usize {
    FOREGROUND.load(Ordering::Relaxed)
}

pub fn set_foreground(slot: usize) {
    FOREGROUND.store(slot, Ordering::Relaxed);
}

/// Numbers new tasks from 1 again, if no task but the boot and idle tasks exists (used after
/// the self-tests, whose tasks would otherwise use up the first ids).
pub fn restart_ids() {
    let mut tasks = TASKS.lock();
    let only_boot = (0..MAX_TASKS).all(|i| i == BOOT || i == IDLE || tasks.slots[i].state == State::Free);
    if only_boot {
        tasks.next_id = 1;
    }
}

/// Loads new page tables for the running task (and records them for when it resumes).
pub fn set_cr3(cr3: u64) {
    let restore = cpu::interrupts_enabled();
    cpu::cli();
    TASKS.lock().slots[current()].cr3 = cr3;
    unsafe { core::arch::asm!("mov cr3, {}", in(reg) cr3, options(nostack)) };
    if restore {
        cpu::sti();
    }
}

// --- Ctrl-C and close requests ---

/// Ctrl-C for task `slot`'s program. Called from the keyboard IRQ: atomics only.
pub fn request_break(slot: usize) {
    if let Some(flag) = BREAK.get(slot) {
        flag.store(true, Ordering::Relaxed);
    }
}

pub fn break_pending(slot: usize) -> bool {
    BREAK.get(slot).is_some_and(|f| f.load(Ordering::Relaxed))
}

/// Consumes a pending Ctrl-C of task `slot`.
pub fn take_break(slot: usize) -> bool {
    BREAK.get(slot).is_some_and(|f| f.swap(false, Ordering::Relaxed))
}

/// Asks a task to end: its program stops, and whatever waits for input gets `Key::Close`.
pub fn close(slot: usize) {
    if slot < MAX_TASKS {
        CLOSE[slot].store(true, Ordering::Relaxed);
        BREAK[slot].store(true, Ordering::Relaxed);
    }
}

pub fn close_requested(slot: usize) -> bool {
    CLOSE.get(slot).is_some_and(|f| f.load(Ordering::Relaxed))
}

/// Ends a task at once, wherever it is. What it had allocated on the heap is lost. The boot
/// task, the idle task and the running task cannot be killed.
pub fn kill(slot: usize) -> bool {
    if slot == BOOT || slot == IDLE || slot == current() || slot >= MAX_TASKS {
        return false;
    }
    let mut tasks = TASKS.lock();
    let task = &mut tasks.slots[slot];
    if matches!(task.state, State::Free | State::Dead) {
        return false;
    }
    task.state = State::Dead;
    true
}

// --- Looking at tasks ---

#[derive(Clone)]
pub struct Info {
    pub slot: usize,
    pub id: u32,
    pub name: String,
    pub state: State,
}

/// The tasks that exist, except the idle task, in slot order.
pub fn list() -> Vec<Info> {
    let tasks = TASKS.lock();
    tasks
        .slots
        .iter()
        .enumerate()
        .filter(|&(i, t)| i != IDLE && !matches!(t.state, State::Free))
        .map(|(slot, t)| Info { slot, id: t.id, name: t.name.clone(), state: t.state })
        .collect()
}

pub fn is_alive(slot: usize) -> bool {
    slot < MAX_TASKS && !matches!(TASKS.lock().slots[slot].state, State::Free | State::Dead)
}

/// The slot of the task with this id, if it is still running.
pub fn slot_of(id: u32) -> Option<usize> {
    let tasks = TASKS.lock();
    (0..MAX_TASKS).find(|&i| {
        i != IDLE && tasks.slots[i].id == id && !matches!(tasks.slots[i].state, State::Free | State::Dead)
    })
}

pub fn id_of(slot: usize) -> u32 {
    TASKS.lock().slots.get(slot).map_or(0, |t| t.id)
}

pub fn name_of(slot: usize) -> String {
    TASKS.lock().slots.get(slot).map(|t| t.name.clone()).unwrap_or_default()
}

pub fn rename(slot: usize, name: &str) {
    let name = String::from(name);
    let old = TASKS.lock().slots.get_mut(slot).map(|t| core::mem::replace(&mut t.name, name));
    drop(old);
}

/// Waits until task `slot` has ended.
pub fn join(slot: usize) {
    while is_alive(slot) {
        sleep_until(pit::ticks() + 1);
    }
}
