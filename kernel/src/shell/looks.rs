//! Screen commands: display (the screen mode) and theme (colors and other looks).

use super::{Args, CmdError, CmdResult, Pager, Shell};
use crate::display::{self, Request};
use crate::input::{self, Key};
use crate::theme::{self, Theme, ITEMS, OPTIONS, PRESETS};
use crate::vga::Color;
use crate::viewer::Viewer;
use crate::{console, pit};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

/// `view` and `gview`: opens the picture (errors are reported before the screen is touched) and
/// runs the viewer until the user leaves.
fn show_picture(shell: &Shell, a: &Args, gview: bool) -> CmdResult {
    let name = if gview { "gview" } else { "view" };
    let [file] = a.pos[..] else { return Err(CmdError::Usage) };
    let path = shell.path(file);
    let viewer = Viewer::open(&path, gview, !a.has('n'))
        .map_err(|e| CmdError::Message(format!("{name}: {file}: {e}")))?;
    viewer.run();
    Ok(())
}

pub(super) fn cmd_view(shell: &mut Shell, a: &Args, _: &mut dyn Write) -> CmdResult {
    show_picture(shell, a, false)
}

pub(super) fn cmd_gview(shell: &mut Shell, a: &Args, _: &mut dyn Write) -> CmdResult {
    show_picture(shell, a, true)
}

/// `paint [-s WxH] [file]`: opens (or starts) the picture and runs paint until the user leaves.
/// Errors are reported before the screen is touched.
pub(super) fn cmd_paint(shell: &mut Shell, a: &Args, _: &mut dyn Write) -> CmdResult {
    let file = match a.pos[..] {
        [] => None,
        [file] => Some(file),
        _ => return Err(CmdError::Usage),
    };
    let size = match a.value('s') {
        None => None,
        Some(text) => Some(
            crate::paint::parse_size(text).ok_or_else(|| CmdError::Message(format!("paint: {}", crate::paint::SIZE_ERROR)))?,
        ),
    };
    let paint = crate::paint::Paint::open(&shell.cwd, file, size).map_err(|e| CmdError::Message(format!("paint: {e}")))?;
    paint.run();
    Ok(())
}

/// How long `display` waits for the answer to "keep this mode?" before going back.
const KEEP_SECONDS: u64 = 15;

fn describe_request(request: Request) -> String {
    match request {
        Request::Graphics { scale, .. } => format!("{} at {scale}%", display::mode_name(request)),
        Request::Text { .. } => format!("{} (VGA text mode)", display::mode_name(request)),
    }
}

fn show_mode(out: &mut dyn Write) -> CmdResult {
    writeln!(out, "Now:         {}", display::mode().describe())?;
    let start = match display::saved() {
        Some(Ok(request)) => describe_request(request),
        Some(Err(e)) => format!("{} has a problem: {e}", display::CONFIG_PATH),
        None => String::from("text (VGA text mode; nothing saved)"),
    };
    writeln!(out, "At start-up: {start}")?;
    if let Some((w, h)) = display::preferred_resolution() {
        writeln!(out, "The monitor prefers {w}x{h} ('display native').")?;
    }
    writeln!(out, "'display -l' lists resolutions; e.g. 'display 1920x1080 -s 150' switches.")?;
    Ok(())
}

/// Common resolutions by aspect ratio, each with whether this computer can show it.
fn list_common(out: &mut dyn Write) -> CmdResult {
    writeln!(out, "Text modes: text (80x25), text50 (80x50). Graphics resolutions:")?;
    for (ratio, sizes) in display::COMMON {
        write!(out, "  {ratio:<7}")?;
        for &(w, h) in sizes.iter() {
            write!(out, " {:<10}", format!("{w}x{h}"))?;
        }
        writeln!(out)?;
        write!(out, "  {:<7}", "")?;
        for &(w, h) in sizes.iter() {
            let status = match display::plan(w, h) {
                Ok(plan) if plan.size() == (w, h) => "yes",
                Ok(_) => "framed",
                Err(_) => "-",
            };
            write!(out, " {status:<10}")?;
        }
        writeln!(out)?;
    }
    writeln!(out, "yes: shown exactly. framed: in the middle of a larger mode, with black")?;
    writeln!(out, "around it. -: not possible. Any other size works the same way:")?;
    writeln!(out, "'display <width>x<height>'.")?;
    if let Some((w, h)) = display::direct_limits() {
        writeln!(out, "This display can be set to any size up to {w}x{h}, as video memory allows.")?;
    }
    writeln!(out, "Scale (-s): 100 to 800 percent. At 100% a character is 8x16 pixels, so")?;
    writeln!(out, "1920x1080 gives 240x67 characters; at 200%, 120x33.")?;
    Ok(())
}

/// Every graphics mode of the video BIOS that the system can use.
fn list_vbe(out: &mut dyn Write) -> CmdResult {
    let (modes, version, memory) = display::vbe_modes();
    if modes.is_empty() {
        writeln!(out, "The video BIOS offers no graphics mode the system can use (VBE 2.0 or later")?;
        writeln!(out, "with a linear framebuffer, 15 to 32 bits per pixel).")?;
    } else {
        writeln!(
            out,
            "VBE {}.{}, {} MiB of video memory at {:#x}, {} usable modes:",
            version >> 8,
            version & 0xFF,
            memory >> 20,
            modes[0].base,
            modes.len()
        )?;
        for row in modes.chunks(3) {
            for m in row {
                write!(out, "  {:#06x} {:>5}x{:<5} {:>2}-bit", m.number, m.width, m.height, m.bpp)?;
            }
            writeln!(out)?;
        }
    }
    if let Some((w, h)) = display::direct_limits() {
        writeln!(out, "Bochs display interface (QEMU, Bochs, VirtualBox): any size up to {w}x{h}.")?;
    }
    Ok(())
}

