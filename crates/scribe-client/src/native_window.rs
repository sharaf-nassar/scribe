//! Choose a backend whose compositor owns the terminal's native window frame.
//!
//! GNOME does not advertise the Wayland decoration protocol. Rather than draw
//! an imitation frame or open a borderless terminal, re-exec on the same
//! desktop's X11 server. Compositors with native Wayland decorations keep the
//! Wayland path; settings retains its deliberately client-decorated design.

use std::{env, ffi::OsString, process::Command, sync::mpsc, time::Duration};

use wayland_client::{Connection, Dispatch, QueueHandle, protocol::wl_registry};
use x11rb::{connection::Connection as _, protocol::xproto::ConnectionExt as _};

const DECORATION_MANAGER: &str = "zxdg_decoration_manager_v1";
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const DESKTOP_WAYLAND_DISPLAY: &str = "SCRIBE_DESKTOP_WAYLAND_DISPLAY";

/// The desktop environment, before this client's backend-only re-exec.
/// Service startup must not erase Wayland from the user's systemd environment
/// just because this terminal uses X11 for native decorations.
pub fn desktop_env(name: &str) -> Option<OsString> {
    desktop_env_from(name, |key| env::var_os(key))
}

fn desktop_env_from(name: &str, lookup: impl Fn(&str) -> Option<OsString>) -> Option<OsString> {
    lookup(name)
        .or_else(|| (name == "WAYLAND_DISPLAY").then(|| lookup(DESKTOP_WAYLAND_DISPLAY)).flatten())
}

