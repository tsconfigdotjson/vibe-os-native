import Darwin
import Foundation
@preconcurrency import Virtualization

/// `vibe-vm create`, `boot` and `restore`.
public enum Runner {
    /// Gives the VM its identity, checks Virtualization.framework accepts
    /// it (save and restore included), and saves it. Prints a ``Created``
    /// line or a ``Failure``; returns the exit code.
    @MainActor
    public static func create(configPath: String) -> Int32 {
        do {
            var config = try VMConfig.load(path: configPath)
            try requireHost()
            if config.machineID == nil {
                config.machineID = VZBuilder.newMachineID()
            }
            if config.mac == nil {
                config.mac = randomMAC()
            }
            let vz = try VZBuilder.configuration(for: config, consoleLog: nil)
            try VZBuilder.validate(vz)
            try config.save(path: configPath)
            emit(Created(machineID: config.machineID ?? "", mac: config.mac ?? ""))
            return 0
        } catch {
            emit(error as? Failure ?? Failure(.badConfig, describe(error)))
            return 1
        }
    }

    /// Starts the VM (`.boot` fresh, `.restore` from its snapshot), prints
    /// a ``Ready`` line and serves the control socket until the VM stops.
    /// It detaches first: a session of its own, so vibed's end leaves it
    /// running.
    @MainActor
    public static func launch(
        _ mode: Started, configPath: String, socketPath: String, since processStart: ContinuousClock.Instant
    ) -> Never {
        _ = setsid()
        signal(SIGPIPE, SIG_IGN)
        Task { @MainActor in
            await run(mode, configPath: configPath, socketPath: socketPath, since: processStart)
        }
        dispatchMain()
    }

    @MainActor
    static func run(
        _ mode: Started, configPath: String, socketPath: String, since processStart: ContinuousClock.Instant
    ) async {
        let config: VMConfig
        let socket: ControlSocket
        let machine: VZMachine
        do {
            config = try VMConfig.load(path: configPath)
            guard config.machineID != nil, config.mac != nil else {
                throw Failure(.badConfig, "\(configPath) has no machine_id or mac; run `vibe-vm create` on it first")
            }
            try requireHost()
            socket = try ControlSocket.bind(path: socketPath)
            do {
                let vz = try VZBuilder.configuration(for: config, consoleLog: config.consoleLog)
                try VZBuilder.validate(vz)
                machine = VZMachine(configuration: vz)
            } catch {
                socket.remove()
                throw error
            }
        } catch {
            emit(error as? Failure ?? Failure(.badConfig, describe(error)))
            exit(1)
        }

        logMessage("\(mode.rawValue): \(config.ref), \(config.cpus) vCPUs, \(config.memory >> 20) MiB")
        let exiter = Exiter(socket: socket)
        let controller = Controller(config: config, machine: machine)
        machine.onStop = { error in
            if let code = controller.machineStopped(error: error) {
                exiter.exit(code)
            }
        }
        // Served from the start, so a probe during a long restore finds a
        // vibe-vm that is restoring, not one that doesn't answer.
        let server = ControlServer(
            socket: socket,
            handle: { request in await controller.handle(request) },
            closeStream: { id in await controller.closeStream(id) },
            exit: { code in exiter.exit(code) })
        server.start()

        let result: Result<Ready, Failure>
        switch mode {
        case .boot: result = await controller.boot(since: processStart)
        case .restore: result = await controller.restore(since: processStart)
        }
        guard case .success(let ready) = result else {
            if case .failure(let failure) = result {
                logMessage(failure.error)
                emit(failure)
            }
            exiter.exit(1)
            return
        }
        logMessage("running after \(ready.timings.totalMS ?? 0) ms")
        emit(ready)
        // vibed reads only the ready line and may close its end at any time;
        // nothing more goes to stdout.
        let null = open("/dev/null", O_WRONLY)
        if null >= 0 {
            dup2(null, STDOUT_FILENO)
            close(null)
        }
    }

    @MainActor
    static func requireHost() throws {
        let host = HostSupport.current()
        guard host.supported else {
            throw Failure(.unsupported, "Virtualization.framework can't run VMs on this Mac")
        }
        guard host.entitled else {
            throw Failure(.unsupported, "this vibe-vm isn't signed with the \(HostSupport.entitlement) entitlement")
        }
    }

    /// One line on stdout, for vibed.
    static func emit(_ value: some Encodable) {
        try? writeAll(STDOUT_FILENO, Wire.line(value))
    }
}

/// Ends vibe-vm, once, whichever gets there first: a reply that ends it, the
/// guest stopping the VM, a failed start. It exits on the main queue, not
/// on a server thread in the middle of Virtualization.framework's work, and
/// lets the socket's lock go first.
final class Exiter: @unchecked Sendable {
    private let lock = NSLock()
    private var exiting = false
    private let socket: ControlSocket

    init(socket: ControlSocket) {
        self.socket = socket
    }

    func exit(_ code: Int32) {
        lock.lock()
        defer { lock.unlock() }
        guard !exiting else { return }
        exiting = true
        let socket = self.socket
        DispatchQueue.main.async {
            socket.release()
            Darwin.exit(code)
        }
    }
}
