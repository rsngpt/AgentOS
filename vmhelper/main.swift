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
    /// Where SIGHUP writes the saved VM state (snapshot).
    var save_path: String?
    /// Persisted machine identity. A restore only accepts a configuration
    /// whose machineIdentifier matches the saved VM's, and the identifier is
    /// otherwise regenerated at random on every launch.
    var machine_id_path: String?
    /// When set, boot by restoring this saved state instead of starting fresh.
    var restore_path: String?
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

// Machine identity must survive save -> restore: VZ rejects a restore whose
// machineIdentifier differs from the saved VM's ("invalid argument"), and a
// fresh random one is generated per launch unless we pin it. Persist it in
// the sandbox dir on first boot and reuse it forever after.
if #available(macOS 13.0, *), let idPath = config.machine_id_path {
    let platform = VZGenericPlatformConfiguration()
    if let data = FileManager.default.contents(atPath: idPath),
       let saved = VZGenericMachineIdentifier(dataRepresentation: data) {
        platform.machineIdentifier = saved
    } else {
        let fresh = VZGenericMachineIdentifier()
        platform.machineIdentifier = fresh
        do {
            try fresh.dataRepresentation.write(to: URL(fileURLWithPath: idPath))
        } catch {
            note("could not persist machine identifier: \(error) — restore will fail")
        }
    }
    vmConfig.platform = platform
}
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
        // Our stdin is the daemon's pipe, so EOF means the daemon is gone —
        // crashed, killed, or shut down. A sandbox VM must never outlive its
        // supervisor: exiting here destroys the VM, which is the same
        // fail-closed guarantee the kill switch relies on.
        shutdown(connFd, SHUT_WR)
        note("daemon disconnected; destroying VM")
        exit(0)
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
/// The daemon's control connection, so a snapshot can close it first: a live
/// virtio-socket connection cannot be captured in saved VM state.
var controlConnection: VZVirtioSocketConnection?

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
            // Keep the connection object alive for the process lifetime, and
            // reachable so a snapshot can tear it down before saving.
            objc_setAssociatedObject(vm, "agentos.conn", conn, .OBJC_ASSOCIATION_RETAIN)
            controlConnection = conn
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
// Default dispositions must be disabled or the process dies before the
// dispatch sources below ever see the signal.
signal(SIGUSR1, SIG_IGN)
signal(SIGUSR2, SIG_IGN)
signal(SIGHUP, SIG_IGN)
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

// SIGHUP = snapshot: pause (save requires it), write the state file, exit.
// The process dying destroys the VM, which is fine — its state is on disk.
let saveSource = DispatchSource.makeSignalSource(signal: SIGHUP, queue: queue)
saveSource.setEventHandler {
    guard let savePath = config.save_path else {
        note("snapshot requested but no save_path configured")
        return
    }
    #if arch(arm64)
    guard #available(macOS 14.0, *) else {
        note("snapshot requires macOS 14+")
        return
    }
    func writeState() {
        // Live virtio-socket state can't be captured in a saved VM: tear the
        // control connection and proxy listener down first. The guest agent
        // re-listens after the restore.
        controlConnection?.close()
        controlConnection = nil
        if let listener = proxyListener,
           let dev = vm.socketDevices.first as? VZVirtioSocketDevice,
           let port = config.proxy_port {
            dev.removeSocketListener(forPort: port)
            _ = listener
            proxyListener = nil
        }
        // saveMachineStateTo will not overwrite, so clear any earlier snapshot
        // (re-snapshotting a restored sandbox is normal).
        try? FileManager.default.removeItem(atPath: savePath)
        vm.saveMachineStateTo(url: URL(fileURLWithPath: savePath)) { error in
            if let error {
                fail("saving VM state: \(error)")
            }
            note("state saved to \(savePath)")
            exit(0)
        }
    }
    if vm.canPause {
        vm.pause { result in
            switch result {
            case .success: writeState()
            case .failure(let e): fail("pause before save: \(e)")
            }
        }
    } else {
        writeState() // already paused
    }
    #else
    note("snapshot is arm64-only")
    #endif
}
saveSource.resume()

/// Everything that must happen once the guest is executing, whether it was
/// booted fresh or restored from a snapshot.
@Sendable func guestIsRunning(_ how: String) {
    note("VM running (\(how))")
    installProxyListener()
    connectVsock(attempt: 1)
}

queue.async {
    if let restorePath = config.restore_path {
        #if arch(arm64)
        if #available(macOS 14.0, *) {
            // Restore requires a stopped VM and leaves it paused.
            vm.restoreMachineStateFrom(url: URL(fileURLWithPath: restorePath)) { error in
                if let error {
                    fail("restoring VM state from \(restorePath): \(error)")
                }
                vm.resume { result in
                    switch result {
                    case .success: guestIsRunning("restored")
                    case .failure(let e): fail("resume after restore: \(e)")
                    }
                }
            }
        } else {
            fail("restore requires macOS 14+")
        }
        #else
        fail("restore is arm64-only")
        #endif
    } else {
        vm.start { result in
            switch result {
            case .success: guestIsRunning("fresh boot")
            case .failure(let error):
                fail("VM failed to start: \(error)")
            }
        }
    }
}

RunLoop.main.run()
