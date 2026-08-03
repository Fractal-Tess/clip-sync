<script lang="ts">
	import { RefreshCw, Search } from '@lucide/svelte';

	import { Button } from '$lib/components/ui/button';
	import { InputGroup, InputGroupAddon, InputGroupInput } from '$lib/components/ui/input-group';
	import { Kbd } from '$lib/components/ui/kbd';

	let {
		query = $bindable(),
		searchInput = $bindable(null),
		historyLoaded,
		total,
		rangeStart,
		rangeEnd,
		refreshing,
		onSearch,
		onKeydown
	}: {
		query: string;
		searchInput: HTMLInputElement | null;
		historyLoaded: boolean;
		total: number;
		rangeStart: number;
		rangeEnd: number;
		refreshing: boolean;
		onSearch: () => void;
		onKeydown: (event: KeyboardEvent) => void;
	} = $props();
</script>

<section class="search-deck" aria-labelledby="history-heading">
	<div class="search-heading">
		<h1 id="history-heading">Retained clipboard history</h1>
		<span>
			{historyLoaded
				? total === 0
					? '0 records'
					: `${rangeStart}–${rangeEnd} of ${total}`
				: 'Loading register'}
		</span>
	</div>
	<form
		class="search-form"
		onsubmit={(event) => {
			event.preventDefault();
			onSearch();
		}}
	>
		<InputGroup class="search-register">
			<InputGroupAddon><Search aria-hidden="true" /></InputGroupAddon>
			<InputGroupInput
				bind:ref={searchInput}
				bind:value={query}
				aria-label="Search clipboard history"
				aria-keyshortcuts="/"
				placeholder="Search content, source, or type"
				onkeydown={onKeydown}
			/>
			<InputGroupAddon align="inline-end" class="search-key"><Kbd>/</Kbd></InputGroupAddon>
		</InputGroup>
		<Button
			type="submit"
			variant="outline"
			size="sm"
			class="refresh-button"
			disabled={refreshing}
			aria-keyshortcuts="R"
		>
			<RefreshCw class={refreshing ? 'spin' : ''} aria-hidden="true" />
			<span>Refresh</span>
		</Button>
	</form>
</section>
