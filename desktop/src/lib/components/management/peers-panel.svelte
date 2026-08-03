<script lang="ts">
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { formatAge, formatBytes } from '$lib/format';
	import type { Peers } from '$lib/bridge';
	import ManagementConfirmDialog from './management-confirm-dialog.svelte';

	let {
		peers,
		busy,
		onRefresh,
		onForget
	}: {
		peers: Peers;
		busy: boolean;
		onRefresh: () => void;
		onForget: (deviceId: string) => void;
	} = $props();

	let deviceId = $state('');
	let pendingForget = $state<string | null>(null);
	const connected = $derived(peers.peers.filter((peer) => peer.connected));

	function deviceState(device: Peers['devices'][number]) {
		if (device.local) return 'local';
		if (device.forgotten) return 'forgotten';
		return 'remembered';
	}

	function confirmForget() {
		if (!pendingForget) return;
		onForget(pendingForget);
		pendingForget = null;
		deviceId = '';
	}
</script>

<section class="management-panel" aria-labelledby="peers-heading">
	<header class="management-heading-row">
		<div>
			<p class="management-kicker">Mesh visibility</p>
			<h1 id="peers-heading">Peers</h1>
			<p>
				{peers.localHostname} · {peers.localAddresses.length > 0
					? peers.localAddresses.join(', ')
					: 'networking disabled'}
			</p>
		</div>
		<Button variant="outline" size="sm" disabled={busy} onclick={onRefresh}>Refresh</Button>
	</header>

	{#if peers.discoveryError}
		<p class="management-error">Interface discovery: {peers.discoveryError}</p>
	{/if}

	<div class="management-group">
		<h2>Connected <span>{connected.length}</span></h2>
		<div class="management-card-grid">
			{#each connected as peer (peer.address)}
				<article class="management-card peer-card">
					<header>
						<span class="state-dot online"></span><strong>{peer.hostname}</strong><small
							>CONNECTED</small
						>
					</header>
					<code>{peer.address}</code>
					{#if peer.stats}
						<dl class="peer-stats">
							<div>
								<dt>Shared</dt>
								<dd>{peer.stats.sharedItems}</dd>
							</div>
							<div>
								<dt>Bytes</dt>
								<dd>{formatBytes(peer.stats.sharedBytes)}</dd>
							</div>
							<div>
								<dt>Pinned</dt>
								<dd>{peer.stats.pinnedItems}</dd>
							</div>
						</dl>
						<p>
							{peer.stats.lastSharedMillis
								? `Latest share ${formatAge(peer.stats.lastSharedMillis)}`
								: 'No retained items'}
						</p>
					{:else}
						<p>History stats unavailable until identity is authenticated.</p>
					{/if}
				</article>
			{:else}
				<div class="management-empty-state">
					<strong>No connected peers</strong>
					<p>Peers appear here after an authenticated ClipSync mesh session is established.</p>
				</div>
			{/each}
		</div>
	</div>

	<div class="management-group">
		<h2>Remembered mesh devices <span>{peers.devices.length}</span></h2>
		<p>Forgetting removes a replication identity; it does not revoke the shared mesh secret.</p>
		<div class="device-list">
			{#each peers.devices as device (device.deviceId)}
				<div class="device-row">
					<code>{device.deviceId}</code>
					<span>{deviceState(device)}</span>
					{#if !device.local && !device.forgotten}
						<Button
							variant="outline"
							size="sm"
							disabled={busy}
							onclick={() => (pendingForget = device.deviceId)}>Forget</Button
						>
					{/if}
				</div>
			{/each}
		</div>
		<form
			class="management-inline-form"
			onsubmit={(event) => {
				event.preventDefault();
				if (deviceId.trim()) pendingForget = deviceId.trim();
			}}
		>
			<Input
				bind:value={deviceId}
				aria-label="Stable device UUID"
				placeholder="Stable device UUID"
			/>
			<Button type="submit" variant="outline" size="sm" disabled={busy || !deviceId.trim()}
				>Review forget</Button
			>
		</form>
	</div>

	{#if pendingForget}
		<ManagementConfirmDialog
			title={`Forget ${pendingForget}?`}
			description="A machine holding the mesh secret can rejoin with a new identity."
			confirmLabel="Confirm forget"
			destructive
			{busy}
			onConfirm={confirmForget}
			onCancel={() => (pendingForget = null)}
		/>
	{/if}
</section>
