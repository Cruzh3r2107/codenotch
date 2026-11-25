// Codenotch Focus: the two things Codenotch cannot do for itself under Wayland, and nothing else.
//
//   FocusedPid()            -> u   process ID owning the focused window, 0 when none
//   Activate(au pids, s hint) -> b   bring forward the window of the first PID in `pids` that has one
//
// `pids` is a process chain, nearest first (claude, its shell, the terminal…), so the first match is
// the window closest to the session. Several windows of one process (a terminal with two windows)
// are told apart by `hint`, matched against window titles; without a match the most recently used
// one wins. Bound to the session bus as com.codenotch.Focus while the extension is enabled.
import Gio from 'gi://Gio';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const NAME = 'com.codenotch.Focus';
const PATH = '/com/codenotch/Focus';
const XML = `<node>
  <interface name="${NAME}">
    <method name="FocusedPid"><arg type="u" direction="out" name="pid"/></method>
    <method name="Activate">
      <arg type="au" direction="in" name="pids"/>
      <arg type="s" direction="in" name="hint"/>
      <arg type="b" direction="out" name="done"/>
    </method>
  </interface>
</node>`;

class Focus {
    FocusedPid() {
        const win = global.display.focus_window;
        return win ? Math.max(win.get_pid(), 0) : 0;
    }

    Activate(pids, hint) {
        // Most recently used first, across every workspace
        const windows = global.display
            .get_tab_list(0 /* Meta.TabList.NORMAL */, null)
            .filter(w => !w.skip_taskbar);
        const lower = (hint || '').toLowerCase();
        for (const pid of pids) {
            const own = windows.filter(w => w.get_pid() === pid);
            if (!own.length)
                continue;
            const titled = lower ? own.find(w => (w.get_title() || '').toLowerCase().includes(lower)) : null;
            Main.activateWindow(titled ?? own[0]);
            return true;
        }
        return false;
    }
}

export default class CodenotchFocusExtension extends Extension {
    enable() {
        this._dbus = Gio.DBusExportedObject.wrapJSObject(XML, new Focus());
        this._dbus.export(Gio.DBus.session, PATH);
        this._owner = Gio.bus_own_name_on_connection(Gio.DBus.session, NAME, Gio.BusNameOwnerFlags.NONE, null, null);
    }

    disable() {
        if (this._owner) {
            Gio.bus_unown_name(this._owner);
            this._owner = 0;
        }
        this._dbus?.unexport();
        this._dbus = null;
    }
}
