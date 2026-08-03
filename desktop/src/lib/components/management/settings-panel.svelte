<script lang="ts">
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { formatBytes, parseByteSize } from '$lib/format';
	import type { Settings, SharedSetting } from '$lib/bridge';
	import ManagementConfirmDialog from './management-confirm-dialog.svelte';

	let {
		settings,
		busy,
		onRefresh,
		onUpdate,
		onUpdatePeerInterfaces
	}: {
		settings: Settings;
		busy: boolean;
		onRefresh: () => void;
		onUpdate: (setting: SharedSetting, value: number) => void;
		onUpdatePeerInterfaces: (interfaces: string[]) => void;
	} = $props();

	let meshQuota = $state('');
	let captureThreshold = $state('');
	let peerInterfaces = $derived(settings.local.peerInterfaces.join(', '));
	let pending = $state.raw<{ setting: SharedSetting; label: string; value: number } | null>(null);
	let pendingPeerInterfaces = $state.raw<string[] | null>(null);
	const parsedMeshQuota = $derived(parseByteSize(meshQuota));
	const parsedCaptureThreshold = $derived(Number(captureThreshold));
	const parsedPeerInterfaces = $derived(parsePeerInterfaces(peerInterfaces));

	function review(setting: SharedSetting, label: string, input: string) {
		const value = setting === 'meshQuotaBytes' ? parseByteSize(input) : Number(input);
		if (value !== null && Number.isSafeInteger(value) && value > 0) {
			pending = { setting, label, value };
		}
	}

	function parsePeerInterfaces(input: string) {
		const interfaces = input
			.split(/[\s,]+/)
			.map((interfaceName) => interfaceName.trim())
			.filter(Boolean);
		if (
			interfaces.some((interfaceName) => !/^[A-Za-z0-9_.:-]{1,15}$/.test(interfaceName)) ||
			new Set(interfaces).size !== interfaces.length
		) {
			return null;
		}
		return interfaces;
	}

	function applyPending() {
		if (!pending) return;
		onUpdate(pending.setting, pending.value);
		pending = null;
	}

	function applyPeerInterfaces() {
		if (!pendingPeerInterfaces) return;
		peerInterfaces = pendingPeerInterfaces.join(', ');
		onUpdatePeerInterfaces(pendingPeerInterfaces);
		pendingPeerInterfaces = null;
	}
</script>

<section class="management-panel" aria-labelledby="settings-heading">
	<header class="management-heading-row">
		<div>
			<p class="management-kicker">Effective configuration</p>
			<h1 id="settings-heading">Settings</h1>
			<p>Shared values replicate immediately; local secret paths remain redacted.</p>
		</div>
		<Button variant="outline" size="sm" disabled={busy} onclick={onRefresh}>Refresh</Button>
	</header>

	<div class="settings-ledger">
		<div><span>Mesh quota</span><strong>{formatBytes(settings.shared.meshQuotaBytes)}</strong></div>
		<div>
			<span>Capture threshold</span><strong
				>{formatBytes(settings.shared.captureThresholdBytes)}</strong
			>
		</div>
		<div>
			<span>Shared revision</span><code>{settings.shared.revision || 'not replicated yet'}</code>
		</div>
	</div>

	<div class="settings-forms">
		<form
			onsubmit={(event) => {
				event.preventDefault();
				review('meshQuotaBytes', 'mesh quota', meshQuota);
			}}
		>
			<label for="mesh-quota">Mesh quota</label>
			<div>
				<Input id="mesh-quota" bind:value={meshQuota} placeholder="e.g. 512 MiB or 1.5 GiB" />
				<Button
					type="submit"
					variant="outline"
					size="sm"
					disabled={busy || parsedMeshQuota === null || parsedMeshQuota <= 0}>Review quota</Button
				>
			</div>
			<small
				>Accepts exact bytes or KB, KiB, MB, MiB, GB, and GiB. Lowering quota may remove unpinned
				history.</small
			>
		</form>
		<form
			onsubmit={(event) => {
				event.preventDefault();
				review('captureThresholdBytes', 'capture threshold', captureThreshold);
			}}
		>
			<label for="capture-threshold">Capture threshold in bytes</label>
			<div>
				<Input
					id="capture-threshold"
					bind:value={captureThreshold}
					inputmode="numeric"
					placeholder={String(settings.shared.captureThresholdBytes)}
				/>
				<Button
					type="submit"
					variant="outline"
					size="sm"
					disabled={busy ||
						!Number.isSafeInteger(parsedCaptureThreshold) ||
						parsedCaptureThreshold <= 0}>Review threshold</Button
				>
			</div>
			<small>Items above this threshold require explicit sharing.</small>
		</form>
	</div>

	{#if pending}
		<ManagementConfirmDialog
			title={`Update ${pending.label}?`}
			description={`Set the replicated value to ${formatBytes(pending.value)} (${pending.value.toLocaleString()} bytes).`}
			confirmLabel="Apply setting"
			{busy}
			onConfirm={applyPending}
			onCancel={() => (pending = null)}
		/>
	{/if}

	<div class="management-group">
		<h2>Local daemon</h2>
		<form
			class="local-setting-form"
			onsubmit={(event) => {
				event.preventDefault();
				if (parsedPeerInterfaces !== null) pendingPeerInterfaces = parsedPeerInterfaces;
			}}
		>
			<label for="peer-interfaces">Discovery and connection interfaces</label>
			<div>
				<Input
					id="peer-interfaces"
					bind:value={peerInterfaces}
					placeholder="eth0, wt0"
					aria-describedby="peer-interfaces-help"
				/>
				<Button
					type="submit"
					variant="outline"
					size="sm"
					disabled={busy || parsedPeerInterfaces === null}>Review interfaces</Button
				>
			</div>
			<small id="peer-interfaces-help">
				Comma- or space-separated Linux interface names. Leave empty to disable network discovery
				and incoming mesh connections.
			</small>
		</form>
		<dl class="settings-detail-list">
			<div>
				<dt>Listen port</dt>
				<dd>{settings.local.listenPort}</dd>
			</div>
			<div>
				<dt>Discovery interval</dt>
				<dd>{settings.local.discoveryIntervalSeconds}s</dd>
			</div>
			<div>
				<dt>Reconciliation interval</dt>
				<dd>{settings.local.reconcileIntervalSeconds}s</dd>
			</div>
			<div>
				<dt>Reconnect delay</dt>
				<dd>{settings.local.reconnectMinSeconds}–{settings.local.reconnectMaxSeconds}s</dd>
			</div>
			<div>
				<dt>Peer interfaces</dt>
				<dd>
					{settings.local.peerInterfaces.length > 0
						? settings.local.peerInterfaces.join(', ')
						: 'networking disabled'}
				</dd>
			</div>
			<div>
				<dt>Mesh key file</dt>
				<dd>
					{settings.local.meshKeyFileConfigured ? 'configured (path redacted)' : 'not configured'}
				</dd>
			</div>
			<div>
				<dt>Config</dt>
				<dd><code>{settings.local.configPath}</code></dd>
			</div>
		</dl>
	</div>

	{#if pendingPeerInterfaces}
		<ManagementConfirmDialog
			title="Update peer discovery interfaces?"
			description={pendingPeerInterfaces.length > 0
				? `Use ${pendingPeerInterfaces.join(', ')} for discovery and mesh connections.`
				: 'Disable network discovery and incoming mesh connections.'}
			confirmLabel="Apply interfaces"
			{busy}
			onConfirm={applyPeerInterfaces}
			onCancel={() => (pendingPeerInterfaces = null)}
		/>
	{/if}
</section>
