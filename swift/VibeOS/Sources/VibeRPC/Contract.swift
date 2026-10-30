import Foundation

// The Swift side of the vibed JSON-RPC 2.0 contract. protocol/vibed-rpc.json
// and protocol/examples/ are the source of truth; VibeRPCTests checks these
// types against them, as the Rust contract tests check vibe_core::rpc.

public enum VibeRPC {
    /// Sent in `daemon.hello`; vibed refuses any other.
    public static let protocolVersion = 1
    public static let jsonrpc = "2.0"
    /// The longest message, newline excluded, either side accepts.
    public static let maxMessageBytes = 16 * 1024 * 1024
    /// Overrides the socket path.
    public static let socketEnv = "VIBE_SOCKET"
    /// The socket path relative to the home directory on macOS.
    public static let macOSSocket = "Library/Application Support/VibeOS/run/vibed.sock"
    /// Every method in the contract.
    public static let methods = [Hello.name]
}

/// A request id. `.null` is only sent by vibed, for a request whose id it
/// couldn't read.
public enum RPCID: Codable, Hashable, Sendable {
    case number(Int64)
    case string(String)
    case null

    public init(from decoder: any Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() {
            self = .null
        } else if let n = try? c.decode(Int64.self) {
            self = .number(n)
        } else if let s = try? c.decode(String.self) {
            self = .string(s)
        } else {
            throw DecodingError.dataCorruptedError(
                in: c, debugDescription: "an id is an integer, a string or null")
        }
    }

    public func encode(to encoder: any Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .number(let n): try c.encode(n)
        case .string(let s): try c.encode(s)
        case .null: try c.encodeNil()
        }
    }
}

/// Any JSON value, for error data and untyped params.
public enum JSONValue: Codable, Hashable, Sendable {
    case null
    case bool(Bool)
    case number(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])

    public init(from decoder: any Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() {
            self = .null
        } else if let b = try? c.decode(Bool.self) {
            self = .bool(b)
        } else if let n = try? c.decode(Double.self) {
            self = .number(n)
        } else if let s = try? c.decode(String.self) {
            self = .string(s)
        } else if let a = try? c.decode([JSONValue].self) {
            self = .array(a)
        } else {
            self = .object(try c.decode([String: JSONValue].self))
        }
    }

    public func encode(to encoder: any Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .null: try c.encodeNil()
        case .bool(let b): try c.encode(b)
        case .number(let n): try c.encode(n)
        case .string(let s): try c.encode(s)
        case .array(let a): try c.encode(a)
        case .object(let o): try c.encode(o)
        }
    }
}

/// A request, or a notification when `id` is nil.
public struct RPCRequest<Params: Codable & Sendable>: Codable, Sendable {
    public var id: RPCID?
    public var method: String
    public var params: Params?

    public init(id: RPCID?, method: String, params: Params?) {
        self.id = id
        self.method = method
        self.params = params
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case jsonrpc, id, method, params
    }

    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        guard try c.decode(String.self, forKey: .jsonrpc) == VibeRPC.jsonrpc else {
            throw DecodingError.dataCorruptedError(
                forKey: .jsonrpc, in: c, debugDescription: "jsonrpc must be \"2.0\"")
        }
        // An absent id makes a notification; a null one doesn't.
        if c.contains(.id) {
            id = try c.decode(RPCID.self, forKey: .id)
        } else {
            id = nil
        }
        method = try c.decode(String.self, forKey: .method)
        params = try c.decodeIfPresent(Params.self, forKey: .params)
    }

    public func encode(to encoder: any Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(VibeRPC.jsonrpc, forKey: .jsonrpc)
        try c.encodeIfPresent(id, forKey: .id)
        try c.encode(method, forKey: .method)
        try c.encodeIfPresent(params, forKey: .params)
    }
}

/// A response: exactly one of a result or an error.
public struct RPCResponse<Value: Codable & Sendable>: Codable, Sendable {
    public enum Outcome: Sendable {
        case result(Value)
        case error(RPCError)
    }

