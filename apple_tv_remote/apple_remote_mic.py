#!/usr/bin/env python3
"""First-generation Siri Remote Opus microphone bridge for PipeWire."""
import argparse
import asyncio
import ctypes
import fcntl
import json
import os
from pathlib import Path
import pwd
import re
import signal
import socket
import struct
import sys
import subprocess

# Keep speech through Bluetooth bursts instead of silently dropping old frames.
MAX_PCM_BYTES = 120 * 16000 * 2


class AudioDrain:
    """Wait for late Bluetooth packets and the final PipeWire audio buffers."""
    def __init__(self, clock=None, sleep=asyncio.sleep):
        self.clock = clock or asyncio.get_running_loop().time
        self.sleep = sleep
        self.received_at = self.played_at = None
        self.writing = False

    async def wait(self, pcm_queue):
        released_at = self.clock()
        while True:
            now = self.clock()
            received_at = max(released_at, self.received_at or released_at)
            played_at = self.played_at if self.played_at is not None else released_at
            if (not pcm_queue and not self.writing and
                    now - received_at >= 0.5 and now - played_at >= 0.2):
                return
            if now - released_at >= 10:
                raise RuntimeError('Remote audio did not drain; recording cancelled')
            await self.sleep(0.02)


def take_pcm_frame(pcm_queue):
    frame = bytes(pcm_queue[:640])
    del pcm_queue[:len(frame)]
    return frame + bytes(640 - len(frame))


