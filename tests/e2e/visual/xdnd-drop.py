#!/usr/bin/env python3
"""Drop a file on an X11 window as a real XDND drag source.

`xdotool` cannot do this: a file drop is not pointer input, it is the XDND
protocol — a ClientMessage handshake plus an X selection transfer.  The GPUI
client's X11 backend implements the target half (`XdndEnter` / `XdndPosition` /
`XdndDrop` plus a `text/uri-list` selection conversion), so the only way to
exercise the client's drop path without stubbing anything is to be a genuine
drag source on the same X server.

This is that source.  It owns `XdndSelection`, announces `text/uri-list`, walks
the target through the handshake, answers the target's `SelectionRequest` with
the file URI list, and finishes on `XdndFinished`.

Usage:
    xdnd-drop.py --window <xid> --path /some/file [--path /another]
"""

import argparse
import sys
import time
from urllib.parse import quote

from Xlib import X, Xatom, display
from Xlib.protocol import event

XDND_VERSION = 5


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--window", required=True, help="Target window id (decimal or 0x hex)")
    parser.add_argument("--path", action="append", required=True, help="File path to drop")
    parser.add_argument("--x", type=int, default=None, help="Root x of the drop point")
    parser.add_argument("--y", type=int, default=None, help="Root y of the drop point")
    args = parser.parse_args()

    target_id = int(args.window, 0)
    uri_list = "".join(f"file://{quote(path)}\r\n" for path in args.path).encode("utf-8")

    dpy = display.Display()
    screen = dpy.screen()
    root = screen.root

    atoms = {
        name: dpy.intern_atom(name)
        for name in (
            "XdndAware",
            "XdndEnter",
            "XdndPosition",
            "XdndStatus",
            "XdndDrop",
            "XdndFinished",
            "XdndSelection",
            "XdndActionCopy",
            "XdndTypeList",
            "text/uri-list",
        )
    }

    target = dpy.create_resource_object("window", target_id)
    aware = target.get_full_property(atoms["XdndAware"], Xatom.ATOM)
    if aware is None:
        sys.exit(f"target window {target_id:#x} is not XdndAware — it cannot accept a drop")

    # The source needs a window of its own: it is the ClientMessage sender, the
    # selection owner, and the recipient of XdndStatus / XdndFinished.
    source = root.create_window(
        0, 0, 1, 1, 0, screen.root_depth, X.InputOutput, X.CopyFromParent,
        event_mask=X.PropertyChangeMask,
    )
    source.set_wm_name("scribe-xdnd-source")
    source.set_selection_owner(atoms["XdndSelection"], X.CurrentTime)
    dpy.sync()
    if dpy.get_selection_owner(atoms["XdndSelection"]).id != source.id:
        sys.exit("could not take ownership of XdndSelection")

    def client_message(message_type, data):
        return event.ClientMessage(
            window=target, client_type=message_type, data=(32, data)
        )

    # 1. Enter: announce the protocol version and the one type on offer. Bit 0
    #    of data[1] stays clear, so the three type slots are read directly.
    target.send_event(
        client_message(
            atoms["XdndEnter"],
            [source.id, XDND_VERSION << 24, atoms["text/uri-list"], 0, 0],
        ),
        event_mask=X.NoEventMask,
    )
    dpy.flush()

    geometry = target.get_geometry()
    coords = target.translate_coords(root, 0, 0)
    origin_x = -coords.x
    origin_y = -coords.y
    drop_x = args.x if args.x is not None else origin_x + geometry.width // 2
    drop_y = args.y if args.y is not None else origin_y + geometry.height // 2

    # 2. Position: the target answers with XdndStatus and, on the first one,
    #    asks for the selection — that conversion is what carries the paths.
    target.send_event(
        client_message(
            atoms["XdndPosition"],
            [
                source.id,
                0,
                (drop_x << 16) | drop_y,
                X.CurrentTime,
                atoms["XdndActionCopy"],
            ],
        ),
        event_mask=X.NoEventMask,
    )
    dpy.flush()

    # 3. Serve the selection request, then drop. The loop below is the whole
    #    source state machine: answer the conversion, note the status, and exit
    #    once the target reports XdndFinished.
    served = False
    dropped = False
    deadline = time.time() + 15
    while time.time() < deadline:
        while dpy.pending_events():
            evt = dpy.next_event()
            if evt.type == X.SelectionRequest:
                requestor = evt.requestor
                requestor.change_property(
                    evt.property, atoms["text/uri-list"], 8, uri_list
                )
                requestor.send_event(
                    event.SelectionNotify(
                        time=evt.time,
                        requestor=requestor,
                        selection=evt.selection,
                        target=evt.target,
                        property=evt.property,
                    ),
                    event_mask=X.NoEventMask,
                )
                dpy.flush()
                served = True
            elif evt.type == X.ClientMessage:
                if evt.client_type == atoms["XdndFinished"]:
                    print("XdndFinished")
                    dpy.sync()
                    return
        if served and not dropped:
            # Only drop once the paths are actually across: the target turns
            # XdndDrop into the submit event that fires its drop handler, and a
            # handler that runs before the selection landed sees no paths.
            time.sleep(0.2)
            target.send_event(
                client_message(
                    atoms["XdndDrop"], [source.id, 0, X.CurrentTime, 0, 0]
                ),
                event_mask=X.NoEventMask,
            )
            dpy.flush()
            dropped = True
        time.sleep(0.05)

    if not served:
        sys.exit("target never asked for the XdndSelection — no drop happened")
    print("dropped without an XdndFinished acknowledgment")


if __name__ == "__main__":
    main()
