import { getCurrentWindow } from '@tauri-apps/api/window';

import {
	commands,
	type HistoryItemView,
	type DiagnosticView,
	type HistoryPageView,
	type HistoryUpdateView,
	type ImagePreviewView,
	type MutationView,
	type PeersView,
	type Result,
	type SettingsView,
	type SharedSettingView,
	type StatusView,
	type TransferView
} from './bindings';

export type Status = StatusView;
export type HistoryItem = HistoryItemView;
export type HistoryPage = HistoryPageView;
export type HistoryUpdate = HistoryUpdateView;
export type Mutation = MutationView;
export type ImagePreview = ImagePreviewView;
export type Peers = PeersView;
export type Settings = SettingsView;
export type Diagnostic = DiagnosticView;
export type Transfer = TransferView;
export type SharedSetting = SharedSettingView;

function unwrap<T>(result: Result<T, string>) {
	if (result.status === 'error') throw new Error(result.error);
	return result.data;
}

const sampleHistory: HistoryItem[] = [
	{
		contentId: 'preview-release-notes',
		preview: 'Release checklist: verify Wayland activation, encrypted storage, and peer catch-up.',
		mimeTypes: ['text/plain;charset=utf-8'],
		logicalSize: 86,
		sourceNode: 'preview-node-vd',
		sourceDevice: 'vd',
		pinned: true,
		physicalMillis: Date.now() - 82_000,
		originMillis: Date.now() - 82_000
	},
	{
		contentId: 'preview-docs-url',
		preview: 'https://v2.tauri.app/start/frontend/sveltekit/',
		mimeTypes: ['text/plain'],
		logicalSize: 51,
		sourceNode: 'preview-node-kiwi',
		sourceDevice: 'kiwi',
		pinned: false,
		physicalMillis: Date.now() - 3_640_000,
		originMillis: Date.now() - 3_640_000
	},
	{
		contentId: 'preview-image',
		preview: 'image/png · 164627 bytes',
		mimeTypes: ['image/png'],
		logicalSize: 164_627,
		sourceNode: 'preview-node-vd',
		sourceDevice: 'vd',
		pinned: false,
		physicalMillis: Date.now() - 86_400_000,
		originMillis: Date.now() - 86_400_000
	}
];

export function isTauri() {
	return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
}

export async function getStatus() {
	if (isTauri()) return unwrap(await commands.getStatus());
	return {
		version: 'preview',
		hostname: 'browser-preview',
		uptimeSeconds: 0,
		configPath: 'Tauri bridge not connected',
		localAddresses: ['192.168.10.4'],
		discoveredPeers: 1,
		connectedPeers: 1
	} satisfies Status;
}

