// VibeOS's way to Pi's own model list and sign-in, for the app (PRD: Who gets which
// credentials; Profiles). Loaded with `pi --mode rpc -e`, for this process only, by
// `vibed` and nothing else: no agent ever has it. It holds no provider's details.
// Pi's own code signs in and writes `auth.json`, under Pi's own lock, as `/login`
// does, and this only carries Pi's questions out and the owner's answers back.
//
// It says what it has as JSON, marked `vibe`, through Pi's RPC extension UI: a
// `notify` for what's only to be shown, and an `input` for what wants an answer.
// Nothing here is logged. An answer goes into Pi's sign-in and nowhere else.
//
// `ctx.modelRegistry.runtime` is what `/login` itself uses (Pi 1.5's
// `interactive-mode.ts`) and isn't a documented API, so `vibed` runs only the Pi it
// pins with this: look again when the pin moves.

import { randomUUID } from "node:crypto";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

// How long a new login's model list may take to come: it's only a nicety.
const REFRESH_MS = 15_000;

// What Pi's own settings call this install, as `/login` passes on, if it has one
// yet. One sign-in needs it to name a device; a new one does as well for that.
function deviceId(): string {
	try {
		const dir = process.env.PI_CODING_AGENT_DIR ?? "";
		const said = JSON.parse(readFileSync(join(dir, "settings.json"), "utf8")).deviceId;
		if (typeof said === "string" && said) return said;
	} catch {
		// No settings yet, which is usual.
	}
	return randomUUID();
}

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
					oauth: Boolean(p.auth?.oauth?.login),
					oauth_label: p.auth?.oauth?.loginLabel,
					api_key: Boolean(p.auth?.apiKey?.login),
					configured: runtime.getProviderAuthStatus(p.id)?.configured === true,
					models: p.getModels().map((m: any) => ({ id: m.id, name: m.name })),
				}));
				say({ vibe: "catalog", providers });
			} catch (error) {
				say({ vibe: "failed", message: messageOf(error) });
			}
		},
	});

	pi.registerCommand("vibe-login", {
		description: "VibeOS: sign in to a provider",
		handler: async (args, ctx) => {
			const say = (what: unknown) => ctx.ui.notify(JSON.stringify(what), "info");
			const [provider, method] = args.trim().split(/\s+/);
			try {
				const runtime = (ctx.modelRegistry as any).runtime;
				await runtime.login(
					provider,
					method,
					{
						// A prompt has a signal when something else can answer it, as a
						// browser's callback can: then it's over, with no answer needed.
						prompt: async (prompt: any) => {
							const { signal, ...asked } = prompt;
							const answer = await ctx.ui.input(
								JSON.stringify({ vibe: "prompt", ...asked }),
								"",
								signal ? { signal } : undefined,
							);
							say({ vibe: "prompt_over" });
							if (answer === undefined) throw new Error("Login cancelled");
							return answer;
						},
						notify: (event: any) => say({ vibe: "event", ...event }),
					},
					{ getDeviceId: deviceId },
				);
				say({ vibe: "event", type: "progress", message: "Getting its models…" });
				// As `/login` does: some list what a login may use only once it's there.
				await runtime
					.refresh({ providers: [provider], signal: AbortSignal.timeout(REFRESH_MS) })
					.catch(() => {});
				say({ vibe: "done" });
			} catch (error) {
				say({ vibe: "failed", message: messageOf(error) });
			}
		},
	});
}
