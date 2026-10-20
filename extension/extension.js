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

        const activity = new PopupMenu.PopupImageMenuItem('Open activity', 'view-list-symbolic');
        activity.connect('activate', () => {
            try {
                Gio.Subprocess.new(['orchrd-activity'], Gio.SubprocessFlags.NONE);
            } catch (error) {
                Main.notifyError('Could not open Orchrd activity', error.message);
            }
        });
        this.menu.addMenuItem(activity);

        this._auto = new PopupMenu.PopupSwitchMenuItem('Auto speak results', extension.settings.get_boolean('auto-speak'));
        this._auto.connect('toggled', (_item, enabled) => extension.setAutoSpeak(enabled));
        this.menu.addMenuItem(this._auto);

        this._sounds = new PopupMenu.PopupSwitchMenuItem(
            'Interaction sounds',
            extension.settings.get_boolean('interface-sounds')
        );
        this._sounds.connect('toggled', (_item, enabled) =>
            extension.settings.set_boolean('interface-sounds', enabled));
        this.menu.addMenuItem(this._sounds);

        const repeat = new PopupMenu.PopupMenuItem('Repeat last response');
        repeat.connect('activate', () => extension.repeatLast());
        this.menu.addMenuItem(repeat);
    }

    update(snapshot) {
        const active = (snapshot.agents ?? []).find(a => a.agent_name === snapshot.active_agent);
        const agent = snapshot.active_agent ? ` · ${active?.display_name || snapshot.active_agent}` : '';
        this._status.label.text = `${snapshot.state}${agent}`;
        const agents = snapshot.agents ?? [];
        this._agents.removeAll();
        if (agents.length) {
            for (const agent of agents) {
                this._agents.addMenuItem(new PopupMenu.PopupMenuItem(
                    `${agent.display_name || agent.agent_name}: ${agent.status}`,
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
        this._lifetime = {};
        this._proxySignals = [];
        this._ownerSignal = null;
        this._snapshot = null;
        this.settings = this.getSettings();
        this._pressed = false;
        this._cueTimeouts = new Set();
        this._indicator = new OrchrdIndicator(this);
        Main.panel.addToStatusArea(this.uuid, this._indicator);
        this._connectProxy();

        const flags = Meta.KeyBindingFlags.IGNORE_AUTOREPEAT | Meta.KeyBindingFlags.TRIGGER_RELEASE;
        Main.wm.addKeybinding('push-to-talk', this.settings, flags, Shell.ActionMode.ALL,
            (_display, _window, event) => this._handleTalkEvent(event));
    }

    disable() {
        Main.wm.removeKeybinding('push-to-talk');
        if (this._pressed)
            this._call('CancelListening');
        this._pressed = false;
        this._lifetime = null;
        for (const timeout of this._cueTimeouts ?? [])
            GLib.source_remove(timeout);
        this._cueTimeouts?.clear();
        for (const signal of this._proxySignals ?? [])
            this._proxy?.disconnectSignal(signal);
        this._proxySignals = [];
        if (this._ownerSignal)
            this._proxy?.disconnect(this._ownerSignal);
        this._ownerSignal = null;
        this._proxy?.run_dispose();
        this._proxy = null;
        this._indicator?.destroy();
        this._indicator = null;
        this.settings = null;
        this._snapshot = null;
    }

    _connectProxy() {
        const lifetime = this._lifetime;
        this._proxy = new OrchrdProxy(Gio.DBus.session, BUS_NAME, OBJECT_PATH, (proxy, error) => {
            if (this._lifetime !== lifetime) {
                proxy.run_dispose();
                return;
            }
            if (error) {
                this._setOffline(error.message);
                return;
            }
            const connect = (name, callback) => {
                this._proxySignals.push(proxy.connectSignal(name, (...args) => {
                    if (this._lifetime === lifetime)
                        callback(...args);
                }));
            };
            connect('StateChanged', (_p, _sender, [json]) => this._applySnapshot(json));
            connect('CompletionFinished', (_p, _sender, [agent_name, speech]) => {
                if (!this.settings.get_boolean('auto-speak'))
                    this._ding('complete');
                const agent = this._snapshot?.agents?.find(a => a.agent_name === agent_name);
                Main.notify(`Orchrd · ${agent?.display_name || agent_name || 'Agent finished'}`, speech);
            });
            connect('ApprovalRequired', (_p, _sender, [_id, message]) => {
                this._ding('dialog-warning');
                Main.notify('Orchrd approval required', message);
            });
            connect('Error', (_p, _sender, [message]) => Main.notifyError('Orchrd', message));
            this._ownerSignal = proxy.connect('notify::g-name-owner', () => {
                if (this._lifetime !== lifetime)
                    return;
                if (proxy.g_name_owner) {
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
        const lifetime = this._lifetime;
        const remote = this._proxy[`${method}Remote`];
        remote.call(this._proxy, ...args, (_result, error) => {
            if (this._lifetime === lifetime && error)
                Main.notifyError('Orchrd', error.message);
        });
    }

    _refresh() {
        const lifetime = this._lifetime;
        this._proxy.GetSnapshotRemote((result, error) => {
            if (this._lifetime !== lifetime)
                return;
            if (error) this._setOffline(error.message);
            else this._applySnapshot(result[0]);
        });
    }

    _applySnapshot(json) {
        try {
            const snapshot = JSON.parse(json);
            this._playStateCue(this._snapshot?.state, snapshot.state);
            this._snapshot = snapshot;
            this._indicator.update(this._snapshot);
        }
        catch (error) { console.error(`Orchrd snapshot: ${error}`); }
    }

    _setOffline(message) {
        this._indicator?.update({state: 'offline', agents: [], message});
    }

    _ding(eventId) {
        try { global.display.get_sound_player().play_from_theme(eventId, 'Orchrd notification', null); }
        catch (error) { console.debug(`Orchrd sound unavailable: ${error}`); }
    }

    _playStateCue(previous, current) {
        if (!previous || previous === current || !this.settings?.get_boolean('interface-sounds'))
            return;

        if (current === 'transcribing') {
            this._ding('message');
        } else if (current === 'routing') {
            this._ding('dialog-information');
            this._queueCue('audio-volume-change', 180);
            this._queueCue('audio-volume-change', 360);
        }
    }

    _queueCue(eventId, delayMs) {
        const timeout = GLib.timeout_add(GLib.PRIORITY_DEFAULT, delayMs, () => {
            this._cueTimeouts.delete(timeout);
            if (this.settings?.get_boolean('interface-sounds'))
                this._ding(eventId);
            return GLib.SOURCE_REMOVE;
        });
        this._cueTimeouts.add(timeout);
    }
}
