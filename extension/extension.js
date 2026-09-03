import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

const BUS_NAME = 'io.github.kacper.Orchrd1';
const OBJECT_PATH = '/io/github/kacper/Orchrd1';
const IFACE = 'io.github.kacper.Orchrd1';
const HERDR_SESSION = 'orchrd';

const OrchrdProxy = Gio.DBusProxy.makeProxyWrapper(`<node>
  <interface name="${IFACE}">
    <method name="StartListening"/><method name="StopListening"/><method name="CancelListening"/>
    <method name="RepeatLast"/>
    <method name="SetAnnouncementMode"><arg type="s" direction="in"/></method>
    <method name="Approve"><arg type="s" direction="in"/><arg type="b" direction="in"/></method>
    <method name="GetSnapshot"><arg type="s" direction="out"/></method>
    <signal name="StateChanged"><arg type="s"/></signal>
    <signal name="TranscriptReady"><arg type="s"/></signal>
    <signal name="CompletionFinished"><arg type="s"/><arg type="s"/></signal>
    <signal name="ApprovalRequired"><arg type="s"/><arg type="s"/></signal>
    <signal name="Error"><arg type="s"/></signal>
  </interface>
</node>`);

const OrchrdIndicator = GObject.registerClass(
class OrchrdIndicator extends PanelMenu.Button {
    _init(extension) {
        super._init(0.5, 'Orchrd');
        this._extension = extension;
        const iconFile = extension.dir.get_child('icons').get_child('orchrd-symbolic.svg');
        this._icon = new St.Icon({
            gicon: new Gio.FileIcon({file: iconFile}),
            style_class: 'system-status-icon orchrd-panel-icon',
        });
        this.add_child(this._icon);

        this._status = new PopupMenu.PopupMenuItem('Orchrd: connecting', {reactive: false});
        this.menu.addMenuItem(this._status);
        this._agents = new PopupMenu.PopupMenuSection();
        this.menu.addMenuItem(this._agents);
        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

        const openHerdr = new PopupMenu.PopupImageMenuItem('Open Herdr', 'utilities-terminal-symbolic');
        openHerdr.connect('activate', () => extension.openHerdr());
        this.menu.addMenuItem(openHerdr);

        this._auto = new PopupMenu.PopupSwitchMenuItem('Auto speak results', extension.settings.get_boolean('auto-speak'));
        this._auto.connect('toggled', (_item, enabled) => extension.setAutoSpeak(enabled));
        this.menu.addMenuItem(this._auto);

        const repeat = new PopupMenu.PopupMenuItem('Repeat last response');
        repeat.connect('activate', () => extension.repeatLast());
        this.menu.addMenuItem(repeat);
    }

    update(snapshot) {
        const project = snapshot.active_project ? ` · ${snapshot.active_project}` : '';
        this._status.label.text = `${snapshot.state}${project}`;
        const agents = snapshot.agents ?? [];
        this._agents.removeAll();
        if (agents.length) {
            for (const agent of agents) {
                this._agents.addMenuItem(new PopupMenu.PopupMenuItem(
                    `${agent.project_id}: ${agent.status}`,
                    {reactive: false}
                ));
            }
        } else {
            this._agents.addMenuItem(new PopupMenu.PopupMenuItem(
                'No agents',
                {reactive: false}
            ));
        }
        if (snapshot.state === 'listening')
            this.add_style_pseudo_class('active');
        else
            this.remove_style_pseudo_class('active');
    }
});

export default class OrchrdExtension extends Extension {
    enable() {
        this.settings = this.getSettings();
        this._pressed = false;
        this._indicator = new OrchrdIndicator(this);
        Main.panel.addToStatusArea(this.uuid, this._indicator);
        this._connectProxy();

        const flags = Meta.KeyBindingFlags.IGNORE_AUTOREPEAT | Meta.KeyBindingFlags.TRIGGER_RELEASE;
        Main.wm.addKeybinding('push-to-talk', this.settings, flags, Shell.ActionMode.ALL,
            (_display, _window, event) => this._handleTalkEvent(event));
    }

