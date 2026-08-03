<script lang="ts">
	import { AlertCircle, Check, Server } from '@lucide/svelte';

	import { Alert, AlertAction, AlertDescription, AlertTitle } from '$lib/components/ui/alert';
	import { Button } from '$lib/components/ui/button';

	let {
		connectedToTauri,
		error,
		notice,
		onRetry
	}: {
		connectedToTauri: boolean;
		error: string | null;
		notice: string | null;
		onRetry: () => void;
	} = $props();
</script>

<div class="message-rail">
	{#if !connectedToTauri}
		<Alert class="preview-banner">
			<Server aria-hidden="true" />
			<AlertTitle>Sample register</AlertTitle>
			<AlertDescription>
				Browser preview uses non-sensitive entries. Run <code>bun run tauri dev</code> for daemon IPC.
			</AlertDescription>
		</Alert>
	{/if}

	{#if error}
		<Alert variant="destructive" class="request-message">
			<AlertCircle aria-hidden="true" />
			<AlertTitle>Request incomplete</AlertTitle>
			<AlertDescription>{error}</AlertDescription>
			<AlertAction>
				<Button size="xs" variant="outline" onclick={onRetry}>Refresh history</Button>
			</AlertAction>
		</Alert>
	{:else if notice}
		<Alert role="status" class="request-message success-message">
			<Check aria-hidden="true" />
			<AlertTitle>{connectedToTauri ? 'Request complete' : 'Preview request simulated'}</AlertTitle>
			<AlertDescription>
				{connectedToTauri ? notice : `${notice}. No system clipboard or daemon data was changed.`}
			</AlertDescription>
		</Alert>
	{/if}
</div>
