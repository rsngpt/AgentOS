// agentos-vmhelper — the per-sandbox VM child process on macOS.
//
// The daemon spawns one of these per microVM. Design contract
// (ARCHITECTURE.md §3): this process *is* the VM — SIGKILLing it destroys
// the VM with no cooperation needed, which is what makes the kill switch
// absolute.
//
// Usage: agentos-vmhelper <config.json>
//
// Behavior:
//   1. Boot a Linux microVM (direct kernel boot, no network device).
//   2. Connect to the guest agent's vsock port (retrying while it boots).
//   3. Relay bytes: our stdin -> vsock, vsock -> our stdout. The daemon
//      speaks the framed control protocol over these pipes; we are a dumb pipe.
//   4. Guest console output goes to the config's console_log file.
//   5. Exit when the VM stops (guest powered off) or on any error.

import Foundation
import Virtualization

struct MountConfig: Codable {
    var tag: String
    var host_path: String
    var read_only: Bool
}

struct DiskConfig: Codable {
    var path: String
    var read_only: Bool
}

struct HelperConfig: Codable {
    var kernel: String
    var initramfs: String
    var cmdline: String
    var vcpus: Int
    var mem_mib: UInt64
    var vsock_port: UInt32
    var console_log: String
    /// virtio-fs shares; read_only is enforced here, host-side.
    var mounts: [MountConfig]?
    /// virtio-blk disks in guest order (vda, vdb, …); rootfs is read-only.
    var disks: [DiskConfig]?
    /// When set, guest connections to vsock port `proxy_port` are bridged to
    /// this Unix socket (the daemon's per-sandbox egress proxy).
    var proxy_socket: String?
    var proxy_port: UInt32?
}

func fail(_ msg: String) -> Never {
    FileHandle.standardError.write("[vmhelper] \(msg)\n".data(using: .utf8)!)
    exit(70)
}

func note(_ msg: String) {
    FileHandle.standardError.write("[vmhelper] \(msg)\n".data(using: .utf8)!)
}

guard CommandLine.arguments.count == 2 else {
    fail("usage: agentos-vmhelper <config.json>")
}

let config: HelperConfig
do {
    let data = try Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[1]))
    config = try JSONDecoder().decode(HelperConfig.self, from: data)
} catch {
    fail("bad config: \(error)")
}

// --- VM configuration -------------------------------------------------------

let bootLoader = VZLinuxBootLoader(kernelURL: URL(fileURLWithPath: config.kernel))
bootLoader.initialRamdiskURL = URL(fileURLWithPath: config.initramfs)
bootLoader.commandLine = config.cmdline

let vmConfig = VZVirtualMachineConfiguration()
vmConfig.bootLoader = bootLoader
vmConfig.cpuCount = max(1, min(config.vcpus, VZVirtualMachineConfiguration.maximumAllowedCPUCount))
vmConfig.memorySize = max(VZVirtualMachineConfiguration.minimumAllowedMemorySize,
                          config.mem_mib * 1024 * 1024)

// vsock is the guest's only I/O channel: no network device is ever attached.
vmConfig.socketDevices = [VZVirtioSocketDeviceConfiguration()]
vmConfig.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]

// virtio-fs shares. Read-only is enforced by VZSharedDirectory on the host:
// nothing the (root-privileged) guest does can upgrade its access.
var fsDevices: [VZDirectorySharingDeviceConfiguration] = []
for m in config.mounts ?? [] {
    let dev = VZVirtioFileSystemDeviceConfiguration(tag: m.tag)
    let dir = VZSharedDirectory(url: URL(fileURLWithPath: m.host_path), readOnly: m.read_only)
    dev.share = VZSingleDirectoryShare(directory: dir)
    fsDevices.append(dev)
}
vmConfig.directorySharingDevices = fsDevices

