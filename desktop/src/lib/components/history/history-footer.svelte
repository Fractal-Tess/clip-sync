<script lang="ts">
	import { ChevronLeft, ChevronRight, Wifi } from '@lucide/svelte';

	import type { Status } from '$lib/bridge';
	import { Button } from '$lib/components/ui/button';
	import { Kbd } from '$lib/components/ui/kbd';

	let {
		currentPage,
		pageCount,
		total,
		status,
		connectedToTauri,
		historyActive,
		onPrevious,
		onNext
	}: {
		currentPage: number;
		pageCount: number;
		total: number;
		status: Status | null;
		connectedToTauri: boolean;
		historyActive: boolean;
		onPrevious: () => void;
		onNext: () => void;
	} = $props();
</script>

<footer class="shell-footer">
	<nav class="pagination-register" aria-label="Clipboard history pages" hidden={!historyActive}>
		<Button
			variant="outline"
			size="icon-xs"
			class="pager-button"
			disabled={currentPage === 0 || total === 0}
			aria-label="Previous history page"
			aria-keyshortcuts="PageUp"
			title="Previous page (Page Up)"
			onclick={onPrevious}
		>
			<ChevronLeft aria-hidden="true" />
		</Button>
		<span class="page-status" aria-live="polite">{currentPage + 1} / {pageCount}</span>
		<Button
			variant="outline"
			size="icon-xs"
			class="pager-button"
			disabled={currentPage >= pageCount - 1 || total === 0}
			aria-label="Next history page"
			aria-keyshortcuts="PageDown"
			title="Next page (Page Down)"
			onclick={onNext}
		>
			<ChevronRight aria-hidden="true" />
		</Button>
	</nav>
	<span class="bridge-address">
		<Wifi aria-hidden="true" />
		{status?.localAddresses.join(', ') ||
			(connectedToTauri ? 'Local daemon bridge' : 'Sample register')}
	</span>
	<div class="shortcut-register" aria-label="Keyboard shortcuts">
		{#if historyActive}
			<span><Kbd>/</Kbd><span>Search</span></span>
			<span><Kbd>HJKL</Kbd><span>Navigate</span></span>
			<span><Kbd>Enter</Kbd><span>Activate</span></span>
			<span class="page-shortcut"><Kbd>Pg↑↓</Kbd><span>Page</span></span>
			<span class="refresh-shortcut"><Kbd>R</Kbd><span>Refresh</span></span>
		{:else}
			<span><Kbd>Esc</Kbd><span>Close</span></span>
		{/if}
	</div>
</footer>
