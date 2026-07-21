/**
 * Haunt Guardrails Extension
 *
 * Intercepts bash tool calls and evaluates them against a policy file
 * using a cheap model. Blocks commands that violate the policy.
 *
 * - Reads guardrails-policy.md from extension directory
 * - Calls OpenRouter directly with meta-llama/llama-4-maverick
 * - Caches verdicts for 1 hour (keyed by policy hash + command)
 * - Fails closed: blocks if model is unreachable or returns unparseable output
 *
 * Commands:
 *   /guardrails        — toggle on/off
 *   /guardrails on     — enable
 *   /guardrails off    — disable
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { isToolCallEventType } from "@earendil-works/pi-coding-agent";
import { readFile } from "node:fs/promises";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { createHash } from "node:crypto";

// Get the directory of this extension file
const __dirname = dirname(fileURLToPath(import.meta.url));
const POLICY_FILE = join(__dirname, "guardrails-policy.md");
const CACHE_TTL_MS = 3600_000; // 1 hour

// Resolve provider config from model registry (available via ctx)
const DEFAULT_BASE_URL = "https://openrouter.ai/api/v1";
const GUARD_MODEL = "meta-llama/llama-4-maverick";
const PROVIDER = "openrouter";

interface CacheEntry {
	verdict: string;
	time: number;
}

async function evaluateCommand(
	apiKey: string,
	baseUrl: string,
	policy: string,
	command: string,
): Promise<string> {
	const prompt = `You are a guard for the Bash tool running in unrestricted mode.
Decide if the proposed command should be ALLOWED or DENIED based on the POLICY below.

POLICY:
${policy}

PROPOSED COMMAND:
${command}

Reply with EXACTLY one line. Either:
  ALLOW
or:
  DENY: <one short sentence explaining which policy rule it hits>

Do not preamble. Do not explain unless denying.`;

	const res = await fetch(`${baseUrl}/chat/completions`, {
		method: "POST",
		headers: {
			Authorization: `Bearer ${apiKey}`,
			"Content-Type": "application/json",
		},
		body: JSON.stringify({
			model: "meta-llama/llama-4-maverick",
			messages: [{ role: "user", content: prompt }],
			max_tokens: 100,
			temperature: 0,
		}),
		signal: AbortSignal.timeout(10_000),
	});

	if (!res.ok) {
		throw new Error(`OpenRouter API error ${res.status}`);
	}

	const json = (await res.json()) as {
		choices: Array<{ message: { content: string } }>;
	};

	const content = json.choices?.[0]?.message?.content?.trim() ?? "";
	return content.split("\n")[0]; // Take first line only
}

export default async function (pi: ExtensionAPI) {
	const cache = new Map<string, CacheEntry>();
	let enabled = true;



	// Toggle command
	pi.registerCommand("guardrails", {
		description: "Toggle guardrails on/off",
		handler: async (args, ctx) => {
			if (args === "off" || args === "disable") {
				enabled = false;
				ctx.ui.notify("Guardrails disabled", "info");
			} else if (args === "on" || args === "enable") {
				enabled = true;
				ctx.ui.notify("Guardrails enabled", "info");
			} else {
				enabled = !enabled;
				ctx.ui.notify(`Guardrails ${enabled ? "enabled" : "disabled"}`, "info");
			}
		},
	});

	pi.on("tool_call", async (event, ctx) => {
		// Only intercept bash tool
		if (!isToolCallEventType("bash", event)) return;
		if (!enabled) return;

		const command = event.input.command;
		if (!command) return;

		// Load policy from extension directory
		let policy: string;
		try {
			policy = await readFile(POLICY_FILE, "utf8");
		} catch {
			return; // No policy file = allow all
		}

		// Cache key: policy hash + command
		const policyHash = createHash("sha256").update(policy).digest("hex").slice(0, 16);
		const cacheKey = createHash("sha256").update(policyHash + "\n" + command).digest("hex");

		// Check cache
		const cached = cache.get(cacheKey);
		if (cached && Date.now() - cached.time < CACHE_TTL_MS) {
			if (cached.verdict.startsWith("DENY")) {
				const reason = cached.verdict.replace(/^DENY:\s*/, "");
				return { block: true, reason: `Guardrails: ${reason}` };
			}
			return; // Cached ALLOW
		}

		// Resolve API key and base URL from the model registry
		const apiKey = ctx.modelRegistry
			? await ctx.modelRegistry.getApiKeyForProvider(PROVIDER)
			: undefined;
		if (!apiKey) {
			return; // No API key — skip guardrails
		}
		const model = ctx.modelRegistry?.find(PROVIDER, GUARD_MODEL);
		const baseUrl = model?.baseUrl?.replace(/\/+$/, "") ?? DEFAULT_BASE_URL;

		// Call model
		let verdict: string;
		try {
			verdict = await evaluateCommand(apiKey, baseUrl, policy, command);
		} catch (err) {
			// Fail closed: if model is unreachable, block
			return {
				block: true,
				reason: `Guardrails: model error — ${err instanceof Error ? err.message : "unknown"}`,
			};
		}

		// Cache the verdict (only if non-empty)
		if (verdict) {
			cache.set(cacheKey, { verdict, time: Date.now() });
		} else {
			// Empty verdict = fail closed
			return { block: true, reason: "Guardrails: empty verdict from model" };
		}

		// Parse and apply verdict
		if (verdict.startsWith("ALLOW")) {
			return; // Allow
		} else if (verdict.startsWith("DENY")) {
			const reason = verdict.replace(/^DENY:\s*/, "");
			return { block: true, reason: `Guardrails: ${reason}` };
		} else {
			return { block: true, reason: `Guardrails: unparseable verdict — ${verdict}` };
		}
	});
}