/// Startup failures precede windows, hooks, singleton claims and session IPC.
#[derive(Debug, thiserror::Error)]
pub enum NativeWindowError {
    #[error("cannot probe Wayland window decorations: {0}")]
    WaylandConnect(#[from] wayland_client::ConnectError),
    #[error("cannot read Wayland window decorations: {0}")]
    WaylandDispatch(#[source] Box<wayland_client::DispatchError>),
    #[error("native window-decoration probe did not complete: {0}")]
    Probe(#[from] mpsc::RecvTimeoutError),
    #[error("cannot start window-decoration probe: {0}")]
    ProbeStart(std::io::Error),
    #[error("cannot locate Scribe: {0}")]
    Executable(std::io::Error),
    #[error(
        "this Wayland compositor does not provide native window decorations. Enable XWayland with a window manager, or use a compositor supporting zxdg_decoration_manager_v1. Scribe will not open a terminal without native window controls."
    )]
    NoNativeFrame,
}

#[derive(Default)]
struct DecorationGlobals {
    native_frame: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for DecorationGlobals {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { interface, .. } = event {
            state.native_frame |= interface == DECORATION_MANAGER;
        }
    }
}

/// One local registry roundtrip, before any application window or IPC exists.
fn wayland_has_native_frame() -> Result<bool, NativeWindowError> {
    let connection = Connection::connect_to_env()?;
    let mut queue = connection.new_event_queue();
    let _registry = connection.display().get_registry(&queue.handle(), ());
    let mut globals = DecorationGlobals::default();
    queue
        .roundtrip(&mut globals)
        .map_err(|error| NativeWindowError::WaylandDispatch(Box::new(error)))?;
    Ok(globals.native_frame)
}

/// An environment variable alone does not prove that `XWayland` is usable.
/// Require a live EWMH window manager before committing to its native frame.
fn x11_has_window_manager() -> bool {
    let Ok((connection, screen)) = x11rb::connect(None) else { return false };
    let Some(screen) = connection.setup().roots.get(screen) else { return false };
    let Ok(atom_cookie) = connection.intern_atom(true, b"_NET_SUPPORTING_WM_CHECK") else {
        return false;
    };
    let Ok(atom) = atom_cookie.reply() else { return false };
    if atom.atom == x11rb::NONE {
        return false;
    }
    let Ok(root_cookie) = connection.get_property(
        false,
        screen.root,
        atom.atom,
        x11rb::protocol::xproto::AtomEnum::WINDOW,
        0,
        1,
    ) else {
        return false;
    };
    let Ok(root_reply) = root_cookie.reply() else { return false };
    let Some(wm) = root_reply.value32().and_then(|mut windows| windows.next()) else {
        return false;
    };
    let Ok(wm_cookie) = connection.get_property(
        false,
        wm,
        atom.atom,
        x11rb::protocol::xproto::AtomEnum::WINDOW,
        0,
        1,
    ) else {
        return false;
    };
    wm_cookie
        .reply()
        .is_ok_and(|reply| reply.value32().and_then(|mut windows| windows.next()) == Some(wm))
}

fn needs_probe(args: &[OsString], compositor: &str) -> bool {
    compositor == "Wayland"
        && !args.iter().any(|arg| {
            matches!(
                arg.to_str(),
                Some(
                    "--settings"
                        | "--vulkan-probe"
                        | "--terminal-image-renderer-probe"
                        | "--help"
                        | "--version"
                )
            )
        })
}

#[derive(Debug, PartialEq, Eq)]
enum NativeBackend {
    Wayland,
    X11,
}

fn select_backend(
    wayland_frame: bool,
    x11_frame: impl FnOnce() -> bool,
) -> Result<NativeBackend, NativeWindowError> {
    if wayland_frame {
        Ok(NativeBackend::Wayland)
    } else if x11_frame() {
        Ok(NativeBackend::X11)
    } else {
        Err(NativeWindowError::NoNativeFrame)
    }
}

fn probe_backend() -> Result<NativeBackend, NativeWindowError> {
    // Both Wayland's registry roundtrip and the X11 WM check can block on a
    // wedged server. Bound their total startup cost. Failure exits the process,
    // so the worker cannot linger behind a functioning client.
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("native-window-probe".to_owned())
        .spawn(move || {
            let result = wayland_has_native_frame().and_then(|native| {
                select_backend(native, || {
                    env::var_os("DISPLAY").is_some_and(|value| !value.is_empty())
                        && x11_has_window_manager()
                })
            });
            drop(sender.send(result));
        })
        .map_err(NativeWindowError::ProbeStart)?;
    receiver.recv_timeout(PROBE_TIMEOUT)?
}

fn x11_command(executable: OsString, arguments: &[OsString], wayland_display: OsString) -> Command {
    let mut command = Command::new(executable);
    // GPUI chooses Wayland solely from this variable. Leave DISPLAY, XDG
    // session identity, bus/runtime paths and all application arguments intact.
    // Preserve the desktop socket separately for service-environment import;
    // the backend selector's absence makes re-exec idempotent.
    command
        .args(arguments)
        .env_remove("WAYLAND_DISPLAY")
        .env(DESKTOP_WAYLAND_DISPLAY, wayland_display);
    command
}

/// Return the process-local re-exec needed for a native frame, if any.
///
/// Called before GPUI, hook repair, singleton acquisition or session restore.
/// No launcher/config file is written and no server process is restarted.
pub fn relaunch_command() -> Result<Option<Command>, NativeWindowError> {
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    if !needs_probe(&args, gpui::guess_compositor()) || probe_backend()? == NativeBackend::Wayland {
        return Ok(None);
    }
    let executable = env::current_exe().map_err(NativeWindowError::Executable)?;
    Ok(Some(x11_command(
        executable.into_os_string(),
        &args,
        env::var_os("WAYLAND_DISPLAY").unwrap_or_default(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    // @lat: [[test#GPUI Client Headless Suites#Native window backend selection]]
    #[test]
    fn only_interactive_wayland_terminals_probe_decorations() {
        assert!(needs_probe(&[], "Wayland"));
        assert!(needs_probe(&["--restore-child".into()], "Wayland"));
        assert!(!needs_probe(&[], "X11"));
        assert!(!needs_probe(&[], "Headless"));
        for mode in [
            "--settings",
            "--help",
            "--version",
            "--vulkan-probe",
            "--terminal-image-renderer-probe",
        ] {
            assert!(!needs_probe(&[mode.into()], "Wayland"), "{mode}");
        }
    }

    // @lat: [[test#GPUI Client Headless Suites#Native window backend selection]]
    #[test]
    fn native_wayland_wins_and_missing_x11_fails_instead_of_losing_controls() {
        let consulted_x11 = std::cell::Cell::new(false);
        assert_eq!(
            select_backend(true, || {
                consulted_x11.set(true);
                false
            })
            .unwrap(),
            NativeBackend::Wayland
        );
        assert!(!consulted_x11.get(), "native Wayland must not depend on DISPLAY");
        assert_eq!(select_backend(false, || true).unwrap(), NativeBackend::X11);
        assert!(matches!(select_backend(false, || false), Err(NativeWindowError::NoNativeFrame)));
    }

    // @lat: [[test#GPUI Client Headless Suites#Native window backend selection]]
    #[test]
    fn service_environment_keeps_wayland_after_backend_reexec() {
        let lookup = |key: &str| match key {
            DESKTOP_WAYLAND_DISPLAY => Some(OsString::from("wayland-original")),
            "XDG_SESSION_TYPE" => Some(OsString::from("wayland")),
            _ => None,
        };
        assert_eq!(desktop_env_from("WAYLAND_DISPLAY", lookup), Some("wayland-original".into()));
        assert_eq!(desktop_env_from("XDG_SESSION_TYPE", lookup), Some("wayland".into()));
        assert_eq!(desktop_env_from("DISPLAY", lookup), None);
        assert_eq!(desktop_env_from("WAYLAND_DISPLAY", |_| None), None);
        assert_eq!(
            desktop_env_from("WAYLAND_DISPLAY", |key| match key {
                "WAYLAND_DISPLAY" => Some("wayland-current".into()),
                _ => lookup(key),
            }),
            Some("wayland-current".into()),
            "a current desktop socket outranks an inherited marker"
        );
    }

    // @lat: [[test#GPUI Client Headless Suites#Native window backend selection]]
    #[test]
    fn reexec_preserves_arguments_and_desktop_environment() {
        let args = vec!["--restore-child".into(), "--finish-update-restart".into()];
        let command = x11_command("/opt/Scribe Client".into(), &args, "wayland-original".into());
        assert_eq!(command.get_program(), "/opt/Scribe Client");
        assert_eq!(command.get_args().collect::<Vec<_>>(), args);
        assert_eq!(
            command.get_envs().collect::<Vec<_>>(),
            vec![
                (
                    std::ffi::OsStr::new(DESKTOP_WAYLAND_DISPLAY),
                    Some(std::ffi::OsStr::new("wayland-original"))
                ),
                (std::ffi::OsStr::new("WAYLAND_DISPLAY"), None),
            ]
        );
    }
}