// virtio-blk disks. Order determines guest device names (vda, vdb, …); the
// guest agent expects vda=rootfs, vdb=overlay. Read-only enforced host-side.
var blockDevices: [VZStorageDeviceConfiguration] = []
for d in config.disks ?? [] {
    do {
        let attachment = try VZDiskImageStorageDeviceAttachment(
            url: URL(fileURLWithPath: d.path), readOnly: d.read_only)
        blockDevices.append(VZVirtioBlockDeviceConfiguration(attachment: attachment))
    } catch {
        fail("cannot attach disk \(d.path): \(error)")
    }
}
vmConfig.storageDevices = blockDevices

// Guest console -> log file (for debugging boot problems).
FileManager.default.createFile(atPath: config.console_log, contents: nil)
guard let consoleOut = FileHandle(forWritingAtPath: config.console_log) else {
    fail("cannot open console log \(config.console_log)")
}
let consolePort = VZVirtioConsoleDeviceSerialPortConfiguration()
consolePort.attachment = VZFileHandleSerialPortAttachment(
    fileHandleForReading: nil, fileHandleForWriting: consoleOut)
vmConfig.serialPorts = [consolePort]

do {
    try vmConfig.validate()
} catch {
    fail("invalid VM configuration: \(error)")
}

// Report whether this configuration supports save/restore (snapshotting).
// Not all device combinations do, and the API only tells us at runtime.
#if arch(arm64)
if #available(macOS 14.0, *) {
    do {
        try vmConfig.validateSaveRestoreSupport()
        note("save/restore: SUPPORTED for this configuration")
    } catch {
        note("save/restore: UNSUPPORTED — \(error)")
    }
}
#endif

// --- Boot and relay ---------------------------------------------------------

final class Delegate: NSObject, VZVirtualMachineDelegate {
    func guestDidStop(_ vm: VZVirtualMachine) {
        note("guest powered off")
        exit(0)
    }
    func virtualMachine(_ vm: VZVirtualMachine, didStopWithError error: Error) {
        fail("VM stopped with error: \(error)")
    }
}

let queue = DispatchQueue.main
let vm = VZVirtualMachine(configuration: vmConfig, queue: queue)
let delegate = Delegate()
vm.delegate = delegate

/// Copy bytes from fd `src` to fd `dst` until EOF/error. Returns on EOF.
@Sendable func relayLoop(_ src: Int32, _ dst: Int32) {
    var buf = [UInt8](repeating: 0, count: 32 * 1024)
    outer: while true {
        let n = buf.withUnsafeMutableBytes { read(src, $0.baseAddress, $0.count) }
        if n <= 0 { break }
        var off = 0
        while off < n {
            let w = buf.withUnsafeBytes { write(dst, $0.baseAddress!.advanced(by: off), n - off) }
            if w <= 0 { break outer }
            off += w
        }
    }
}

/// Blocking two-way byte relay between our stdio and the vsock connection fd.
@Sendable func startRelay(connFd: Int32) {
    Thread.detachNewThread {
        relayLoop(0, connFd)
        // stdin closed: daemon went away; half-close towards the guest.
        shutdown(connFd, SHUT_WR)
    }
    Thread.detachNewThread {
        relayLoop(connFd, 1)
    }
}

/// Bridges guest-initiated vsock connections (egress proxy traffic) to the
/// daemon's per-sandbox Unix socket. One UDS connection per guest connection.
final class ProxyBridge: NSObject, VZVirtioSocketListenerDelegate {
    let udsPath: String
    init(udsPath: String) { self.udsPath = udsPath }

