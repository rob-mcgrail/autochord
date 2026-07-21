/**
 * Haunt Footer Extension
 *
 * Replicates the built-in footer layout but converts USD costs to NZD.
 * - Fetches NZD/USD rate from a free API, caches for 3 days
 * - Shows pwd, git branch, session name on first line
 * - Shows token stats with NZD cost on second line
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import type { AssistantMessage } from "@earendil-works/pi-ai";
import { truncateToWidth, visibleWidth } from "@earendil-works/pi-tui";
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { join } from "node:path";
import { homedir } from "node:os";

const CACHE_DIR = join(homedir(), ".pi", "haunt-footer");
const CACHE_FILE = join(CACHE_DIR, "rate.json");
const CACHE_MAX_AGE_MS = 3 * 24 * 60 * 60 * 1000; // 3 days

interface CachedRate {
	rate: number; // NZD per 1 USD
	fetchedAt: string; // ISO timestamp
}

async function readCache(): Promise<CachedRate | null> {
	try {
		const raw = await readFile(CACHE_FILE, "utf8");
		return JSON.parse(raw);
	} catch {
		return null;
	}
}

async function writeCache(rate: number): Promise<void> {
	await mkdir(CACHE_DIR, { recursive: true });
	const data: CachedRate = { rate, fetchedAt: new Date().toISOString() };
	await writeFile(CACHE_FILE, JSON.stringify(data, null, 2), "utf8");
}

async function fetchExchangeRate(): Promise<number> {
	const res = await fetch(
		"https://open.er-api.com/v6/latest/USD",
		{ signal: AbortSignal.timeout(10_000) },
	);
	if (!res.ok) throw new Error(`Exchange rate API returned ${res.status}`);
	const json = (await res.json()) as { rates: { NZD: number } };
	if (!json.rates?.NZD) throw new Error("NZD rate not found in response");
	return json.rates.NZD;
}

async function getExchangeRate(): Promise<number> {
	const cached = await readCache();
	if (cached) {
		const age = Date.now() - new Date(cached.fetchedAt).getTime();
		if (age < CACHE_MAX_AGE_MS) return cached.rate;
	}

	try {
		const rate = await fetchExchangeRate();
		await writeCache(rate);
		return rate;
	} catch (err) {
		// If fetch fails but we have a stale cache, use it as fallback
		if (cached) return cached.rate;
		throw err;
	}
}

function formatTokens(count: number): string {
	if (count < 1000) return count.toString();
	if (count < 10000) return `${(count / 1000).toFixed(1)}k`;
	if (count < 1000000) return `${Math.round(count / 1000)}k`;
	if (count < 10000000) return `${(count / 1000000).toFixed(1)}M`;
	return `${Math.round(count / 1000000)}M`;
}

export default async function (pi: ExtensionAPI) {
	// Fetch rate at startup (async factory)
	let rate: number;
	try {
		rate = await getExchangeRate();
	} catch {
		rate = 1.6; // fallback if API is unreachable and no cache
	}

	pi.on("session_start", async (_event, ctx) => {
		ctx.ui.setFooter((tui, theme, footerData) => {
			const unsub = footerData.onBranchChange(() => tui.requestRender());

			return {
				dispose: unsub,
				invalidate() {},
				render(width: number): string[] {
					// Calculate cumulative usage from all entries
					let totalInput = 0;
					let totalOutput = 0;
					let totalCacheRead = 0;
					let totalCacheWrite = 0;
					let totalCost = 0;

					for (const entry of ctx.sessionManager.getEntries()) {
						if (entry.type === "message" && entry.message.role === "assistant") {
							const m = entry.message as AssistantMessage;
							totalInput += m.usage.input;
							totalOutput += m.usage.output;
							totalCacheRead += m.usage.cacheRead;
							totalCacheWrite += m.usage.cacheWrite;
							totalCost += m.usage.cost.total;
						}
					}

					// Pwd with ~ for home directory
					let pwd = ctx.cwd;
					const home = process.env.HOME || process.env.USERPROFILE;
					if (home && pwd.startsWith(home)) {
						pwd = `~${pwd.slice(home.length)}`;
					}

					// Add git branch if available
					const branch = footerData.getGitBranch();
					if (branch) {
						pwd = `${pwd} (${branch})`;
					}

					// Add session name if set
					const sessionName = ctx.sessionManager.getSessionName();
					if (sessionName) {
						pwd = `${pwd} • ${sessionName}`;
					}

					// Build stats line
					const statsParts: string[] = [];
					if (totalInput) statsParts.push(`↑${formatTokens(totalInput)}`);
					if (totalOutput) statsParts.push(`↓${formatTokens(totalOutput)}`);
					if (totalCacheRead) statsParts.push(`R${formatTokens(totalCacheRead)}`);
					if (totalCacheWrite) statsParts.push(`W${formatTokens(totalCacheWrite)}`);

					// NZD cost
					if (totalCost) {
						statsParts.push(`$${(totalCost * rate).toFixed(3)} NZD`);
					}

					// Context usage
					const contextUsage = ctx.getContextUsage();
					const contextWindow = contextUsage?.contextWindow ?? ctx.model?.contextWindow ?? 0;
					const contextPercentValue = contextUsage?.percent ?? 0;
					const contextPercent = contextUsage?.percent != null
						? `${contextPercentValue.toFixed(1)}%`
						: "?";
					statsParts.push(`${contextPercent}/${formatTokens(contextWindow)}`);

					const statsLeft = statsParts.join(" ");
					const statsLeftWidth = visibleWidth(statsLeft);

					// Right side: model name with shortcut hint
					const modelName = ctx.model?.id || "no-model";
					const rightSide = `(ctrl-p to cycle) ${modelName}`;
					const rightSideWidth = visibleWidth(rightSide);

					// Build stats line with padding
					const minPadding = 2;
					const totalNeeded = statsLeftWidth + minPadding + rightSideWidth;

					let statsLine: string;
					if (totalNeeded <= width) {
						const padding = " ".repeat(width - statsLeftWidth - rightSideWidth);
						statsLine = statsLeft + padding + rightSide;
					} else {
						const availableForRight = width - statsLeftWidth - minPadding;
						if (availableForRight > 0) {
							const truncatedRight = truncateToWidth(rightSide, availableForRight, "");
							const padding = " ".repeat(Math.max(0, width - statsLeftWidth - visibleWidth(truncatedRight)));
							statsLine = statsLeft + padding + truncatedRight;
						} else {
							statsLine = statsLeft;
						}
					}

					// Dim the lines
					const dimStatsLeft = theme.fg("dim", statsLeft);
					const remainder = statsLine.slice(statsLeft.length);
					const dimRemainder = theme.fg("dim", remainder);
					const pwdLine = truncateToWidth(theme.fg("dim", pwd), width, theme.fg("dim", "..."));

					return [pwdLine, dimStatsLeft + dimRemainder];
				},
			};
		});
	});
}
