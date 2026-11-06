import Foundation
import VibeRPC

/// Tailscale's page for turning HTTPS on, if `error` is vibed saying that's what stands in
/// the way of serving a host's MCP on its tailnet name. Only an `https` link: it comes from a
/// host, and goes in a button.
public func tailscaleEnableURL(from error: any Error) -> URL? {
    guard case VibedClientError.rpc(let rpc) = error,
        case .object(let data)? = rpc.data,
        case .string(let link)? = data["tailscale_enable_url"],
        let url = URL(string: link),
        url.scheme == "https"
    else { return nil }
    return url
}