    func listener(
        _ listener: VZVirtioSocketListener,
        shouldAcceptNewConnection connection: VZVirtioSocketConnection,
        from socketDevice: VZVirtioSocketDevice
    ) -> Bool {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return false }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let ok = udsPath.withCString { cstr -> Bool in
            withUnsafeMutableBytes(of: &addr.sun_path) { raw in
                let n = strlen(cstr)
                guard n < raw.count else { return false }
                raw.baseAddress!.copyMemory(from: cstr, byteCount: n + 1)
                return true
            }
        }
        guard ok else { close(fd); return false }
        let rc = withUnsafePointer(to: &addr) { p in
            p.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard rc == 0 else {
            note("proxy bridge: cannot reach daemon socket \(udsPath)")
            close(fd)
            return false
        }
        // dup the vsock fd so the connection object's lifetime doesn't matter.
        let vfd = dup(connection.fileDescriptor)
        connection.close()
        Thread.detachNewThread {
            relayLoop(vfd, fd)
            shutdown(fd, SHUT_WR)
        }
        Thread.detachNewThread {
            relayLoop(fd, vfd)
            shutdown(vfd, SHUT_WR)
            // Both directions done when the reverse thread also exits; closing
            // here after our direction guarantees no fd leak per connection.
            close(fd)
            close(vfd)
        }
        return true
    }
}

var proxyBridge: ProxyBridge? // must outlive the VM
var proxyListener: VZVirtioSocketListener?

func installProxyListener() {
    guard let udsPath = config.proxy_socket, let port = config.proxy_port else { return }
    guard let socketDevice = vm.socketDevices.first as? VZVirtioSocketDevice else { return }
    let bridge = ProxyBridge(udsPath: udsPath)
    let listener = VZVirtioSocketListener()
    listener.delegate = bridge
    socketDevice.setSocketListener(listener, forPort: port)
    proxyBridge = bridge
    proxyListener = listener
    note("proxy bridge on vsock port \(port) -> \(udsPath)")
}

func connectVsock(attempt: Int) {
    guard let socketDevice = vm.socketDevices.first as? VZVirtioSocketDevice else {
        fail("no vsock device on running VM")
    }
    socketDevice.connect(toPort: config.vsock_port) { result in
        switch result {
        case .success(let conn):
            note("vsock connected (attempt \(attempt))")
            // Keep the connection object alive for the process lifetime.
            objc_setAssociatedObject(vm, "agentos.conn", conn, .OBJC_ASSOCIATION_RETAIN)
            startRelay(connFd: conn.fileDescriptor)
        case .failure:
            if attempt >= 100 {
                fail("guest agent never opened vsock port \(config.vsock_port)")
            }
            queue.asyncAfter(deadline: .now() + .milliseconds(100)) {
                connectVsock(attempt: attempt + 1)
            }
        }
    }
}

// Pause/resume are driven by signals rather than a control message, because
// our stdio is already dedicated to relaying the guest's vsock stream.
// SIGUSR1 = pause, SIGUSR2 = resume. Dispatch sources run the handler on the
// VM queue, which is where Virtualization.framework requires these calls.
signal(SIGUSR1, SIG_IGN)
signal(SIGUSR2, SIG_IGN)
let pauseSource = DispatchSource.makeSignalSource(signal: SIGUSR1, queue: queue)
pauseSource.setEventHandler {
    guard vm.canPause else {
        note("pause ignored: VM cannot pause in state \(vm.state.rawValue)")
        return
    }
    vm.pause { result in
        switch result {
        case .success: note("paused")
        case .failure(let e): note("pause failed: \(e)")
        }
    }
}
pauseSource.resume()

let resumeSource = DispatchSource.makeSignalSource(signal: SIGUSR2, queue: queue)
resumeSource.setEventHandler {
    guard vm.canResume else {
        note("resume ignored: VM cannot resume in state \(vm.state.rawValue)")
        return
    }
    vm.resume { result in
        switch result {
        case .success: note("resumed")
        case .failure(let e): note("resume failed: \(e)")
        }
    }
}
resumeSource.resume()

queue.async {
    vm.start { result in
        switch result {
        case .success:
            note("VM running")
            installProxyListener()
            connectVsock(attempt: 1)
        case .failure(let error):
            fail("VM failed to start: \(error)")
        }
    }
}

RunLoop.main.run()