/// Asks whether to keep the new mode; no answer in time counts as no.
fn ask_keep(out: &mut dyn Write) -> Result<bool, CmdError> {
    write!(out, "Keep this mode? (y/n; going back in {KEEP_SECONDS} s) ")?;
    let deadline = pit::uptime_ms() + KEEP_SECONDS * 1000;
    let key = input::read_key_until(|| pit::uptime_ms() >= deadline);
    let keep = matches!(key, Some(Key::Char(b'y' | b'Y')));
    writeln!(out, "{}", if keep { "y" } else if key.is_none() { "(no answer)" } else { "n" })?;
    Ok(keep)
}

pub(super) fn cmd_display(_: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    if a.has('a') {
        return list_vbe(out);
    }
    if a.has('l') {
        return list_common(out);
    }
    let scale = match a.value('s') {
        None => None,
        Some(v) => Some(
            v.trim_end_matches('%')
                .parse::<u32>()
                .map_err(|_| CmdError::Message(format!("-s: not a scale in percent: {v}")))?,
        ),
    };
    let current = display::mode();
    let request = match (&a.pos[..], scale) {
        ([], None) => return show_mode(out),
        ([], Some(scale)) => match current.request {
            Request::Graphics { width, height, .. } => Request::Graphics { width, height, scale },
            Request::Text { .. } => {
                return Err("a scale is for graphics modes: give one too, e.g. 'display 1920x1080 -s 150'".into())
            }
        },
        ([name], scale) => {
            let scale = scale.unwrap_or_else(display::last_scale);
            if name.eq_ignore_ascii_case("native") {
                let (width, height) =
                    display::preferred_resolution().ok_or("the monitor does not say which resolution it prefers")?;
                Request::Graphics { width, height, scale }
            } else {
                display::parse_mode(name, scale)?
            }
        }
        _ => return Err(CmdError::Usage),
    };
    let mode = console::set_display(request)?;
    writeln!(out, "{}", mode.describe())?;
    if !a.has('f') && !ask_keep(out)? {
        console::set_display(current.request)?;
        writeln!(out, "Back to {}", display::mode().describe())?;
        return Ok(());
    }
    if !a.has('n') {
        display::save(request)?;
        writeln!(out, "Saved: used at every start ({}).", display::CONFIG_PATH)?;
    }
    Ok(())
}

fn list_theme(out: &mut dyn Write, look: &Theme) -> CmdResult {
    writeln!(out, "Colors ('theme <item> <color> [on <background>]'):")?;
    for info in ITEMS {
        let value = look.value(info.name).unwrap_or_default();
        writeln!(out, "  {:<18} {value:<23} {}", info.name, info.about)?;
    }
    writeln!(out, "What the colors look like ('theme palette.<color> #rrggbb' or 'default'):")?;
    for row in Color::ALL.chunks(3) {
        for (name, _) in row {
            let value = look.value(&format!("palette.{name}")).unwrap_or_default();
            write!(out, "  {name:<10} {value:<9}")?;
        }
        writeln!(out)?;
    }
    writeln!(out, "Options ('theme <option> <value>'):")?;
    for (name, values, about) in OPTIONS {
        let value = look.value(name).unwrap_or_default();
        writeln!(out, "  {name:<18} {value:<10} {about} ({})", values.replace(' ', " or "))?;
    }
    let presets: Vec<&str> = PRESETS.iter().map(|(n, _)| *n).collect();
    writeln!(out, "Ready-made looks ('theme preset <name>'): {}", presets.join(" "))?;
    writeln!(out, "Changes are saved in {} and used at every start.", theme::CONFIG_PATH)?;
    Ok(())
}

pub(super) fn cmd_theme(_: &mut Shell, a: &Args, out: &mut dyn Write) -> CmdResult {
    let mut out = Pager::new(out, a.has('p'));
    let mut look = theme::current();
    let changed = match a.pos[..] {
        [] if a.has('r') => {
            look = Theme::CLASSIC;
            String::from("the classic looks")
        }
        [] => return list_theme(&mut out, &look),
        _ if a.has('r') => return Err(CmdError::Usage),
        ["preset"] => return Err("which preset? (see 'theme')".into()),
        ["preset", name] => {
            let names: Vec<&str> = PRESETS.iter().map(|(n, _)| *n).collect();
            look = theme::preset(name)
                .ok_or_else(|| CmdError::Message(format!("no preset {name}; there are: {}", names.join(" "))))?;
            format!("the {name} looks")
        }
        [name] => {
            let value = look.value(name).ok_or_else(|| CmdError::Message(format!("unknown setting: {name} (see 'theme')")))?;
            writeln!(out, "{name} {value}")?;
            return Ok(());
        }
        [name, ref value @ ..] => {
            look.set(name, value)?;
            format!("{name} {}", look.value(name).unwrap_or_default())
        }
    };
    theme::apply(look);
    if a.has('n') {
        writeln!(out, "Now {changed} (not saved)")?;
    } else {
        theme::save()?;
        writeln!(out, "Now {changed} (saved)")?;
    }
    Ok(())
}
