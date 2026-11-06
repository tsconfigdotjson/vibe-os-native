// VibeOS's way to Pi's own model list, for the app (PRD: Profiles). Loaded with
// `pi --mode rpc -e`, for this process only, by `vibed` and nothing else: no agent ever
// has it. It holds no provider's details, and only ever reads: a login is Pi's own.
//
// It says what it has as JSON, marked `vibe`, through a `notify` of Pi's RPC extension
// UI. Nothing here is logged.
//
// `ctx.modelRegistry.runtime` isn't a documented API, so `vibed` runs only the Pi it
// pins with this: look again when the pin moves.

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

function messageOf(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

export default function (pi: ExtensionAPI) {
	pi.registerCommand("vibe-catalog", {
		description: "VibeOS: Pi's providers and models, as JSON",
		handler: async (_args, ctx) => {
			const say = (what: unknown) => ctx.ui.notify(JSON.stringify(what), "info");
			try {
				const runtime = (ctx.modelRegistry as any).runtime;
				const providers = runtime.getProviders().map((p: any) => ({
					id: p.id,
					name: p.name,
					configured: runtime.getProviderAuthStatus(p.id)?.configured === true,
					models: p.getModels().map((m: any) => ({ id: m.id, name: m.name })),
				}));
				say({ vibe: "catalog", providers });
			} catch (error) {
				say({ vibe: "failed", message: messageOf(error) });
			}
		},
	});
}