async def run_command(*args):
    process = await asyncio.create_subprocess_exec(
        *args, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    try:
        out, err = await asyncio.wait_for(process.communicate(), 5)
    except BaseException:
        if process.returncode is None:
            process.kill()
        await process.wait()
        raise
    if process.returncode:
        raise RuntimeError(f'{args[0]} failed: {err.decode().strip()}')
    return out.decode()


class SiriDictation:
    """Control Orchard recording from Siri while preserving the usual microphone."""
    def __init__(self, command=run_command):
        self.command = command
        self.owned = False
        self.previous_source = None
        self.events = asyncio.Queue()

    async def source(self):
        info = await self.command('wpctl', 'inspect', '@DEFAULT_AUDIO_SOURCE@')
        match = re.search(r'node.name = "([^"]+)"', info)
        if not match:
            raise RuntimeError('Cannot identify the default microphone')
        return match[1]

    async def select(self, name):
        nodes = json.loads(await self.command('pw-dump'))
        for node in nodes:
            if node.get('info', {}).get('props', {}).get('node.name') == name:
                await self.command('wpctl', 'set-default', str(node['id']))
                return
        raise RuntimeError(f'Microphone unavailable: {name}')

    async def restore(self):
        if self.previous_source is not None:
            # Preserve a manual input change made while dictating.
            if await self.source() == 'apple_remote_microphone':
                await self.select(self.previous_source)
            self.previous_source = None

    async def start(self):
        status = json.loads(await self.command('orchrdctl', 'status'))
        if status.get('voice_state') != 'idle':
            print('Siri recording skipped: Orchard is busy', flush=True)
            return
        self.previous_source = await self.source()
        try:
            await self.select('apple_remote_microphone')
            await self.command('orchrdctl', 'start')
            self.owned = True
            print('Orchard recording from Siri', flush=True)
        except BaseException:
            await self.restore()
            raise

    async def finish(self, cancel=False):
        try:
            if self.owned:
                await self.command('orchrdctl', 'cancel' if cancel else 'stop')
                self.owned = False
        finally:
            await self.restore()

    async def run(self, pcm_queue, audio_drain):
        while True:
            down = await self.events.get()
            try:
                if down:
                    if not self.owned:
                        await self.start()
                elif self.owned:
                    released_at = audio_drain.clock()
                    queued_ms = len(pcm_queue) / 32
                    await audio_drain.wait(pcm_queue)
                    print(f'Siri audio drained: queued={queued_ms:.0f}ms '
                          f'wait={audio_drain.clock() - released_at:.2f}s', flush=True)
                    await self.finish()
            except Exception as error:
                print(f'Siri dictation: {error}', file=sys.stderr, flush=True)
                if self.owned:
                    await self.finish(cancel=True)
                    pcm_queue.clear()


class ATTAssembler:
    def __init__(self):
        self.pending = {}

    def feed(self, packet):
        if len(packet) < 4:
            return None
        flags, length = struct.unpack_from('<HH', packet)
        handle, boundary = flags & 4095, (flags >> 12) & 3
        if len(packet) != length + 4:
            return None
        data = packet[4:]
        if boundary in (0, 2):
            self.pending[handle] = bytearray(data)
        elif boundary == 1 and handle in self.pending:
            self.pending[handle].extend(data)
        else:
            return None
        data = self.pending[handle]
        if len(data) < 4:
            return None
        size, cid = struct.unpack_from('<HH', data)
        if len(data) < size + 4:
            return None
        del self.pending[handle]
        if len(data) != size + 4 or cid != 4 or data[4:7] != b'\x1b\x23\x00':
            return None
        return bytes(data[7:])


def opus_packet(report):
    # Gen 1: flags, buttons, two status bytes, u16 sequence, length, Opus.
    if len(report) < 8 or report[0] != 0x10:
        return None
    length = report[6]
    if not length or len(report) < 7 + length:
        return None
    return int.from_bytes(report[4:6], 'little'), report[7:7 + length]


class OpusDecoder:
    def __init__(self):
        self.lib = ctypes.CDLL(os.environ.get('APPLE_OPUS_LIBRARY', 'libopus.so.0'))
        self.lib.opus_decoder_create.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.POINTER(ctypes.c_int)]
        self.lib.opus_decoder_create.restype = ctypes.c_void_p
        self.lib.opus_decode.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int,
                                        ctypes.POINTER(ctypes.c_int16), ctypes.c_int, ctypes.c_int]
        self.lib.opus_decode.restype = ctypes.c_int
        self.lib.opus_decoder_destroy.argtypes = [ctypes.c_void_p]
        error = ctypes.c_int()
        self.decoder = self.lib.opus_decoder_create(16000, 1, ctypes.byref(error))
        if error.value or not self.decoder:
            raise RuntimeError(f'Opus initialization failed: {error.value}')
        self.buffer = (ctypes.c_int16 * 1920)()

    def decode(self, packet, frame_size=1920):
        size = self.lib.opus_decode(self.decoder, packet, len(packet), self.buffer, frame_size, 0)
        if size < 0:
            raise ValueError(f'Invalid Opus packet: {size}')
        return bytes(self.buffer)[:size * 2]

    def conceal(self, frame_size=320):
        """Ask Opus to synthesize one missing frame from decoder history."""
        size = self.lib.opus_decode(self.decoder, None, 0, self.buffer, frame_size, 0)
        if size < 0:
            raise ValueError(f'Opus loss concealment failed: {size}')
        return bytes(self.buffer)[:size * 2]

    def reset(self):
        # Reset prediction between separate push-to-talk sessions.
        self.lib.opus_decoder_ctl.argtypes = [ctypes.c_void_p, ctypes.c_int]
        self.lib.opus_decoder_ctl(self.decoder, 4028)  # OPUS_RESET_STATE

    def close(self):
        if self.decoder:
            self.lib.opus_decoder_destroy(self.decoder)
            self.decoder = None


def capture_reports(path):
    assembler = ATTAssembler()
    with open(path, 'rb') as stream:
        header = stream.read(16)
        if header != b'btsnoop\x00' + struct.pack('>II', 1, 2001):
            raise ValueError('Expected BlueZ monitor btsnoop format')
        while record := stream.read(24):
            if len(record) < 24:
                break
            original, included, flags, drops, stamp = struct.unpack('>IIIIQ', record)
            data = stream.read(included)
            if len(data) != included:
                break
            if flags & 65535 == 5:
                report = assembler.feed(data)
                if report is not None:
                    yield report


def le_connection_handle(event, wanted_address):
    """Return the handle when an LE connection event belongs to our remote."""
    if len(event) < 14 or event[0] != 0x3e or event[2] not in (0x01, 0x0a):
        return None
    if event[3] != 0 or event[8:14] != wanted_address:
        return None
    return struct.unpack_from('<H', event, 4)[0] & 4095


