<script lang="ts">
	import { ChevronDown, FileType2, Monitor, RefreshCw, Search, X } from '@lucide/svelte';

	import { Button } from '$lib/components/ui/button';
	import {
		DropdownMenu,
		DropdownMenuContent,
		DropdownMenuLabel,
		DropdownMenuRadioGroup,
		DropdownMenuRadioItem,
		DropdownMenuSeparator,
		DropdownMenuTrigger
	} from '$lib/components/ui/dropdown-menu';
	import { InputGroup, InputGroupAddon, InputGroupInput } from '$lib/components/ui/input-group';
	import { Kbd } from '$lib/components/ui/kbd';

	const contentTypes = [
		{ value: 'all', label: 'All types' },
		{ value: 'text', label: 'Text' },
		{ value: 'image', label: 'Images' },
		{ value: 'file', label: 'Files' },
		{ value: 'application/', label: 'Other / binary' }
	];

	let {
		query = $bindable(),
		searchInput = $bindable(null),
		historyLoaded,
		total,
		rangeStart,
		rangeEnd,
		contentType,
		source,
		sources,
		hasFilters,
		refreshing,
		onSearch,
		onKeydown,
		onContentTypeChange,
		onSourceChange,
		onClearFilters
	}: {
		query: string;
		searchInput: HTMLInputElement | null;
		historyLoaded: boolean;
		total: number;
		rangeStart: number;
		rangeEnd: number;
		contentType: string;
		source: string;
		sources: string[];
		hasFilters: boolean;
		refreshing: boolean;
		onSearch: () => void;
		onKeydown: (event: KeyboardEvent) => void;
		onContentTypeChange: (contentType: string) => void;
		onSourceChange: (source: string) => void;
		onClearFilters: () => void;
	} = $props();

	const contentTypeLabel = $derived(
		contentTypes.find((option) => option.value === contentType)?.label ?? 'All types'
	);
	const sourceLabel = $derived(source === 'all' ? 'All sources' : source);
	const resultCount = $derived(
		historyLoaded
			? total === 0
				? '0 records'
				: `${rangeStart}–${rangeEnd} of ${total}`
			: 'Loading register'
	);
</script>

<section class="search-deck" aria-label="Clipboard history filters and search">
	<div class="filter-controls">
		<DropdownMenu>
			<DropdownMenuTrigger class="filter-trigger" aria-label={`Content type: ${contentTypeLabel}`}>
				<FileType2 aria-hidden="true" />
				<span>{contentTypeLabel}</span>
				<ChevronDown class="filter-chevron" aria-hidden="true" />
			</DropdownMenuTrigger>
			<DropdownMenuContent class="filter-menu" align="start">
				<DropdownMenuLabel>Content type</DropdownMenuLabel>
				<DropdownMenuSeparator />
				<DropdownMenuRadioGroup value={contentType}>
					{#each contentTypes as option (option.value)}
						<DropdownMenuRadioItem
							value={option.value}
							onSelect={() => onContentTypeChange(option.value)}
						>
							{option.label}
						</DropdownMenuRadioItem>
					{/each}
				</DropdownMenuRadioGroup>
			</DropdownMenuContent>
		</DropdownMenu>

		<DropdownMenu>
			<DropdownMenuTrigger
				class="filter-trigger source-filter"
				aria-label={`Source: ${sourceLabel}`}
			>
				<Monitor aria-hidden="true" />
				<span>{sourceLabel}</span>
				<ChevronDown class="filter-chevron" aria-hidden="true" />
			</DropdownMenuTrigger>
			<DropdownMenuContent class="filter-menu" align="start">
				<DropdownMenuLabel>Source device</DropdownMenuLabel>
				<DropdownMenuSeparator />
				<DropdownMenuRadioGroup value={source}>
					<DropdownMenuRadioItem value="all" onSelect={() => onSourceChange('all')}>
						All sources
					</DropdownMenuRadioItem>
					{#each sources as sourceOption (sourceOption)}
						<DropdownMenuRadioItem
							value={sourceOption}
							onSelect={() => onSourceChange(sourceOption)}
						>
							{sourceOption}
						</DropdownMenuRadioItem>
					{/each}
				</DropdownMenuRadioGroup>
			</DropdownMenuContent>
		</DropdownMenu>

		{#if hasFilters}
			<Button
				variant="ghost"
				size="icon-sm"
				class="clear-filters"
				onclick={onClearFilters}
				aria-label="Clear all filters"
				title="Clear all filters"
			>
				<X aria-hidden="true" />
			</Button>
		{/if}
		<span class="sr-only" aria-live="polite">{resultCount}</span>
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
				placeholder="Search clipboard history"
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
