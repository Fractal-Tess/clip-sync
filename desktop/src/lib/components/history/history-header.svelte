<script lang="ts">
	import { Badge } from '$lib/components/ui/badge';
	import type { Status } from '$lib/bridge';

	let {
		status,
		loading,
		connectedToTauri
	}: { status: Status | null; loading: boolean; connectedToTauri: boolean } = $props();
</script>

<header class="shell-header">
	<div class="brand-lockup">
		<img src="/clip-sync-icon.png" alt="" class="brand-mark" />
		<div class="brand-copy">
			<strong>ClipSync</strong>
			<span>History register</span>
		</div>
	</div>

	<div class="daemon-status" aria-live="polite">
		<span class:online={Boolean(status)} class="status-light" aria-hidden="true"></span>
		<span class="hostname">
			{status
				? status.hostname === 'browser-preview'
					? 'Preview node'
					: status.hostname
				: loading
					? 'Connecting'
					: 'Unavailable'}
		</span>
		{#if status}
			<span class="peer-count">
				{status.discoveredPeers} discovered · {status.connectedPeers} connected
			</span>
		{/if}
		<Badge variant="outline" class="runtime-badge">
			<span class="runtime-full">{connectedToTauri ? 'Tauri IPC' : 'Browser preview'}</span>
			<span class="runtime-short">{connectedToTauri ? 'IPC' : 'Preview'}</span>
		</Badge>
	</div>
</header>