export async function getHistory(query: string, offset: number, limit: number) {
	if (isTauri()) return unwrap(await commands.getHistory(query, offset, limit));
	const normalized = query.trim().toLowerCase();
	const deviceFilter = normalized.match(/^(?:d|device):"((?:\\.|[^"])*)"$/);
	const requestedDevice = deviceFilter?.[1]?.replaceAll(/\\(["\\])/g, '$1');
	const matches = sampleHistory.filter((item) => {
		if (requestedDevice !== undefined) {
			return [item.sourceDevice, item.sourceNode].some(
				(source) => source?.toLowerCase() === requestedDevice
			);
		}
		return normalized
			? `${item.preview} ${item.sourceDevice} ${item.sourceNode} ${item.mimeTypes.join(' ')}`
					.toLowerCase()
					.includes(normalized)
			: true;
	});
	const start = Math.max(0, Math.trunc(offset));
	const pageSize = Math.max(1, Math.trunc(limit));
	return {
		items: matches.slice(start, start + pageSize),
		total: matches.length
	} satisfies HistoryPage;
}

function sampleImagePreview(contentId: string) {
	const width = 96;
	const height = 60;
	const rgba = Array.from({ length: width * height * 4 }, (_, index) => {
		const pixel = Math.floor(index / 4);
		const x = pixel % width;
		const y = Math.floor(pixel / width);
		const channel = index % 4;
		const frame = x < 3 || x >= width - 3 || y < 3 || y >= height - 3;
		const signal = x > 15 && x < 76 && y > 14 && y < 21;
		const handoff = x > 58 && x < 78 && y > 37 && y < 50;
		const color = frame || signal ? [67, 214, 230] : handoff ? [255, 127, 131] : [13, 23, 27];
		return channel === 3 ? 255 : color[channel];
	});
	return {
		contentId,
		mimeType: 'image/png',
		width,
		height,
		rgba
	} satisfies ImagePreview;
}

export async function getPeers() {
	if (isTauri()) return unwrap(await commands.getPeers());
	return {
		localHostname: 'browser-preview',
		localAddresses: ['192.168.10.4'],
		peers: [
			{
				hostname: 'kiwi.preview',
				address: '192.168.10.9',
				connected: true,
				stats: {
					sharedItems: 18,
					sharedBytes: 24_912,
					pinnedItems: 2,
					lastSharedMillis: Date.now()
				}
			}
		],
		discoveryError: null,
		devices: [
			{ deviceId: 'preview-local', local: true, forgotten: false },
			{ deviceId: 'preview-remote', local: false, forgotten: false }
		]
	} satisfies Peers;
}

export async function getSettings() {
	if (isTauri()) return unwrap(await commands.getSettings());
	return {
		shared: {
			meshQuotaBytes: 1_073_741_824,
			captureThresholdBytes: 1_048_576,
			revision: 'preview'
		},
		local: {
			listenPort: 47_822,
			discoveryIntervalSeconds: 15,
			reconcileIntervalSeconds: 5,
			reconnectMinSeconds: 1,
			reconnectMaxSeconds: 30,
			peerInterfaces: ['eth0'],
			meshKeyFileConfigured: true,
			configPath: 'Browser preview — no local configuration'
		}
	} satisfies Settings;
}

export async function getDiagnostics() {
	if (isTauri()) return unwrap(await commands.getDiagnostics());
	return [
		{ name: 'daemon', ok: true, detail: 'Browser preview simulation' },
		{ name: 'clipboard', ok: true, detail: 'Sensitive clipboard access is disabled in preview' }
	] satisfies Diagnostic[];
}

export async function getTransfers() {
	if (isTauri()) return unwrap(await commands.getTransfers());
	return [] satisfies Transfer[];
}

export async function cancelTransfer(transferId: string) {
	if (isTauri()) return unwrap(await commands.cancelTransfer(transferId));
	return {
		ok: true,
		message: 'Preview cancellation simulated',
		resourceId: transferId
	} satisfies Mutation;
}

export async function forgetDevice(deviceId: string) {
	if (isTauri()) return unwrap(await commands.forgetDevice(deviceId));
	return {
		ok: true,
		message: 'Preview device forget simulated',
		resourceId: deviceId
	} satisfies Mutation;
}

export async function updateSharedSetting(setting: SharedSetting, value: number) {
	if (isTauri()) return unwrap(await commands.updateSharedSetting(setting, value));
	return {
		ok: true,
		message: 'Preview settings update simulated',
		resourceId: setting
	} satisfies Mutation;
}

export async function updatePeerInterfaces(interfaces: string[]) {
	if (isTauri()) return unwrap(await commands.updatePeerInterfaces(interfaces));
	return {
		ok: true,
		message: `Preview peer interfaces set to ${interfaces.join(', ') || 'all interfaces'}`,
		resourceId: 'peer_interfaces'
	} satisfies Mutation;
}

export async function updateHistory(contentId: string, action: HistoryUpdate) {
	if (isTauri()) return unwrap(await commands.updateHistory(contentId, action));
	const index = sampleHistory.findIndex((item) => item.contentId === contentId);
	const item = sampleHistory[index];
	if (!item) return { ok: false, message: 'Preview record not found', resourceId: null };
	if (action === 'delete') sampleHistory.splice(index, 1);
	else item.pinned = action === 'pin';
	return {
		ok: true,
		message: `Preview history ${action} simulated`,
		resourceId: contentId
	} satisfies Mutation;
}

export async function getImagePreview(contentId: string) {
	if (isTauri()) return unwrap(await commands.getImagePreview(contentId));
	return sampleImagePreview(contentId);
}

export async function activateHistory(contentId: string) {
	if (isTauri()) return unwrap(await commands.activateHistory(contentId));
	return {
		ok: true,
		message: 'Preview activation simulated',
		resourceId: contentId
	} satisfies Mutation;
}

export async function closeAppWindow() {
	if (isTauri()) await getCurrentWindow().close();
}
