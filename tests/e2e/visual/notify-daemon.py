#!/usr/bin/env python3
"""A real freedesktop notification service for the notification E2E.

The GPUI client's dispatcher talks raw `zbus` to `org.freedesktop.Notifications`
on the session bus.  Nothing in the visual container owns that name, so without
this stub the client's `Notify` call fails at the bus and the whole delivery
half of the feature is unobservable.  This is a genuine D-Bus service — it
claims the well-known name, implements the four methods the spec requires, and
emits the two signals — so the client under test is exercising its real
transport, its real `replaces_id` coalescing, and its real signal subscriptions.

Every `Notify` and `CloseNotification` is appended to a JSONL record, which is
what the test script asserts against.  A control FIFO lets the script make the
service emit `ActionInvoked` (the click) or `NotificationClosed` (expiry) at a
chosen moment, which is the only way to drive click-to-focus without a human.

Usage:
    notify-daemon.py --record /output/notifications.jsonl --control /tmp/notify.ctl

Control commands, one per line:
    invoke <id> [action_key]   emit ActionInvoked
    closed <id> [reason]       emit NotificationClosed
"""

import argparse
import json
import os
import threading
import time

import dbus
import dbus.service
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib

BUS_NAME = "org.freedesktop.Notifications"
OBJECT_PATH = "/org/freedesktop/Notifications"


class NotificationService(dbus.service.Object):
    """The service object exported at /org/freedesktop/Notifications."""

    def __init__(self, bus_name, record_path):
        super().__init__(bus_name, OBJECT_PATH)
        self.record_path = record_path
        self.next_id = 1
        self.live = set()
        self.lock = threading.Lock()

    def _record(self, entry):
        entry["at"] = time.time()
        with open(self.record_path, "a", encoding="utf-8") as handle:
            handle.write(json.dumps(entry) + "\n")
            handle.flush()
            os.fsync(handle.fileno())

    @dbus.service.method(BUS_NAME, in_signature="susssasa{sv}i", out_signature="u")
    def Notify(
        self,
        app_name,
        replaces_id,
        app_icon,
        summary,
        body,
        actions,
        hints,
        expire_timeout,
    ):
        # The spec's coalescing contract: a non-zero replaces_id that names a
        # still-live notification is answered with that same id, so the toast is
        # swapped in place instead of stacking.  A stale id allocates a new one.
        with self.lock:
            replaces_id = int(replaces_id)
            if replaces_id != 0 and replaces_id in self.live:
                notification_id = replaces_id
            else:
                notification_id = self.next_id
                self.next_id += 1
            self.live.add(notification_id)
        self._record(
            {
                "call": "notify",
                "id": notification_id,
                "app_name": str(app_name),
                "replaces_id": replaces_id,
                "app_icon": str(app_icon),
                "summary": str(summary),
                "body": str(body),
                "actions": [str(action) for action in actions],
                "expire_timeout": int(expire_timeout),
            }
        )
        return dbus.UInt32(notification_id)

    @dbus.service.method(BUS_NAME, in_signature="u", out_signature="")
    def CloseNotification(self, notification_id):
        notification_id = int(notification_id)
        with self.lock:
            self.live.discard(notification_id)
        self._record({"call": "close_notification", "id": notification_id})
        self.NotificationClosed(dbus.UInt32(notification_id), dbus.UInt32(3))

    @dbus.service.method(BUS_NAME, in_signature="", out_signature="as")
    def GetCapabilities(self):
        return ["actions", "body", "persistence"]

    @dbus.service.method(BUS_NAME, in_signature="", out_signature="ssss")
    def GetServerInformation(self):
        return ("scribe-e2e-notifyd", "scribe", "1.0", "1.2")

    @dbus.service.signal(BUS_NAME, signature="us")
    def ActionInvoked(self, notification_id, action_key):
        pass

    @dbus.service.signal(BUS_NAME, signature="uu")
    def NotificationClosed(self, notification_id, reason):
        pass


def serve_control(service, control_path):
    """Read one command per line from the control FIFO, forever.

    A FIFO rather than a socket because the test script only ever needs to
    write a line: `printf 'invoke 1\\n' > "$CONTROL"` is the whole client side.
    """
    while True:
        with open(control_path, "r", encoding="utf-8") as handle:
            for line in handle:
                parts = line.split()
                if not parts:
                    continue
                command = parts[0]
                if command == "invoke" and len(parts) >= 2:
                    key = parts[2] if len(parts) > 2 else "default"
                    GLib.idle_add(
                        service.ActionInvoked, dbus.UInt32(int(parts[1])), key
                    )
                elif command == "closed" and len(parts) >= 2:
                    reason = int(parts[2]) if len(parts) > 2 else 1
                    GLib.idle_add(
                        service.NotificationClosed,
                        dbus.UInt32(int(parts[1])),
                        dbus.UInt32(reason),
                    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--record", required=True)
    parser.add_argument("--control", required=True)
    args = parser.parse_args()

    open(args.record, "w", encoding="utf-8").close()
    if not os.path.exists(args.control):
        os.mkfifo(args.control)

    DBusGMainLoop(set_as_default=True)
    bus = dbus.SessionBus()
    # REPLACE_EXISTING keeps a stale daemon from a previous phase from wedging
    # the run; the client only ever resolves the well-known name.
    name = dbus.service.BusName(BUS_NAME, bus=bus, replace_existing=True, do_not_queue=True)
    service = NotificationService(name, args.record)

    thread = threading.Thread(
        target=serve_control, args=(service, args.control), daemon=True
    )
    thread.start()
    GLib.MainLoop().run()


if __name__ == "__main__":
    main()