    disable() {
        Main.wm.removeKeybinding('push-to-talk');
        this._proxy?.disconnectSignal?.(this._stateSignal);
        this._proxy = null;
        this._indicator?.destroy();
        this._indicator = null;
        this.settings = null;
    }

    _connectProxy() {
        this._proxy = new OrchrdProxy(Gio.DBus.session, BUS_NAME, OBJECT_PATH, proxy => {
            if (proxy instanceof Error) {
                this._setOffline(proxy.message);
                return;
            }
            this._stateSignal = this._proxy.connectSignal('StateChanged', (_p, _sender, [json]) => this._applySnapshot(json));
            this._proxy.connectSignal('CompletionFinished', (_p, _sender, [project, speech]) => {
                if (!this.settings.get_boolean('auto-speak'))
                    this._ding('complete');
                Main.notify(`Orchrd · ${project || 'Agent finished'}`, speech);
            });
            this._proxy.connectSignal('ApprovalRequired', (_p, _sender, [_id, message]) => {
                this._ding('dialog-warning');
                Main.notify('Orchrd approval required', message);
            });
            this._proxy.connectSignal('Error', (_p, _sender, [message]) => Main.notifyError('Orchrd', message));
            this._proxy.connect('notify::g-name-owner', () => {
                if (this._proxy.g_name_owner) {
                    this._syncMode();
                    this._refresh();
                } else {
                    this._setOffline('Service unavailable');
                }
            });
            this._syncMode();
            this._refresh();
        });
    }

    _handleTalkEvent(event) {
        if (!event)
            return;
        if (event.type() === Clutter.EventType.KEY_PRESS && !this._pressed) {
            this._pressed = true;
            this.startListening();
        } else if (event.type() === Clutter.EventType.KEY_RELEASE && this._pressed) {
            this._pressed = false;
            this.stopListening();
        }
    }

    startListening() { this._call('StartListening'); }
    stopListening() { this._call('StopListening'); }
    repeatLast() { this._call('RepeatLast'); }

    openHerdr() {
        try {
            Gio.Subprocess.new([
                '/run/current-system/sw/bin/kgx',
                '--title=Orchrd · Herdr',
                '--',
                '/etc/profiles/per-user/kacper/bin/herdr',
                '--session',
                HERDR_SESSION,
            ], Gio.SubprocessFlags.NONE);
        } catch (error) {
            Main.notifyError('Orchrd', `Could not open Herdr: ${error.message}`);
        }
    }

    setAutoSpeak(enabled) {
        this.settings.set_boolean('auto-speak', enabled);
        this._call('SetAnnouncementMode', enabled ? 'auto' : 'muted');
    }

    _syncMode() { this._call('SetAnnouncementMode', this.settings.get_boolean('auto-speak') ? 'auto' : 'muted'); }

    _call(method, ...args) {
        if (!this._proxy?.g_name_owner) {
            this._setOffline('Service unavailable');
            return;
        }
        const remote = this._proxy[`${method}Remote`];
        remote.call(this._proxy, ...args, (_result, error) => {
            if (error)
                Main.notifyError('Orchrd', error.message);
        });
    }

    _refresh() {
        this._proxy.GetSnapshotRemote((result, error) => {
            if (error) this._setOffline(error.message);
            else this._applySnapshot(result[0]);
        });
    }

    _applySnapshot(json) {
        try { this._indicator.update(JSON.parse(json)); }
        catch (error) { console.error(`Orchrd snapshot: ${error}`); }
    }

    _setOffline(message) {
        this._indicator?.update({state: 'offline', agents: [], message});
    }

    _ding(eventId) {
        try { global.display.get_sound_player().play_from_theme(eventId, 'Orchrd notification', null); }
        catch (error) { console.debug(`Orchrd sound unavailable: ${error}`); }
    }
}