def inspect(path):
    from collections import Counter
    decoder = OpusDecoder()
    counts, samples, peak, errors = Counter(), 0, 0, 0
    try:
        for report in capture_reports(path):
            counts[len(report)] += 1
            packet = opus_packet(report)
            if packet:
                try:
                    pcm = decoder.decode(packet[1])
                    samples += len(pcm) // 2
                    peak = max(peak, max(abs(s[0]) for s in struct.iter_unpack('<h', pcm)))
                except ValueError:
                    errors += 1
        print(json.dumps(dict(lengths=dict(counts), decoded_seconds=samples / 16000,
                              peak=peak, errors=errors)))
    finally:
        decoder.close()


def monitor(mac):
    """Open privileged HCI monitor, then drop root before parsing traffic."""
    uid = int(os.environ.get('PKEXEC_UID', os.getuid()))
    raw = socket.socket(socket.AF_BLUETOOTH, socket.SOCK_RAW, socket.BTPROTO_HCI)
    request = bytearray(struct.pack('<HH', 0, 64) + bytes(64 * 16))
    fcntl.ioctl(raw, 0x800448D4, request, True)  # HCIGETCONNLIST
    wanted = bytes.fromhex(mac.replace(':', ''))[::-1]
    handle = None
    for offset in range(4, 4 + struct.unpack_from('<H', request, 2)[0] * 16, 16):
        if request[offset + 2:offset + 8] == wanted:
            handle = struct.unpack_from('<H', request, offset)[0]
            break
    raw.close()
    capture = socket.socket(socket.AF_BLUETOOTH, socket.SOCK_RAW, socket.BTPROTO_HCI)
    # Python 3.13's HCI bind wrapper only supports the raw-channel tuple.
    # Bind sockaddr_hci directly to select HCI_CHANNEL_MONITOR.
    libc = ctypes.CDLL(None, use_errno=True)
    libc.bind.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint]
    libc.bind.restype = ctypes.c_int
    address = ctypes.create_string_buffer(struct.pack('HHH', socket.AF_BLUETOOTH, 65535, 2))
    if libc.bind(capture.fileno(), address, 6) != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))
    # Service mode runs as the desktop user with only Bluetooth capabilities.
    # Interactive mode still drops pkexec's root identity before capture.
    def tune_link(connection):
        if os.environ.get('APPLE_REMOTE_TUNE_LINK') != '1':
            return
        try:
            result = subprocess.run(
                ['hcitool', 'lecup', f'--handle={connection}', '--min=6',
                 '--max=12', '--latency=0', '--timeout=200'],
                capture_output=True, timeout=4, text=True)
            print(f'Bluetooth fast interval request: {result.returncode} '
                  f'{result.stderr.strip()}', file=sys.stderr, flush=True)
        except (OSError, subprocess.TimeoutExpired) as error:
            print(f'Bluetooth interval update failed: {error}', file=sys.stderr, flush=True)

    if os.geteuid() == 0:
        os.setgroups([])
        os.setgid(pwd.getpwuid(uid).pw_gid)
        os.setuid(uid)
    if handle is not None:
        tune_link(handle)
    assembler = ATTAssembler()
    if handle is None:
        print('Remote is asleep; waiting for connection (root privileges dropped)',
              file=sys.stderr, flush=True)
    else:
        print(f'Monitoring remote connection {handle}; root privileges dropped',
              file=sys.stderr, flush=True)
    try:
        while True:
            data = capture.recv(65541)
            if len(data) < 6:
                continue
            opcode, index, length = struct.unpack_from('<HHH', data)
            payload = data[6:]
            if index != 0 or len(payload) != length:
                continue
            if opcode == 3:
                if payload[:1] == b'\x05' and len(payload) >= 6:
                    if struct.unpack_from('<H', payload, 3)[0] & 4095 == handle:
                        print('Remote disconnected; waiting for reconnection', file=sys.stderr, flush=True)
                        handle = None
                        assembler = ATTAssembler()
                    continue
                connected = le_connection_handle(payload, wanted)
                if connected is not None:
                    handle = connected
                    assembler = ATTAssembler()
                    print(f'Remote reconnected on handle {handle}', file=sys.stderr, flush=True)
                    tune_link(handle)
                # Report the actual negotiated interval, rather than just
                # claiming success when the update command is accepted.
                if (len(payload) >= 12 and payload[:1] == b'\x3e'
                        and payload[2] == 3
                        and struct.unpack_from('<H', payload, 4)[0] == handle):
                    interval = struct.unpack_from('<H', payload, 6)[0] * 1.25
                    print(f'Bluetooth interval: {interval:g} ms (status {payload[3]})',
                          file=sys.stderr, flush=True)
                continue
            if (opcode != 5 or handle is None or len(payload) < 4 or
                    struct.unpack_from('<H', payload)[0] & 4095 != handle):
                continue
            report = assembler.feed(payload)
            if report and (report[0] == 0x10 or len(report) == 2):
                sys.stdout.buffer.write(struct.pack('<H', len(report)) + report)
                sys.stdout.buffer.flush()
    finally:
        capture.close()


