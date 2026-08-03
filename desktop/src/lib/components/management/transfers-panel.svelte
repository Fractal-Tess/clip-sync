<script lang="ts">
	import { Button } from '$lib/components/ui/button';
	import { formatBytes } from '$lib/format';
	import type { Transfer } from '$lib/bridge';
	import ManagementConfirmDialog from './management-confirm-dialog.svelte';

	let {
		transfers,
		busy,
		onRefresh,
		onCancel
	}: {
		transfers: Transfer[];
		busy: boolean;
		onRefresh: () => void;
		onCancel: (transferId: string) => void;
	} = $props();

	let pendingCancel = $state<string | null>(null);
	const pendingTransfer = $derived(
		transfers.find(
			(transfer) => transfer.transferId === pendingCancel && cancellable(transfer.state)
		) ?? null
	);

	function progress(transfer: Transfer) {
		if (transfer.totalBytes <= 0) return 0;
		return Math.min(100, (transfer.completedBytes / transfer.totalBytes) * 100);
	}

	function cancellable(state: string) {
		return !['complete', 'cancelled', 'failed'].includes(state.toLowerCase());
	}

	function confirmCancel() {
		if (!pendingTransfer) {
			pendingCancel = null;
			return;
		}
		onCancel(pendingTransfer.transferId);
		pendingCancel = null;
	}
</script>

<section class="management-panel" aria-labelledby="transfers-heading">
	<header class="management-heading-row">
		<div>
			<p class="management-kicker">Payload movement</p>
			<h1 id="transfers-heading">Transfers</h1>
			<p>Live progress reported by the daemon.</p>
		</div>
		<Button variant="outline" size="sm" disabled={busy} onclick={onRefresh}>Refresh</Button>
	</header>

	<div class="transfer-list">
		{#each transfers as transfer (transfer.transferId)}
			<article class="management-card transfer-card">
				<header>
					<code>{transfer.transferId}</code>
					<strong>{transfer.state}</strong>
				</header>
				<p>Peer {transfer.peer || 'unknown'} · content {transfer.contentId || 'pending'}</p>
				<progress
					value={progress(transfer)}
					max="100"
					aria-label={`Transfer ${transfer.transferId} progress`}
				></progress>
				<footer>
					<span>{formatBytes(transfer.completedBytes)} / {formatBytes(transfer.totalBytes)}</span>
					{#if cancellable(transfer.state)}
						<Button
							variant="outline"
							size="sm"
							disabled={busy}
							onclick={() => (pendingCancel = transfer.transferId)}>Cancel</Button
						>
					{/if}
				</footer>
			</article>
		{:else}
			<div class="management-empty-state">
				<strong>No transfers</strong>
				<p>Active and recently retained transfers will appear here.</p>
			</div>
		{/each}
	</div>

	{#if pendingTransfer}
		<ManagementConfirmDialog
			title={`Cancel ${pendingTransfer.transferId}?`}
			description="Partial local staging will be cleaned and cancellation will replicate."
			confirmLabel="Confirm cancel"
			destructive
			{busy}
			onConfirm={confirmCancel}
			onCancel={() => (pendingCancel = null)}
		/>
	{/if}
</section>
