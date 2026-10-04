// Run with: gjs extension/test-lifecycle.js
// Exercise the extension's async lifecycle with a fake D-Bus proxy, without a shell session.
const {Gio, GLib} = imports.gi;
const [, bytes] = Gio.File.new_for_path('extension/extension.js').load_contents(null);
const source = new TextDecoder().decode(bytes)
    .replace(/^import .*;\n/gm, '')
    .replace('export default class OrchrdExtension', 'return class OrchrdExtension');

function assert(condition, message) {
    if (!condition)
        throw new Error(message);
}

const proxies = [];
let notifications = 0;
class FakeProxy {
    constructor(_bus, _name, _path, ready) {
        this.ready = ready;
        this.g_name_owner = 'test-owner';
        this.signals = new Map();
        this.handlers = new Map();
        this.serial = 0;
        this.disposed = false;
        this.cancelled = false;
        proxies.push(this);
    }
    connectSignal(name, callback) {
        const id = ++this.serial;
        this.signals.set(id, {name, callback});
        return id;
    }
    disconnectSignal(id) { this.signals.delete(id); }
    connect(_name, callback) {
        const id = ++this.serial;
        this.handlers.set(id, callback);
        return id;
    }
    disconnect(id) { this.handlers.delete(id); }
    run_dispose() { this.disposed = true; }
    SetAnnouncementModeRemote(_mode, callback) { this.modeReply = callback; }
    GetSnapshotRemote(callback) { this.snapshotReply = callback; }
    CancelListeningRemote(callback) { this.cancelled = true; this.cancelReply = callback; }
}

const ExtensionClass = new Function(
    'Gio', 'GLib', 'GObject', 'PanelMenu', 'Extension', 'Main', source
)(
    {DBus: {session: {}}, DBusProxy: {makeProxyWrapper: () => FakeProxy}},
    GLib,
    {registerClass: klass => klass},
    {Button: class {}},
    class {},
    {wm: {removeKeybinding() {}}, notify() { notifications++; }, notifyError() { notifications++; }}
);

function begin(extension) {
    extension._lifetime = {};
    extension._proxySignals = [];
    extension._ownerSignal = null;
    extension._cueTimeouts = new Set();
    extension.settings = {get_boolean: () => false};
    extension._indicator = {destroy() {}, update() { throw new Error('stale snapshot applied'); }};
    extension._connectProxy();
    return proxies.at(-1);
}

const extension = new ExtensionClass();
const first = begin(extension);
first.ready(first, null);
assert(first.signals.size === 4 && first.handlers.size === 1, 'all proxy subscriptions installed');
const queuedSignals = [...first.signals.values()];
const queuedOwner = [...first.handlers.values()][0];
extension._pressed = true;
extension.disable();
assert(first.cancelled, 'disabling during push-to-talk cancels recording');
assert(first.signals.size === 0 && first.handlers.size === 0, 'all subscriptions removed');
assert(first.disposed, 'proxy disposed');

// Late replies must also remain inert after this same extension object is re-enabled.
const second = begin(extension);
for (const {name, callback} of queuedSignals) {
    const args = name === 'StateChanged' ? ['{"state":"idle"}'] : ['agent', 'message'];
    callback(first, 'sender', args);
}
queuedOwner();
first.snapshotReply(['{"state":"idle"}'], null);
first.modeReply(null, new Error('late failure'));
first.cancelReply(null, new Error('late cancel failure'));
assert(notifications === 0, 'old callbacks must not notify or mutate a new lifetime');
assert(extension._proxy === second, 'old callbacks preserve the new proxy');

// Proxy initialization itself may finish after disable.
extension.disable();
second.ready(second, null);
assert(second.disposed && second.signals.size === 0, 'late initialization is disposed without subscriptions');
print('Extension lifecycle checks passed');