async def bridge(args):
    runtime = Path(os.environ.get('XDG_RUNTIME_DIR', '/tmp'))
    lock_fd = os.open(runtime / f'apple-remote-mic-{os.getuid()}.lock',
                      os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        os.close(lock_fd)
        raise RuntimeError('An Apple remote microphone bridge is already running')
    decoder = OpusDecoder()
    monitor_process = player = None
    tasks = []
    loop = asyncio.get_running_loop()
    stop = asyncio.Event()
    for sig in (signal.SIGINT, signal.SIGTERM):
        loop.add_signal_handler(sig, stop.set)
    pcm_queue = bytearray()
    audio_drain = AudioDrain()
    stats = dict(frames=0, errors=0, sequence_skips=0, concealed=0,
                 underruns=0, peak=0)
    playout = dict(started=False, receiving=False)
    dictation = SiriDictation() if args.orchrd else None
    try:
        monitor_process = await asyncio.create_subprocess_exec(
            *([] if args.direct_monitor else ['pkexec']),
            sys.executable, str(Path(__file__).resolve()),
            '--monitor', '--mac', args.mac, stdout=asyncio.subprocess.PIPE,
            start_new_session=True)
        properties = json.dumps({'media.class': 'Audio/Source',
            'node.name': 'apple_remote_microphone',
            'node.description': 'Apple TV Remote Microphone',
            'node.virtual': True, 'node.want-driver': True,
            'priority.session': 1, 'node.pause-on-idle': False})
        player = await asyncio.create_subprocess_exec(
            'pw-cat', '--playback', '--raw', '--rate', '16000', '--channels', '1',
            '--format', 's16', '--target', '0', '--latency', '40ms',
            '--properties', properties, '-', stdin=asyncio.subprocess.PIPE)

        async def receive():
            previous = None
            previous_arrival = None
            pressed = False
            while True:
                try:
                    prefix = await monitor_process.stdout.readexactly(2)
                except asyncio.IncompleteReadError:
                    raise RuntimeError('Bluetooth reader stopped; check authentication and remote connection') from None
                length = struct.unpack('<H', prefix)[0]
                if not 0 < length <= 512:
                    raise RuntimeError('Invalid monitor frame length')
                report = await monitor_process.stdout.readexactly(length)
                if len(report) == 2:
                    down = bool(report[1] & 0x10)
                    if down and not pressed:
                        decoder.reset()
                        previous = None
                        previous_arrival = None
                        playout['started'] = False
                        playout['receiving'] = True
                        print('Siri pressed: receiving audio', flush=True)
                        if dictation:
                            dictation.events.put_nowait(True)
                    if not down and pressed:
                        playout['receiving'] = False
                        print(f'Siri released: {stats}', flush=True)
                        if dictation:
                            dictation.events.put_nowait(False)
                    pressed = down
                    continue
                packet = opus_packet(report)
                if packet is None:
                    stats['errors'] += 1
                    continue
                sequence, payload = packet
                arrival = loop.time()
                if previous is not None:
                    difference = (sequence - previous) & 65535
                    if difference == 0:
                        continue
                    if difference > 32768:
                        decoder.reset()
                    elif difference > 1:
                        sequence_skips = difference - 1
                        stats['sequence_skips'] += sequence_skips
                        # The remote's counter can advance without a missing
                        # audio interval. Only conceal skips corroborated by
                        # elapsed delivery time, otherwise speech is stretched.
                        elapsed = arrival - previous_arrival
                        timed_skips = max(0, int((elapsed + 0.010) / 0.020) - 1)
                        for _ in range(min(sequence_skips, timed_skips, 10)):
                            try:
                                pcm_queue.extend(decoder.conceal())
                                stats['concealed'] += 1
                            except ValueError:
                                stats['errors'] += 1
                                pcm_queue.extend(bytes(640))
                previous = sequence
                previous_arrival = arrival
                try:
                    pcm = decoder.decode(payload)
                except ValueError:
                    stats['errors'] += 1
                    continue
                stats['frames'] += 1
                stats['peak'] = max(stats['peak'], max(abs(s[0]) for s in struct.iter_unpack('<h', pcm)))
                pcm_queue.extend(pcm)
                audio_drain.received_at = loop.time()
                if len(pcm_queue) > MAX_PCM_BYTES:
                    raise RuntimeError('Remote audio backlog exceeds the two-minute limit')

        async def send():
            # Deliver 20 ms continuously. Holding six frames before starting
            # absorbs the remote's bursty Bluetooth delivery with modest latency.
            deadline = loop.time()
            while True:
                if not playout['started'] and (len(pcm_queue) >= 3840 or
                        (pcm_queue and not playout['receiving'])):
                    playout['started'] = True
                has_audio = playout['started'] and bool(pcm_queue)
                if has_audio:
                    frame = take_pcm_frame(pcm_queue)
                else:
                    frame = bytes(640)
                    if playout['started'] and playout['receiving']:
                        stats['underruns'] += 1
                audio_drain.writing = has_audio
                player.stdin.write(frame)
                await player.stdin.drain()
                if has_audio:
                    audio_drain.played_at = loop.time()
                audio_drain.writing = False
                deadline += 0.020
                await asyncio.sleep(max(0, deadline - loop.time()))
                if loop.time() > deadline + 0.1:
                    deadline = loop.time()

        tasks = [asyncio.create_task(receive()), asyncio.create_task(send()),
                 asyncio.create_task(stop.wait()), asyncio.create_task(player.wait())]
        if dictation:
            tasks.append(asyncio.create_task(dictation.run(pcm_queue, audio_drain)))
        print('Apple TV Remote Microphone created. Hold Siri to speak. '
              'Raw capture may request system authentication.', flush=True)
        done, _ = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        for task in done:
            if task is not tasks[2]:
                task.result()
                if not stop.is_set():
                    raise RuntimeError('Microphone transport stopped')
    finally:
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        if dictation:
            try:
                await dictation.finish(cancel=True)
            except Exception as error:
                print(f'Dictation cleanup: {error}', file=sys.stderr)
        for proc in (player, monitor_process):
            if proc is not None and proc.returncode is None:
                try:
                    proc.terminate()
                    await asyncio.wait_for(proc.wait(), 2)
                except (ProcessLookupError, PermissionError, TimeoutError):
                    if proc is monitor_process and proc.stdout:
                        proc.stdout.feed_eof()
        decoder.close()
        os.close(lock_fd)
        print(f'Microphone stopped: {stats}', flush=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--mac', default='28:EC:95:12:B9:E5')
    parser.add_argument('--monitor', action='store_true')
    parser.add_argument('--direct-monitor', action='store_true',
                        help='Service mode: use supplied Bluetooth capabilities')
    parser.add_argument('--inspect-capture')
    parser.add_argument('--orchrd', action=argparse.BooleanOptionalAction, default=True,
                        help='Send Siri speech to Orchard (default: enabled)')
    args = parser.parse_args()
    if args.monitor:
        monitor(args.mac)
    elif args.inspect_capture:
        inspect(args.inspect_capture)
    else:
        asyncio.run(bridge(args))

if __name__ == '__main__':
    main()