    public var id: RPCID
    public var outcome: Outcome

    public init(id: RPCID, outcome: Outcome) {
        self.id = id
        self.outcome = outcome
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case jsonrpc, id, result, error
    }

    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        guard try c.decode(String.self, forKey: .jsonrpc) == VibeRPC.jsonrpc else {
            throw DecodingError.dataCorruptedError(
                forKey: .jsonrpc, in: c, debugDescription: "jsonrpc must be \"2.0\"")
        }
        id = try c.decode(RPCID.self, forKey: .id)
        switch (c.contains(.result), c.contains(.error)) {
        case (true, false):
            outcome = .result(try c.decode(Value.self, forKey: .result))
        case (false, true):
            outcome = .error(try c.decode(RPCError.self, forKey: .error))
        default:
            throw DecodingError.dataCorruptedError(
                forKey: .result, in: c,
                debugDescription: "a response has exactly one of result and error")
        }
    }

    public func encode(to encoder: any Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(VibeRPC.jsonrpc, forKey: .jsonrpc)
        try c.encode(id, forKey: .id)
        switch outcome {
        case .result(let value): try c.encode(value, forKey: .result)
        case .error(let error): try c.encode(error, forKey: .error)
        }
    }
}

/// The error object of a response.
public struct RPCError: Codable, Hashable, Sendable, Error, CustomStringConvertible {
    public var code: Int
    public var message: String
    public var data: JSONValue?

    public init(code: Int, message: String, data: JSONValue? = nil) {
        self.code = code
        self.message = message
        self.data = data
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case code, message, data
    }

    public var description: String { "\(message) (\(code))" }
}

/// The error codes in the contract, in the order protocol/vibed-rpc.json
/// lists them.
public enum RPCErrorCode: Int, CaseIterable, Sendable {
    case parseError = -32700
    case invalidRequest = -32600
    case methodNotFound = -32601
    case invalidParams = -32602
    case internalError = -32603
    /// The client's protocol isn't vibed's; the data is a
    /// `ProtocolMismatchData`.
    case protocolMismatch = -32000

    /// The name protocol/vibed-rpc.json uses.
    public var name: String {
        switch self {
        case .parseError: "parse_error"
        case .invalidRequest: "invalid_request"
        case .methodNotFound: "method_not_found"
        case .invalidParams: "invalid_params"
        case .internalError: "internal_error"
        case .protocolMismatch: "protocol_mismatch"
        }
    }
}

/// A method in the contract, with its param and result types.
public protocol RPCMethod {
    associatedtype Params: Codable & Sendable
    associatedtype Result: Codable & Sendable
    static var name: String { get }
}

/// `daemon.hello`: the handshake. It names the client and checks both sides
/// speak the same protocol version.
public enum Hello: RPCMethod {
    public typealias Params = HelloParams
    public typealias Result = HelloResult
    public static let name = "daemon.hello"
}

public struct HelloParams: Codable, Hashable, Sendable {
    /// Who is calling, for vibed's log.
    public var client: String
    public var protocolVersion: Int

    public init(client: String, protocolVersion: Int = VibeRPC.protocolVersion) {
        self.client = client
        self.protocolVersion = protocolVersion
    }

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case client
        case protocolVersion = "protocol"
    }
}

public struct HelloResult: Codable, Hashable, Sendable {
    /// Always "vibed".
    public var daemon: String
    public var version: String
    public var protocolVersion: Int
    /// "macos" or "linux".
    public var os: String
    /// "aarch64" or "x86_64".
    public var arch: String
    public var pid: Int

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case daemon, version
        case protocolVersion = "protocol"
        case os, arch, pid
    }
}

/// The data of a `protocolMismatch` error.
public struct ProtocolMismatchData: Codable, Hashable, Sendable {
    /// vibed's protocol version.
    public var protocolVersion: Int

    public enum CodingKeys: String, CodingKey, CaseIterable {
        case protocolVersion = "protocol"
    }
}
