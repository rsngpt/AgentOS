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

struct HelperConfig: Codable {
    var kernel: String
    var initramfs: String
    var cmdline: String
    var vcpus: Int
    var mem_mib: UInt64
    var vsock_port: UInt32
    var console_log: String
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

queue.async {
    vm.start { result in
        switch result {
        case .success:
            note("VM running")
            connectVsock(attempt: 1)
        case .failure(let error):
            fail("VM failed to start: \(error)")
        }
    }
}

RunLoop.main.run()
