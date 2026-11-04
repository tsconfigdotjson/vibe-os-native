// VibeOS's shared MCP services, for a Pi agent (PRD: Environments and
// profiles, Shared MCP services). Loaded with `pi -e`, for this process only: it writes no
// `mcp.json`, and the owner's own servers keep working. A server of the same
// name in `mcp.json` wins, which the reserved `vibe-` prefix keeps from
// happening by accident.
//
// VIBE_SERVICES is what this agent's profile has on, the same JSON as Claude's
// `--mcp-config`:
//
//   {"mcpServers":{"vibe-linear":{"type":"stdio","command":"vibe","args":["mcp-proxy","linear"]}}}
//
// Each is `vibe mcp-proxy <service>`: its host's broker holds the service's
// login, and nothing here or in Pi ever sees it. Pi hands its own environment on
// to what it starts, so the shim finds VIBE_SERVICES_SOCKET and VIBE_AGENT there.
//
// They're `direct`, not Pi's default of `codemode`, as Claude gets them: the
// model sees the tools from its first prompt, and Pi waits for them to connect,
// with no script between it and them. An entry may say another `exposure`.

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

interface Server {
	command?: unknown;
	args?: unknown;
	exposure?: unknown;
}

const EXPOSURES = new Set(["codemode", "deferred", "direct", "hidden"]);

export default function (pi: ExtensionAPI) {
	const text = process.env.VIBE_SERVICES;
	if (!text) return;
	let servers: Record<string, Server>;
	try {
		servers = JSON.parse(text).mcpServers ?? {};
	} catch (error) {
		// Pi's own way to say it: it draws the terminal, and printing over that
		// would garble its screen.
		pi.on("session_start", (_event, ctx) => {
			ctx.ui.notify(`VibeOS: VIBE_SERVICES isn't JSON, so no shared services: ${error}`, "error");
		});
		return;
	}
	for (const [name, server] of Object.entries(servers)) {
		if (typeof server?.command !== "string") continue;
		const args = Array.isArray(server.args) ? server.args.map(String) : [];
		const exposure = EXPOSURES.has(server.exposure as string) ? server.exposure : "direct";
		pi.registerMcpServer(name, {
			command: server.command,
			args,
			description: `${name.replace(/^vibe-/, "")}, shared by VibeOS`,
			exposure,
		} as Parameters<ExtensionAPI["registerMcpServer"]>[1]);
	}
}
